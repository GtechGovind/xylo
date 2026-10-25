use super::*;
use std::{
    collections::BTreeSet,
    path::{Component, Path},
};

pub(super) fn sensitive(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "authorization",
        "bearer ",
        "api_key",
        "api-key",
        "apikey",
        "access_token",
        "refresh_token",
        "password",
        "secret",
        "private key",
        "credentials",
        "id_rsa",
        "id_ed25519",
        "-----begin",
        "ghp_",
        "github_pat_",
        "sk-",
        "@",
        "http://",
        "https://",
        "/users/",
        "/home/",
        "/root/",
        "c:\\users\\",
    ]
    .iter()
    .any(|part| lower.contains(part))
}
pub(super) fn terms(text: &str) -> Vec<String> {
    if sensitive(text) {
        return Vec::new();
    }
    let stop = [
        "the", "and", "with", "this", "that", "from", "for", "you", "your", "please", "then",
        "use", "using", "should", "must", "only", "have", "will", "into", "after", "before",
    ];
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| (3..=24).contains(&word.len()) && word.chars().all(char::is_alphabetic))
        .map(str::to_lowercase)
        .filter(|word| !stop.contains(&word.as_str()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(24)
        .collect()
}
pub(super) fn valid_tool(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}
pub(super) fn ignored_tool(name: &str) -> bool {
    name.starts_with("skill_")
        || name.starts_with("memory_")
        || name.starts_with("account_")
        || matches!(
            name,
            "agent_delegate"
                | "search_tools"
                | "connector_list"
                | "connector_tools"
                | "models_status"
        )
}
fn relative_path(value: &str, scope: &str) -> Option<String> {
    if value.len() > 256 || value.contains('\0') {
        return None;
    }
    #[cfg(not(windows))]
    if value.contains('\\') {
        return None;
    }
    let path = Path::new(value);
    let path = if path.is_absolute() {
        path.strip_prefix(scope).ok()?
    } else {
        path
    };
    // The selected workspace may itself contain a private account directory.
    // Discard that exact prefix before examining or retaining the relative
    // evidence; unmatched absolute paths never enter the stored procedure.
    if sensitive(path.to_str()?) {
        return None;
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                let part = part.to_str()?;
                if part.starts_with('.') {
                    return None;
                }
                parts.push(part);
            }
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    })
}
fn input(key: &str) -> Value {
    json!({"$input":key})
}
/// Retain bounded literal workspace paths and harmless option values. Unrecognized
/// content, IDs, account data, auth and external paths become task-input slots.
pub(super) fn arguments(value: &Value, scope: &str) -> Value {
    let mut result = serde_json::Map::new();
    for (key, value) in value.as_object().into_iter().flatten().take(32) {
        if sensitive(key) {
            result.insert(key.clone(), input("redacted"));
            continue;
        }
        let safe = match (key.as_str(), value) {
            (
                "path" | "file" | "directory" | "root" | "source" | "destination",
                Value::String(value),
            ) => relative_path(value, scope).map_or_else(|| input(key), Value::String),
            ("limit" | "offset" | "max_count" | "timeout_seconds", Value::Number(number))
                if number.as_u64().is_some_and(|number| number <= 100_000) =>
            {
                value.clone()
            }
            (_, Value::Bool(_)) => value.clone(),
            ("query", Value::String(value))
                if matches!(value.as_str(), "TODO" | "FIXME" | "XXX") =>
            {
                Value::String(value.clone())
            }
            ("format", Value::String(value))
                if matches!(value.as_str(), "json" | "text" | "markdown") =>
            {
                Value::String(value.clone())
            }
            _ => input(key),
        };
        result.insert(key.clone(), safe);
    }
    Value::Object(result)
}
/// Cargo checks remain approval-requiring code execution. This allowlist recognizes
/// verification intent, not OS safety: build/test scripts still execute project code.
pub(super) fn verification_command(value: &Value, scope: &str) -> Option<Value> {
    if value["program"] != "cargo" {
        return None;
    }
    let args = value["args"].as_array()?;
    if args.is_empty() || args.len() > 40 {
        return None;
    }
    let subcommand = args[0].as_str()?;
    if !matches!(subcommand, "test" | "check" | "clippy" | "fmt") {
        return None;
    }
    let mut output = vec![json!(subcommand)];
    let mut expecting = None;
    let mut check_fmt = false;
    for arg in args.iter().skip(1) {
        let arg = arg.as_str()?;
        if sensitive(arg) || arg.len() > 256 {
            return None;
        }
        if let Some(option) = expecting.take() {
            let safe = if option == "--manifest-path" {
                relative_path(arg, scope)?
            } else {
                if arg.is_empty()
                    || !arg
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.,/".contains(&b))
                {
                    return None;
                }
                arg.into()
            };
            output.push(json!(safe));
            continue;
        }
        if matches!(arg, "-p" | "--package" | "--features" | "--manifest-path") {
            expecting = Some(arg);
            output.push(json!(arg));
            continue;
        }
        if arg == "--check" {
            check_fmt = true;
        }
        if !matches!(
            arg,
            "--workspace"
                | "--all"
                | "--all-targets"
                | "--all-features"
                | "--no-default-features"
                | "--offline"
                | "--locked"
                | "--frozen"
                | "--lib"
                | "--bins"
                | "--tests"
                | "--examples"
                | "--doc"
                | "--release"
                | "--no-run"
                | "--check"
                | "--"
                | "-D"
                | "warnings"
                | "--nocapture"
                | "--quiet"
        ) {
            return None;
        }
        output.push(json!(arg));
    }
    if expecting.is_some() || (subcommand == "fmt" && !check_fmt) {
        return None;
    }
    Some(json!({"program":"cargo","args":output}))
}
pub(super) fn build_steps(actions: &[JournalAction], scope: &str) -> (Vec<ProcedureStep>, bool) {
    let mut eligible = true;
    let steps=actions.iter().map(|action|{
        let known_verifier=(action.intent.name=="run_command").then(||verification_command(&action.intent.arguments,scope)).flatten();
        if (action.intent.writes || action.intent.name=="run_command") && known_verifier.is_none(){eligible=false;}
        if !valid_tool(&action.intent.name) {eligible=false;}
        ProcedureStep{tool:action.intent.name.clone(),arguments:known_verifier.unwrap_or_else(||arguments(&action.intent.arguments,scope)),requires_approval:action.intent.writes,verification:if action.intent.name=="run_command"{"Require the approved verification command to exit zero; inspect its bounded output."}else{"Require a durable successful tool result; inspect the result before continuing."}.into()}
    }).collect();
    (steps, eligible)
}
pub(super) fn result_verified(action: &JournalAction) -> bool {
    if action.state != "succeeded" {
        return false;
    }
    let Some(result) = &action.result else {
        return false;
    };
    if result.is_null() || result["success"] == false || result["isError"] == true {
        return false;
    }
    if result["status"]
        .as_str()
        .is_some_and(|status| !matches!(status, "success" | "ok" | "completed"))
    {
        return false;
    }
    if action.intent.name == "run_command"
        && result
            .pointer("/summary/code")
            .or_else(|| result.get("code"))
            .and_then(Value::as_i64)
            != Some(0)
    {
        return false;
    }
    true
}

