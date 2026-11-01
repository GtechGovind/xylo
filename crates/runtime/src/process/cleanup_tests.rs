//! Injected private acknowledgment fixtures exercise ownership, not live watchdog behavior.
use super::*;
use crate::workspace::worktree::{self, InspectionInUse, retain_process_workspace};

fn repo() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    for args in [
        vec!["init", "-b", "main"],
        vec![
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "base",
        ],
    ] {
        assert!(
            std::process::Command::new("/usr/bin/git")
                .arg("-C")
                .arg(root.path())
                .args(args)
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    root
}

async fn fixture(
    root: &Path,
    ack: Option<&str>,
    script: &str,
    cancel_on_output: bool,
) -> Result<ProcessResult> {
    let control = tempfile::tempdir().unwrap();
    let ack_path = control.path().join("cleanup");
    if let Some(ack) = ack {
        std::fs::write(&ack_path, ack).unwrap();
    }
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", script])
        .env("XYLO_SANDBOX_CLEANUP_STATUS", &ack_path);
    let cancel = CancellationToken::new();
    let observer = cancel_on_output.then(|| {
        let cancel = cancel.clone();
        Arc::new(move |_: bool, _: &[u8]| cancel.cancel()) as OutputObserver
    });
    let ownership = retain_process_workspace(root).unwrap().unwrap();
    let result = run_prepared_observed(
        command,
        root,
        Some(Duration::from_secs(5)),
        cancel,
        observer,
        true,
    )
    .await;
    ownership.release_after(&result).unwrap();
    result
}

#[tokio::test]
async fn cleanup_missing_and_failed_ack_retain_checkout_after_owner_release() {
    for (name, ack) in [("missing", None), ("failed", Some(r#"{"state":"failed"}"#))] {
        let root = repo();
        let tree = worktree::create(root.path(), name, "HEAD").await.unwrap();
        let error = fixture(&tree.path, ack, "exit 0", false).await.unwrap_err();
        assert!(error.is::<CleanupUnknown>(), "{error:#}");
        for force in [false, true] {
            assert!(
                worktree::remove(&tree.path, force)
                    .await
                    .unwrap_err()
                    .is::<InspectionInUse>()
            );
        }
        assert!(tree.path.exists());
    }
}

#[tokio::test]
async fn cleanup_positive_ack_releases_nonzero_and_cancelled_commands() {
    let root = repo();
    let tree = worktree::create(root.path(), "nonzero", "HEAD")
        .await
        .unwrap();
    let result = fixture(
        &tree.path,
        Some(r#"{"state":"done","code":17}"#),
        "exit 17",
        false,
    )
    .await
    .unwrap();
    assert_eq!(result.code, Some(17));
    worktree::remove(&tree.path, false).await.unwrap();
    let tree = worktree::create(root.path(), "cancelled", "HEAD")
        .await
        .unwrap();
    let error = fixture(
        &tree.path,
        Some(r#"{"state":"done"}"#),
        "printf ready; read -r owner; exit 0",
        true,
    )
    .await
    .unwrap_err();
    assert!(!error.is::<CleanupUnknown>(), "{error:#}");
    assert!(error.to_string().contains("cancelled"), "{error:#}");
    worktree::remove(&tree.path, false).await.unwrap();
}

#[tokio::test]
async fn cleanup_known_start_failure_releases_checkout() {
    let root = repo();
    let tree = worktree::create(root.path(), "not-started", "HEAD")
        .await
        .unwrap();
    let ownership = retain_process_workspace(&tree.path).unwrap().unwrap();
    let result = run_prepared_observed(
        Command::new("/xylo-fixture-missing-executable"),
        &tree.path,
        None,
        CancellationToken::new(),
        None,
        true,
    )
    .await;
    assert!(result.as_ref().unwrap_err().is::<NotStarted>());
    ownership.release_after(&result).unwrap();
    worktree::remove(&tree.path, false).await.unwrap();
}

#[tokio::test]
async fn cleanup_success_cannot_clear_other_process_or_host_ownership() {
    let root = repo();
    let tree = worktree::create(root.path(), "owners", "HEAD")
        .await
        .unwrap();
    let first = retain_process_workspace(&tree.path).unwrap().unwrap();
    let second = retain_process_workspace(&tree.path).unwrap().unwrap();
    first.release_after(&Ok(())).unwrap();
    assert!(
        worktree::remove(&tree.path, true)
            .await
            .unwrap_err()
            .is::<InspectionInUse>()
    );
    second.release_after(&Ok(())).unwrap();
    let host = worktree::retain_inspection_workspace(&tree.path)
        .unwrap()
        .unwrap();
    fixture(
        &tree.path,
        Some(r#"{"state":"done","code":0}"#),
        "exit 0",
        false,
    )
    .await
    .unwrap();
    drop(host);
    assert!(
        worktree::remove(&tree.path, true)
            .await
            .unwrap_err()
            .is::<InspectionInUse>()
    );
}

#[test]
fn cleanup_worker_child() {
    let Some(path) = std::env::var_os("XYLO_TEST_CLEANUP_WORKSPACE") else {
        return;
    };
    let _ownership = retain_process_workspace(Path::new(&path)).unwrap().unwrap();
    // Abrupt process termination skips all destructors; the pre-dispatch marker remains.
    std::process::exit(0);
}

#[tokio::test]
async fn cleanup_worker_exit_retains_checkout_without_a_live_file_lock() {
    let root = repo();
    let tree = worktree::create(root.path(), "worker-exit", "HEAD")
        .await
        .unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "process::cleanup_tests::cleanup_worker_child",
            "--nocapture",
        ])
        .env("XYLO_TEST_CLEANUP_WORKSPACE", &tree.path)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(
        worktree::remove(&tree.path, true)
            .await
            .unwrap_err()
            .is::<InspectionInUse>()
    );
}

#[tokio::test]
async fn cleanup_private_start_failure_proves_no_dispatch_despite_missing_ack() {
    let root = repo();
    let tree = worktree::create(root.path(), "trusted-start-failure", "HEAD")
        .await
        .unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let status_file = temporary.path().join("status");
    let mut command = Command::new("/bin/sh");
    command.args(["-c", r#"printf '{"kind":"spawn_failed","reason":"fixture never dispatched"}\n' > "$STATUS"; exit 127"#])
        .env("STATUS", &status_file)
        .env("XYLO_SANDBOX_PARENT_GUARD", "1")
        .env("XYLO_SANDBOX_CLEANUP_STATUS", status_file.with_extension("cleanup"));
    let ownership = retain_process_workspace(&tree.path).unwrap().unwrap();
    let result = run_prepared_sandboxed(
        xylo_services::sandbox::PreparedSandbox {
            command,
            temporary,
            status_file,
        },
        &tree.path,
        Duration::from_secs(5),
        CancellationToken::new(),
    )
    .await;
    assert!(result.as_ref().unwrap_err().is::<NotStarted>());
    ownership.release_after(&result).unwrap();
    worktree::remove(&tree.path, false).await.unwrap();
}

#[tokio::test]
async fn cleanup_guarded_spawn_without_ack_path_is_unknown() {
    let root = tempfile::tempdir().unwrap();
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "exit 0"]);
    let error = run_prepared_observed(
        command,
        root.path(),
        None,
        CancellationToken::new(),
        None,
        true,
    )
    .await
    .unwrap_err();
    assert!(error.is::<CleanupUnknown>(), "{error:#}");
}
