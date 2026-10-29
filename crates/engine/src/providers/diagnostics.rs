//! Provider errors are untrusted text, potentially reflecting input or auth.
//! Expose only bounded diagnostics; never include raw payloads or headers.
use serde_json::Value;

pub(super) fn identifier(value: &str, redactions: &[String]) -> Option<String> {
    (!value.is_empty()
        && value.len() <= 120
        && !redactions
            .iter()
            .any(|secret| !secret.is_empty() && value.contains(secret))
        && !value.starts_with("sk-")
        && !value.starts_with("eyJ")
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'[' | b']')))
    .then(|| value.to_owned())
}
fn field(value: &str) -> bool {
    [
        "input",
        "tools",
        "reasoning",
        "instructions",
        "model",
        "stream",
        "store",
        "max_output_tokens",
        "temperature",
    ]
    .iter()
    .any(|root| {
        value == *root
            || value
                .strip_prefix(root)
                .is_some_and(|tail| tail.starts_with('[') || tail.starts_with('.'))
    }) && value.len() <= 120
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'[' | b']'))
}
fn bounded(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_owned();
    }
    let end = (0..=max)
        .rev()
        .find(|end| value.is_char_boundary(*end))
        .unwrap_or(0);
    format!("{}…", value[..end].trim_end())
}
fn readable(value: &str, redactions: &[String]) -> String {
    let mut text = value.to_owned();
    for secret in redactions.iter().filter(|secret| !secret.is_empty()) {
        text = text.replace(secret, "[redacted]");
    }
    let mut quoted = String::new();
    let mut chars = text.chars().peekable();
    let mut previous = None;
    while let Some(ch) = chars.next() {
        let apostrophe = ch == '\''
            && previous.is_some_and(char::is_alphanumeric)
            && chars.peek().is_some_and(|ch| ch.is_alphanumeric());
        if matches!(ch, '\'' | '"' | '`') && !apostrophe {
            let mut inner = String::new();
            for next in chars.by_ref() {
                if next == ch {
                    break;
                }
                inner.push(next);
            }
            if field(&inner) {
                quoted.push_str(&inner);
            } else {
                quoted.push_str("[redacted]");
            }
        } else if !ch.is_control()
            && !matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200e}' | '\u{200f}')
        {
            quoted.push(ch);
        } else {
            quoted.push(' ');
        }
        previous = Some(ch);
    }
    let mut output = Vec::new();
    let mut credential_follows = false;
    for word in quoted.split_whitespace() {
        let lower = word.to_ascii_lowercase();
        let key = lower.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_');
        let credential_key = [
            "bearer",
            "basic",
            "authorization",
            "api_key",
            "apikey",
            "access_token",
            "refresh_token",
            "password",
            "secret",
        ];
        let assignment = credential_key
            .iter()
            .chain(["token", "api-key"].iter())
            .any(|key| lower.contains(&format!("{key}=")) || lower.contains(&format!("{key}:")));
        let sensitive = credential_follows
            || lower.contains("://")
            || (lower.contains('?') && lower.contains('='))
            || lower.contains('@')
            || lower.starts_with("www.")
            || lower.starts_with("sk-")
            || lower.starts_with("eyj")
            || word.len() > 120
            || assignment;
        if sensitive {
            output.push("[redacted]");
        } else {
            output.push(word);
        }
        credential_follows = credential_key.contains(&key);
        if output.iter().map(|word| word.len() + 1).sum::<usize>() > 480 {
            break;
        }
    }
    bounded(&output.join(" "), 480)
}
pub(super) fn stream_error(
    value: &Value,
    redactions: &[String],
    fallback: &str,
    transport_request_id: Option<&str>,
) -> String {
    let error = value
        .get("error")
        .or_else(|| value.pointer("/response/error"))
        .unwrap_or(value);
    let safe = |key| {
        error
            .get(key)
            .and_then(Value::as_str)
            .and_then(|value| identifier(value, redactions))
    };
    let code = safe("code")
        .or_else(|| {
            error
                .get("code")
                .and_then(Value::as_i64)
                .map(|code| code.to_string())
        })
        .or_else(|| {
            value
                .pointer("/response/incomplete_details/reason")
                .and_then(Value::as_str)
                .and_then(|reason| identifier(reason, redactions))
        });
    let kind = safe("type")
        .filter(|kind| kind != "error")
        .or_else(|| safe("status"));
    let mut parts = Vec::new();
    if let Some(code) = code {
        parts.push(format!("code={code}"));
    }
    if let Some(kind) = kind {
        parts.push(format!("type={kind}"));
    }
    if let Some(param) = safe("param").filter(|param| field(param)) {
        parts.push(format!("field={param}"));
    }
    let request = value
        .get("request_id")
        .or_else(|| error.get("request_id"))
        .or_else(|| value.pointer("/response/request_id"))
        .and_then(Value::as_str)
        .and_then(|value| identifier(value, redactions))
        .or_else(|| transport_request_id.and_then(|value| identifier(value, redactions)));
    if let Some(request) = request {
        parts.push(format!("request={request}"));
    }
    if parts.is_empty() {
        parts.push(fallback.into());
    }
    let detail = error
        .get("message")
        .and_then(Value::as_str)
        .map(|message| readable(message, redactions))
        .filter(|message| !message.is_empty());
    let detail = detail
        .map(|detail| format!(": {detail}"))
        .unwrap_or_default();
    bounded(
        &format!(
            "Provider stream error ({}){detail}. Saved progress is retained; retry after checking the reported field or provider configuration.",
            parts.join(", ")
        ),
        1000,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn safe_cause_retains_contractions_and_numeric_context_limits() {
        let detail = readable(
            "Request doesn't fit: maximum context length 272000 tokens, requested 300000. Invalid 'input[4].content'. token=private-value, provider.invalid/path?token=private-query",
            &[],
        );
        assert!(detail.contains("doesn't fit"));
        assert!(detail.contains("272000 tokens"));
        assert!(detail.contains("300000"));
        assert!(detail.contains("input[4].content"));
        assert!(!detail.contains("private-value"));
        assert!(!detail.contains("private-query"));
    }
    #[test]
    fn incomplete_response_retains_its_specific_safe_reason() {
        let value = serde_json::json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}});
        assert!(
            stream_error(&value, &[], "response.incomplete", None).contains("max_output_tokens")
        );
    }
}