pub(super) fn correction_command(guidance: &str, scope: &str) -> Option<Value> {
    let command = guidance
        .split('`')
        .skip(1)
        .step_by(2)
        .find(|part| part.trim().starts_with("cargo "))
        .or_else(|| {
            guidance
                .trim()
                .strip_prefix("Use ")
                .map(|text| text.trim_end_matches('.'))
                .filter(|text| text.starts_with("cargo "))
        })?;
    let words = command.split_whitespace().collect::<Vec<_>>();
    verification_command(&json!({"program":words.first()?,"args":&words[1..]}), scope)
}

#[cfg(test)]
mod path_tests {
    use super::*;

    fn workspace() -> (&'static str, &'static str) {
        if cfg!(windows) {
            (
                r"C:\Users\private-person\project",
                r"C:\Users\private-person\project\src\main.rs",
            )
        } else {
            (
                "/Users/private-person/project",
                "/Users/private-person/project/src/main.rs",
            )
        }
    }

    #[test]
    fn native_workspace_paths_retain_only_safe_relative_evidence() {
        let (scope, path) = workspace();
        assert_eq!(relative_path(path, scope).as_deref(), Some("src/main.rs"));
        assert_eq!(
            relative_path("src/./main.rs", scope).as_deref(),
            Some("src/main.rs")
        );
        assert_eq!(relative_path(scope, scope).as_deref(), Some("."));
        let retained = arguments(&json!({"path":path,"api_key":"should-not-survive"}), scope);
        assert_eq!(retained["path"], "src/main.rs");
        assert_eq!(retained["api_key"], json!({"$input":"redacted"}));
        let encoded = retained.to_string();
        assert!(!encoded.contains("private-person"));
        assert!(!encoded.contains("should-not-survive"));
    }

    #[test]
    fn relative_evidence_refuses_external_hidden_traversal_and_auth_paths() {
        let (scope, _) = workspace();
        let outside = format!("{scope}-other/src/main.rs");
        for path in [
            outside.as_str(),
            "../other/main.rs",
            "src/../main.rs",
            "src/.env",
            "src/.private/main.rs",
            "src/access_token.txt",
            "src/main.rs?api_key=should-not-survive",
            "https://example.invalid/private",
            "src\0/main.rs",
            r"src\..\..\private",
        ] {
            assert_eq!(
                arguments(&json!({"path":path}), scope),
                json!({"path":{"$input":"path"}}),
                "Unsafe evidence was retained for {path:?}"
            );
        }
        assert!(relative_path(&"x".repeat(257), scope).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn canonical_windows_namespaces_require_an_exact_workspace_prefix() {
        let scope = r"\\?\C:\Users\private-person\project";
        assert_eq!(
            relative_path(r"\\?\C:\Users\private-person\project\src\main.rs", scope).as_deref(),
            Some("src/main.rs")
        );
        assert_eq!(
            relative_path(r"src\main.rs", scope).as_deref(),
            Some("src/main.rs")
        );
        for path in [
            r"C:src\main.rs",
            r"\src\main.rs",
            r"\\?\C:\Users\private-person\project-other\src\main.rs",
            r"\\?\D:\Users\private-person\project\src\main.rs",
            r"C:\Users\private-person\project\src\main.rs",
        ] {
            assert!(relative_path(path, scope).is_none(), "Accepted {path:?}");
        }
    }
}
