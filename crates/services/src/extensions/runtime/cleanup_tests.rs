//! Cleanup uncertainty must survive portable-component credential redaction.
use super::*;

#[tokio::test]
async fn portable_hook_missing_and_failed_ack_remain_typed_after_redaction() {
    for ack in [None, Some(r#"{"state":"failed"}"#)] {
        let temporary = tempfile::tempdir().unwrap();
        let status_file = temporary.path().join("status");
        if let Some(ack) = ack {
            std::fs::write(status_file.with_extension("cleanup"), ack).unwrap();
        }
        let mut command = tokio::process::Command::new("/bin/sh");
        command.args(["-c", "exit 0"]);
        let error = crate::hooks::run_prepared(
            crate::sandbox::PreparedSandbox {
                command,
                temporary,
                status_file,
            },
            Duration::from_secs(5),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(error.is::<crate::sandbox::CleanupUnknown>());
        let error = error.context("fixture credential secret-value");
        let redacted = redact_execution_error(error, &["secret-value".into()]);
        assert!(redacted.is::<crate::sandbox::CleanupUnknown>());
        assert!(!redacted.to_string().contains("secret-value"));
    }
}
