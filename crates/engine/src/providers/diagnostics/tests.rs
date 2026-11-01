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

#[test]
fn thinking_replay_diagnostics_keep_public_fields_but_hide_reflected_content() {
    let value = serde_json::json!({"error":{"message":"messages.1.content.0: Invalid `signature` in `thinking` block. Set `thinking.block_binding.prefix_mismatch_behavior` to \"drop_block\" with `thinking-binding-controls-2026-08-01` in `anthropic-beta`. The `system` prompt differs. Reflected prompt: \"private user instructions\". Signature value: \"opaque-private-signature\". Bearer fixture-secret"}});
    let reason = reason(&value, &["fixture-secret".into()]).unwrap();
    for field in [
        "signature",
        "thinking.block_binding.prefix_mismatch_behavior",
        "drop_block",
        "thinking-binding-controls-2026-08-01",
        "anthropic-beta",
        "system",
    ] {
        assert!(
            reason.contains(field),
            "Missing public diagnostic field {field}: {reason}"
        );
    }
    for private in [
        "private user instructions",
        "opaque-private-signature",
        "fixture-secret",
    ] {
        assert!(
            !reason.contains(private),
            "Reflected private data must stay redacted"
        );
    }
}
