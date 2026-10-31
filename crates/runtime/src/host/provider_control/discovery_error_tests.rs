use super::*;
#[test]
fn rejected_chatgpt_access_explains_reconnect_without_rewriting_api_key_errors() {
    let config = xylo_core::ProviderConfig {
        credential_ref: Some("account:chatgpt".into()),
        ..Default::default()
    };
    let error = discovery_error(
        &config,
        anyhow::anyhow!("Provider returned HTTP 401 (request_failed)"),
    );
    assert!(error.contains("Reconnect account"));
    assert!(error.contains("401"));
    assert!(
        discovery_error(
            &config,
            anyhow::anyhow!("Provider returned HTTP 400 (invalid_grant)")
        )
        .contains("Reconnect account")
    );
    assert!(
        !discovery_error(
            &config,
            anyhow::anyhow!("Provider returned HTTP 503 (subscription_sharing_usage_unavailable)")
        )
        .contains("Reconnect account")
    );
    let api_key = xylo_core::ProviderConfig::default();
    assert!(
        !discovery_error(
            &api_key,
            anyhow::anyhow!("Provider returned HTTP 401 (request_failed)")
        )
        .contains("ChatGPT")
    );
}
