//! Bounded native process execution shared by Git, search, shells and diagnostics.
//!
//! Output is drained continuously to prevent child deadlock. Only a bounded
//! prefix is retained in memory. The caller receives explicit truncation and
//! must preserve outcome-unknown for interrupted side effects.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};
use tokio_util::sync::CancellationToken;

const OUTPUT_LIMIT: usize = 512 * 1024;

/// A native command owns its descendants for its complete lifetime, including
/// when the caller drops the execution future before cancellation completes.
struct ProcessGroup {
    #[cfg(unix)]
    pid: Option<u32>,
}
impl ProcessGroup {
    fn terminate(&self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            // SAFETY: a negative PID targets the process group created below;
            // it never targets the application's own process group.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.terminate();
    }
}

struct AbortDrain(tokio::task::AbortHandle);
impl Drop for AbortDrain {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The program could not be started, so it cannot have had any side effect.
#[derive(Debug)]
pub struct NotStarted(pub String);
impl std::fmt::Display for NotStarted {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str(&self.0)
    }
}
impl std::error::Error for NotStarted {}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessResult {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}

async fn drain(mut stream: impl AsyncRead + Unpin) -> Result<(Vec<u8>, bool)> {
    let mut saved = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let count = stream.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let retain = (OUTPUT_LIMIT - saved.len()).min(count);
        saved.extend_from_slice(&buffer[..retain]);
        truncated |= retain < count;
    }
    Ok((saved, truncated))
}

/// Execute argv directly. No shell is inserted and arguments are never interpolated.
pub async fn run(
    program: &str,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
    cancel: CancellationToken,
) -> Result<ProcessResult> {
    let mut command = Command::new(program);
    command.args(args);
    run_prepared(command, cwd, timeout, cancel).await
}

pub async fn run_sandboxed(
    program: &str,
    args: &[String],
    cwd: &Path,
    read_roots: &[std::path::PathBuf],
    protected_paths: &[std::path::PathBuf],
    timeout: Duration,
    cancel: CancellationToken,
) -> Result<ProcessResult> {
    let prepared =
        xylo_services::sandbox::prepare_coding(program, args, cwd, read_roots, protected_paths)
            .map_err(|e| anyhow::Error::new(NotStarted(e.to_string())))?;
    let _temporary = prepared.temporary;
    run_prepared(prepared.command, cwd, timeout, cancel).await
}

pub async fn run_prepared(
    mut command: Command,
    cwd: &Path,
    timeout: Duration,
    cancel: CancellationToken,
) -> Result<ProcessResult> {
    if cancel.is_cancelled() {
        return Err(NotStarted("Cancelled before process start".into()).into());
    }
    // Background pipes must not create a separate Windows console.
    #[cfg(windows)]
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .map_err(|error| anyhow::Error::new(NotStarted(format!("Start process: {error}"))))?;
    let group = ProcessGroup {
        #[cfg(unix)]
        pid: child.id(),
    };
    let stdout = child.stdout.take().context("Missing stdout pipe")?;
    let stderr = child.stderr.take().context("Missing stderr pipe")?;
    let output = tokio::spawn(drain(stdout));
    let errors = tokio::spawn(drain(stderr));
    let _output_guard = AbortDrain(output.abort_handle());
    let _error_guard = AbortDrain(errors.abort_handle());
    let result = tokio::select! {
        result = child.wait() => result.context("Wait for native process"),
        _ = cancel.cancelled() => { group.terminate(); let _ = child.kill().await; Err(anyhow::anyhow!("Process cancelled; side effects may have occurred")) },
        _ = tokio::time::sleep(timeout) => { group.terminate(); let _ = child.kill().await; Err(anyhow::anyhow!("Process timed out; side effects may have occurred")) },
    };
    group.terminate();
    let _ = child.wait().await;
    // Descendant processes can retain pipes after the direct child exits. Bound cleanup too.
    let captured = tokio::time::timeout(Duration::from_secs(2), async {
        Ok::<_, anyhow::Error>((output.await??, errors.await??))
    })
    .await;
    let status = result?;
    let ((out, out_truncated), (err, err_truncated)) =
        captured.context("Process output pipes did not close")??;
    Ok(ProcessResult {
        code: status.code(),
        stdout: String::from_utf8_lossy(&out).into_owned(),
        stderr: String::from_utf8_lossy(&err).into_owned(),
        truncated: out_truncated || err_truncated,
    })
}

/// Require a successful exit while retaining the actual native error message.
pub fn require_success(result: ProcessResult) -> Result<ProcessResult> {
    if result.code != Some(0) {
        bail!("Process exited {:?}: {}", result.code, result.stderr);
    }
    Ok(result)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
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
}
