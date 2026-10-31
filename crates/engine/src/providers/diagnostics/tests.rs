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
fn http_reasons_come_from_error_message_or_detail_and_stay_redacted() {
    let detail = serde_json::json!({"detail":"Unsupported parameter: 'max_output_tokens'. Bearer secret-token"});
    let reason = reason(&detail, &["secret-token".into()]).unwrap();
    assert!(reason.contains("Unsupported parameter"), "{reason}");
    assert!(reason.contains("max_output_tokens"), "{reason}");
    assert!(!reason.contains("secret-token"));
    let error = serde_json::json!({"error":{"message":"Invalid schema for function 'lookup': array schema missing items. See https://example.invalid/?key=abc"}});
    let reason = super::reason(&error, &[]).unwrap();
    assert!(reason.contains("array schema missing items"), "{reason}");
    assert!(!reason.contains("https://"));
    assert_eq!(super::reason(&serde_json::json!({}), &[]), None);
}
#[test]
fn incomplete_response_retains_its_specific_safe_reason() {
    let value = serde_json::json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}});
    assert!(stream_error(&value, &[], "response.incomplete", None).contains("max_output_tokens"));
}
