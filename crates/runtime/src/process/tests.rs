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

/// On assertion failure, owner/task handles drop first and request the trusted
/// fence. Never signal a numeric ID from a fixture after that identity retires.
struct FixtureOwner(Option<std::path::PathBuf>);
impl FixtureOwner {
    fn new(path: std::path::PathBuf) -> Self {
        Self(Some(path))
    }
    fn disarm(&mut self) {
        self.0 = None;
    }
}
impl Drop for FixtureOwner {
    fn drop(&mut self) {
        let Some(path) = &self.0 else { return };
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if let Ok(bytes) = std::fs::read(path)
                && let Ok(status) = serde_json::from_slice::<serde_json::Value>(&bytes)
                && matches!(status["state"].as_str(), Some("done" | "failed"))
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

async fn watchdog_exited(status: &serde_json::Value) {
    let watchdog = status["watchdog"].as_u64().unwrap() as u32;
    tokio::time::timeout(Duration::from_secs(1), async {
        while !live_group(watchdog).is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("watchdog did not retire after its cleanup fence");
}

fn live_group(group: u32) -> Vec<u32> {
    let output = std::process::Command::new("/bin/ps")
        .args(["-axo", "pid=,pgid=,stat="])
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse::<u32>().ok()?;
            let pgid = fields.next()?.parse::<u32>().ok()?;
            let state = fields.next()?;
            (pgid == group && !state.starts_with('Z')).then_some(pid)
        })
        .collect()
}

const FORK_BOUNDARY: &str =
    "echo ready; i=0; while [ $i -lt 16 ]; do sleep 300 & i=$((i+1)); done; wait";

#[tokio::test]
async fn guarded_sandbox_kills_descendants_when_its_owner_pipe_closes() {
    use tokio::io::{AsyncBufReadExt, BufReader};
    for iteration in 0..24 {
        let dir = tempfile::tempdir().unwrap();
        let script = if iteration % 2 == 0 {
            "echo ready; sleep 300"
        } else {
            FORK_BOUNDARY
        };
        let prepared = xylo_services::sandbox::prepare_coding(
            "/bin/sh",
            &["-c".into(), script.into()],
            dir.path(),
            &[],
            &[],
        )
        .unwrap();
        let xylo_services::sandbox::PreparedSandbox {
            mut command,
            temporary,
            status_file,
        } = prepared;
        let mut owner_cleanup = FixtureOwner::new(status_file.with_extension("cleanup"));
        command
            .env("XYLO_SANDBOX_PARENT_GUARD", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .kill_on_drop(false);
        let mut child = command.spawn().unwrap();
        let group = child.id().unwrap();
        let owner = child.stdin.take().unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(20), output.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(line.trim(), "ready", "iteration {iteration}");
        drop(owner);
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(!status.success());
        wait_for_guard_cleanup(&status_file.with_extension("cleanup"))
            .await
            .unwrap();
        let mut remaining = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), output.read_to_end(&mut remaining))
            .await
            .unwrap()
            .unwrap();
        assert!(
            live_group(group).is_empty(),
            "orphan at fork boundary {iteration}"
        );
        let guard: serde_json::Value =
            serde_json::from_slice(&std::fs::read(status_file.with_extension("cleanup")).unwrap())
                .unwrap();
        watchdog_exited(&guard).await;
        owner_cleanup.disarm();
        drop(temporary);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guarded_cancellation_and_future_drop_fence_forking_descendants() {
    for iteration in 0..12 {
        let dir = tempfile::tempdir().unwrap();
        let prepared = xylo_services::sandbox::prepare_coding(
            "/bin/sh",
            &["-c".into(), FORK_BOUNDARY.into()],
            dir.path(),
            &[],
            &[],
        )
        .unwrap();
        let status = prepared.status_file.with_extension("cleanup");
        let _temporary = prepared.temporary;
        let mut owner_cleanup = FixtureOwner::new(status.clone());
        let (ready, mut output) = tokio::sync::mpsc::unbounded_channel();
        let observer: OutputObserver = Arc::new(move |_, bytes| {
            let _ = ready.send(bytes.to_vec());
        });
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let cwd = dir.path().to_owned();
        let task = tokio::spawn(async move {
            run_prepared_observed(
                prepared.command,
                &cwd,
                Some(Duration::from_secs(20)),
                token,
                Some(observer),
                true,
            )
            .await
        });
        let _task_guard = AbortDrain(task.abort_handle());
        tokio::time::timeout(Duration::from_secs(20), async {
            while let Some(bytes) = output.recv().await {
                if bytes.windows(5).any(|part| part == b"ready") {
                    return;
                }
            }
            panic!("fixture did not start");
        })
        .await
        .unwrap();
        let guard: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&status).unwrap()).unwrap();
        let group = guard["group"].as_u64().unwrap() as u32;
        if iteration % 2 == 0 {
            cancel.cancel();
            assert!(
                tokio::time::timeout(Duration::from_secs(5), task)
                    .await
                    .unwrap()
                    .unwrap()
                    .is_err()
            );
            assert!(
                live_group(group).is_empty(),
                "cancellation returned before its fork fence"
            );
        } else {
            task.abort();
            let _ = task.await;
            tokio::time::timeout(Duration::from_secs(3), async {
                while !live_group(group).is_empty() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        }
        watchdog_exited(&guard).await;
        owner_cleanup.disarm();
    }
}

#[tokio::test]
async fn guarded_normal_exit_drains_output_and_cleans_background_descendants() {
    for _ in 0..8 {
        let dir = tempfile::tempdir().unwrap();
        let prepared = xylo_services::sandbox::prepare_coding(
            "/bin/sh",
            &["-c".into(), "sleep 300 & echo complete; exit 7".into()],
            dir.path(),
            &[],
            &[],
        )
        .unwrap();
        let status = prepared.status_file.with_extension("cleanup");
        let mut owner_cleanup = FixtureOwner::new(status.clone());
        let result = run_prepared_observed(
            prepared.command,
            dir.path(),
            Some(Duration::from_secs(20)),
            CancellationToken::new(),
            None,
            true,
        )
        .await
        .unwrap();
        let guard: serde_json::Value =
            serde_json::from_slice(&std::fs::read(status).unwrap()).unwrap();
        let group = guard["group"].as_u64().unwrap() as u32;
        assert_eq!(result.code, Some(7));
        assert_eq!(result.stdout, "complete\n");
        assert_eq!(guard["state"], "done");
        assert!(live_group(group).is_empty());
        watchdog_exited(&guard).await;
        owner_cleanup.disarm();
    }
}

#[tokio::test]
async fn managed_sandbox_approved_cwd_only_grants_the_explicit_scope() {
    let project = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let sibling = tempfile::tempdir().unwrap();
    let cwd = outside.path().canonicalize().unwrap();
    let mut prepared = xylo_services::sandbox::prepare_coding_with_scope(
        "/bin/sh",
        &[
            "-c".into(),
            format!(
                "printf approved > permitted; printf denied > '{}'",
                sibling.path().join("denied").display()
            ),
        ],
        project.path(),
        Some(&cwd),
        &[],
        xylo_services::sandbox::Network::None,
    )
    .unwrap();
    prepared.command.env("XYLO_SANDBOX_CWD", &cwd);
    let result = run_prepared_observed(
        prepared.command,
        &cwd,
        Some(Duration::from_secs(20)),
        CancellationToken::new(),
        None,
        true,
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(cwd.join("permitted")).unwrap_or_default(),
        "approved",
        "{}",
        result.stderr
    );
    assert!(!sibling.path().join("denied").exists());
    assert_ne!(result.code, Some(0));
    assert!(xylo_services::sandbox::read_start_status(&prepared.status_file).is_none());
}

#[tokio::test]
async fn managed_sandbox_refuses_an_outside_cwd_without_a_scope() {
    let project = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let cwd = outside.path().canonicalize().unwrap();
    let mut prepared = xylo_services::sandbox::prepare_coding(
        "/bin/sh",
        &["-c".into(), "touch forbidden".into()],
        project.path(),
        &[],
        &[],
    )
    .unwrap();
    prepared.command.env("XYLO_SANDBOX_CWD", &cwd);
    let _ = run_prepared_observed(
        prepared.command,
        &cwd,
        Some(Duration::from_secs(20)),
        CancellationToken::new(),
        None,
        true,
    )
    .await
    .unwrap();
    assert!(!cwd.join("forbidden").exists());
    assert!(xylo_services::sandbox::read_start_status(&prepared.status_file).is_some());
}

#[cfg(unix)]
#[tokio::test]
async fn managed_sandbox_refuses_a_replaced_approved_cwd() {
    let project = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let replaced = outside.path().join("reviewed");
    std::fs::create_dir(&replaced).unwrap();
    let cwd = replaced.canonicalize().unwrap();
    let other = tempfile::tempdir().unwrap();
    let mut prepared = xylo_services::sandbox::prepare_coding(
        "/bin/sh",
        &["-c".into(), "touch forbidden".into()],
        project.path(),
        &[],
        &[],
    )
    .unwrap();
    prepared
        .command
        .env("XYLO_SANDBOX_CWD", &cwd)
        .env("XYLO_SANDBOX_WRITE_ROOT", &cwd);
    std::fs::remove_dir(&cwd).unwrap();
    std::os::unix::fs::symlink(other.path(), &cwd).unwrap();
    let _ = run_prepared_observed(
        prepared.command,
        &cwd,
        Some(Duration::from_secs(20)),
        CancellationToken::new(),
        None,
        true,
    )
    .await
    .unwrap();
    assert!(!other.path().join("forbidden").exists());
    assert!(xylo_services::sandbox::read_start_status(&prepared.status_file).is_some());
}

#[tokio::test]
async fn guarded_start_failure_and_early_owner_loss_retire_the_watchdog() {
    for early_owner_loss in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let prepared = xylo_services::sandbox::prepare_coding(
            "/xylo-fixture-missing-program",
            &[],
            dir.path(),
            &[],
            &[],
        )
        .unwrap();
        let cleanup_status = prepared.status_file.with_extension("cleanup");
        let mut owner_cleanup = FixtureOwner::new(cleanup_status.clone());
        if early_owner_loss {
            let mut command = prepared.command;
            command
                .env("XYLO_SANDBOX_PARENT_GUARD", "1")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .kill_on_drop(false);
            let mut child = command.spawn().unwrap();
            drop(child.stdin.take());
            tokio::time::timeout(Duration::from_secs(5), child.wait())
                .await
                .unwrap()
                .unwrap();
            wait_for_guard_cleanup(&cleanup_status).await.unwrap();
        } else {
            let result = run_prepared_observed(
                prepared.command,
                dir.path(),
                Some(Duration::from_secs(10)),
                CancellationToken::new(),
                None,
                true,
            )
            .await
            .unwrap();
            assert_eq!(result.code, Some(127));
        }
        let guard: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&cleanup_status).unwrap()).unwrap();
        assert_eq!(guard["state"], "done");
        assert!(live_group(guard["group"].as_u64().unwrap() as u32).is_empty());
        watchdog_exited(&guard).await;
        owner_cleanup.disarm();
    }
}
