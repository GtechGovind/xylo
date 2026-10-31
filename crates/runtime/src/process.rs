//! Bounded native process execution shared by Git, search, shells and diagnostics.
//!
//! Output is drained continuously to prevent child deadlock. Only a bounded
//! prefix is retained in memory. The caller receives explicit truncation and
//! must preserve outcome-unknown for interrupted side effects.
//!
//! A sandboxed command that never started (sandbox unavailable, program not
//! spawned) becomes [`NotStarted`]; that is read only from the launcher's
//! private status file, never from the program's own output.

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
            .map_err(|error| {
                // Nothing ran. Keep the sandbox's own explanation unchanged.
                let reason =
                    match error.downcast_ref::<xylo_services::sandbox::SandboxUnavailable>() {
                        Some(unavailable) => unavailable.to_string(),
                        None => format!("{error:#}"),
                    };
                anyhow::Error::new(NotStarted(reason))
            })?;
    Ok(run_prepared_sandboxed(prepared, cwd, timeout, cancel)
        .await?
        .result)
}

/// The outcome of a sandboxed command that started.
#[derive(Clone, Debug)]
pub struct SandboxedResult {
    pub result: ProcessResult,
    /// Destinations the sandbox refused, in first-attempt order.
    pub network_denials: Vec<String>,
}

/// Runs a prepared sandbox command. Whether it started is read only from the
/// launcher's private status file, which the sandboxed program cannot write;
/// its stdout, stderr and exit code are never trusted for that.
pub async fn run_prepared_sandboxed(
    prepared: xylo_services::sandbox::PreparedSandbox,
    cwd: &Path,
    timeout: Duration,
    cancel: CancellationToken,
) -> Result<SandboxedResult> {
    let xylo_services::sandbox::PreparedSandbox {
        command,
        temporary,
        status_file,
    } = prepared;
    let result = run_prepared(command, cwd, timeout, cancel).await;
    if let Some(failure) = xylo_services::sandbox::read_start_status(&status_file) {
        return Err(NotStarted(failure.to_string()).into());
    }
    let network_denials = xylo_services::sandbox::read_network_denials(&status_file);
    drop(temporary);
    Ok(SandboxedResult {
        result: result?,
        network_denials,
    })
}

/// Runs a prepared command without a sandbox, draining output and owning its
/// process group until it ends.
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
mod tests;
