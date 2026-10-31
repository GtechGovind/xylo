use super::*;
/// A stand-in for a prepared sandbox: `script` runs with `$STATUS` naming
/// the launcher status file, which only a real launcher would write.
async fn sandboxed(script: &str) -> Result<SandboxedResult> {
    let dir = tempfile::tempdir().unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let status_file = temporary.path().join("status.jsonl");
    let mut command = Command::new("/bin/sh");
    command.args(["-c", script]).env("STATUS", &status_file);
    run_prepared_sandboxed(
        xylo_services::sandbox::PreparedSandbox {
            command,
            temporary,
            status_file,
        },
        dir.path(),
        Duration::from_secs(5),
        CancellationToken::new(),
    )
    .await
}
#[tokio::test]
async fn launcher_start_failures_mean_the_command_never_started() {
    for (script, reason) in [
        (
            r#"printf '{"kind":"unavailable","reason":"helper missing"}\n' > "$STATUS"; exit 125"#,
            "helper missing",
        ),
        (
            r#"printf '{"kind":"spawn_failed","reason":"no such program"}\n' > "$STATUS"; exit 127"#,
            "no such program",
        ),
    ] {
        let error = sandboxed(script).await.unwrap_err();
        let not_started = error
            .downcast_ref::<NotStarted>()
            .expect("a launcher start failure maps to NotStarted");
        assert!(not_started.0.contains(reason), "{}", not_started.0);
    }
}
#[tokio::test]
async fn a_program_cannot_forge_a_start_failure_on_stderr() {
    for (script, code) in [
        (
            "echo 'XYLO_SANDBOX_SPAWN_FAILED: x' >&2; echo 'XYLO_SANDBOX_UNAVAILABLE: y' >&2; exit 127",
            127,
        ),
        ("echo 'XYLO_SANDBOX_UNAVAILABLE: x' >&2; exit 125", 125),
        ("echo 'command not found' >&2; exit 127", 127),
    ] {
        let outcome = sandboxed(script).await.unwrap();
        assert_eq!(outcome.result.code, Some(code));
    }
}
#[tokio::test]
async fn network_denials_come_from_the_status_file() {
    let outcome = sandboxed(
        r#"printf '{"kind":"network_denied","host":"example.com:443"}\n' > "$STATUS"; exit 1"#,
    )
    .await
    .unwrap();
    assert_eq!(outcome.network_denials, vec!["example.com:443".to_owned()]);
}
#[tokio::test]
async fn managed_sandbox_allows_project_write_but_denies_outside_write() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let result = run_sandboxed(
        "/bin/sh",
        &[
            "-c".into(),
            "printf allowed > inside; printf denied > \"$1/outside\"".into(),
            "sh".into(),
            outside.path().to_string_lossy().into_owned(),
        ],
        dir.path(),
        &[],
        &[],
        Duration::from_secs(20),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("inside")).unwrap(),
        "allowed",
        "sandbox output: {}",
        result.stderr
    );
    assert!(!outside.path().join("outside").exists());
    assert_ne!(result.code, Some(0));
}
#[tokio::test]
async fn managed_sandbox_protected_subdirectory_is_unreadable() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("credentials");
    std::fs::create_dir(&secret).unwrap();
    std::fs::write(secret.join("token"), "private-fixture").unwrap();
    let result = run_sandboxed(
        "/bin/cat",
        &[secret.join("token").to_string_lossy().into_owned()],
        dir.path(),
        &[],
        &[secret],
        Duration::from_secs(20),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(!result.stdout.contains("private-fixture"));
    assert_ne!(result.code, Some(0));
}
#[tokio::test]
async fn arguments_are_literal_and_output_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let result = run(
        "printf",
        &["%s".into(), "$(do-not-execute)".into()],
        dir.path(),
        Duration::from_secs(5),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(result.stdout, "$(do-not-execute)");
    let result = run(
        "sh",
        &["-c".into(), "head -c 600000 /dev/zero".into()],
        dir.path(),
        Duration::from_secs(5),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(result.truncated);
    assert_eq!(result.stdout.len(), OUTPUT_LIMIT);
}
#[tokio::test]
async fn cancellation_terminates_child() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = run(
        "sleep",
        &["30".into()],
        dir.path(),
        Duration::from_secs(60),
        cancel,
    )
    .await;
    assert!(result.is_err());
}
