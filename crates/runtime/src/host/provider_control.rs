//! Correlation and cancellation are shared with the UI-facing host client, so a
//! newer save invalidates old catalogs at enqueue time, before any vault I/O.
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub(super) struct DiscoveryControl {
    state: Mutex<(u64, CancellationToken)>,
}
impl DiscoveryControl {
    pub(super) fn begin(&self) -> u64 {
        let mut state = self.state.lock();
        state.1.cancel();
        state.0 = state.0.wrapping_add(1);
        state.1 = CancellationToken::new();
        state.0
    }
    pub(super) fn generation(&self) -> u64 {
        self.state.lock().0
    }
    pub(super) fn token(&self, generation: u64) -> Option<CancellationToken> {
        let state = self.state.lock();
        (state.0 == generation).then(|| state.1.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn next_enqueued_setup_cancels_older_work_and_invalidates_queued_catalogs() {
        let control = DiscoveryControl::default();
        let old = control.begin();
        let old_cancel = control.token(old).unwrap();
        let new = control.begin();
        assert_ne!(old, new);
        assert!(old_cancel.is_cancelled());
        assert!(control.token(old).is_none());
        assert_eq!(control.generation(), new);
        assert!(!control.token(new).unwrap().is_cancelled());
    }
}

pub(super) fn discovery_error(config: &xylo_core::ProviderConfig, error: anyhow::Error) -> String {
    let message = format!("{error:#}");
    let terminal_refresh = [
        "invalid_grant",
        "invalid_refresh_token",
        "token_expired",
        "refresh_token_expired",
        "refresh_token_invalidated",
        "refresh_token_reused",
    ]
    .iter()
    .any(|code| message.ends_with(&format!("({code})")));
    if config.credential_ref.as_deref() == Some("account:chatgpt")
        && (message.contains("HTTP 401")
            || terminal_refresh
            || message.contains("Reconnect ChatGPT"))
    {
        format!("Reconnect account to renew ChatGPT access and allow plan usage. {message}")
    } else {
        message
    }
}

#[cfg(test)]
mod discovery_error_tests {
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
                anyhow::anyhow!(
                    "Provider returned HTTP 503 (subscription_sharing_usage_unavailable)"
                )
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
}
