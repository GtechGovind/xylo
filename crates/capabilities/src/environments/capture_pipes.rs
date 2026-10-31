//! Small async pipe drains feed bounded native file writers. The real blocking
//! writers own the execution leases even if every async caller disappears.
use super::*;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    sync::mpsc,
    task::JoinHandle,
};

pub(super) enum Status {
    Exited { success: bool, code: Option<i32> },
    NotStarted(String),
    Interrupted(String, EnvironmentCaptureReason),
}
pub(super) struct Progress {
    pub observed: u64,
    pub reason: EnvironmentCaptureReason,
    pub observed_exact: bool,
    preview: Vec<u8>,
}
pub(super) struct ResultData {
    pub status: Status,
    pub stdout: Progress,
    pub stderr: Progress,
    pub stdout_preview: Vec<u8>,
    pub stderr_preview: Vec<u8>,
    pub truncated: bool,
}
struct Reader {
    state: Arc<Mutex<Progress>>,
    task: JoinHandle<()>,
    writer: JoinHandle<bool>,
}

impl Reader {
    fn spawn(
        pipe: Option<impl AsyncRead + Unpin + Send + 'static>,
        mut file: File,
        preview_limit: usize,
        lease: Arc<RunLease>,
    ) -> Self {
        let (send, mut receive) = mpsc::channel::<Vec<u8>>(4);
        let writer_lease = lease.clone();
        let writer = xylo_resources::spawn_blocking(move || {
            let _lease = writer_lease;
            let mut failed = false;
            while let Some(bytes) = receive.blocking_recv() {
                if failed {
                    continue;
                }
                let mut remaining = bytes.as_slice();
                while !remaining.is_empty() {
                    match file.write(remaining) {
                        Ok(0) | Err(_) => {
                            failed = true;
                            break;
                        }
                        Ok(count) => remaining = &remaining[count..],
                    }
                }
            }
            // This can be uninterruptible kernel IO. The owned lease and disk
            // reservation survive until it actually returns; we never free
            // accounting merely because an async timeout stopped waiting.
            failed |= file.sync_all().is_err();
            failed
        });
        let state = Arc::new(Mutex::new(Progress {
            observed: 0,
            observed_exact: false,
            reason: EnvironmentCaptureReason::Interrupted,
            preview: Vec::with_capacity(preview_limit.min(8192)),
        }));
        let progress = state.clone();
        let task = tokio::spawn(async move {
            let _lease = lease;
            let Some(mut pipe) = pipe else {
                if let Ok(mut state) = progress.lock() {
                    state.reason = EnvironmentCaptureReason::ReadError;
                }
                return;
            };
            let mut retained = 0_u64;
            let mut bytes = [0_u8; 16 * 1024];
            loop {
                let count = match pipe.read(&mut bytes).await {
                    Ok(0) => {
                        if let Ok(mut state) = progress.lock() {
                            state.reason = EnvironmentCaptureReason::Eof;
                            state.observed_exact = true;
                        }
                        break;
                    }
                    Ok(count) => count,
                    Err(_) => {
                        if let Ok(mut state) = progress.lock() {
                            state.reason = EnvironmentCaptureReason::ReadError;
                        }
                        break;
                    }
                };
                if let Ok(mut state) = progress.lock() {
                    state.observed = state.observed.saturating_add(count as u64);
                    let preview = preview_limit.saturating_sub(state.preview.len()).min(count);
                    state.preview.extend_from_slice(&bytes[..preview]);
                }
                let keep = (STREAM_LIMIT.saturating_sub(retained)).min(count as u64) as usize;
                if keep > 0 {
                    retained += keep as u64;
                    // After the retained byte ceiling we still drain both
                    // pipes without allocating or queuing discarded chunks.
                    if send.send(bytes[..keep].to_vec()).await.is_err() {
                        if let Ok(mut state) = progress.lock() {
                            state.reason = EnvironmentCaptureReason::WriteError;
                        }
                        break;
                    }
                }
            }
        });
        Self {
            state,
            task,
            writer,
        }
    }

    async fn finish(self, interruption: Option<EnvironmentCaptureReason>) -> Progress {
        self.finish_with_timeout(interruption, Duration::from_secs(2))
            .await
    }

    async fn finish_with_timeout(
        mut self,
        interruption: Option<EnvironmentCaptureReason>,
        drain_timeout: Duration,
    ) -> Progress {
        let reader_failed = match tokio::time::timeout(drain_timeout, &mut self.task).await {
            Ok(result) => result.is_err(),
            Err(_) => {
                self.task.abort();
                let _ = self.task.await;
                false
            }
        };
        // Retain the real writer and its leases through completion/fsync, even
        // after the bounded async drain stops waiting for an open pipe.
        let failed = self.writer.await.unwrap_or(true);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if failed {
            state.reason = EnvironmentCaptureReason::WriteError;
        } else if reader_failed {
            state.reason = EnvironmentCaptureReason::ReadError;
        } else if state.observed > STREAM_LIMIT {
            state.reason = EnvironmentCaptureReason::ByteLimit;
        } else if let Some(reason) = interruption
            && matches!(
                state.reason,
                EnvironmentCaptureReason::Eof | EnvironmentCaptureReason::Interrupted
            )
        {
            // Command end cause and capture completeness are independent. An
            // aborted drain keeps observed_exact=false, while retaining a known
            // cancellation/timeout instead of replacing it with a generic cause.
            state.reason = reason;
        }
        Progress {
            observed: state.observed,
            observed_exact: state.observed_exact,
            reason: state.reason,
            preview: std::mem::take(&mut state.preview),
        }
    }
}

fn no_process(status: Status) -> ResultData {
    let reason = match &status {
        Status::NotStarted(_) => EnvironmentCaptureReason::Cancelled,
        _ => EnvironmentCaptureReason::Interrupted,
    };
    let observed_exact = matches!(status, Status::NotStarted(_));
    ResultData {
        status,
        stdout: Progress {
            observed: 0,
            observed_exact,
            reason,
            preview: Vec::new(),
        },
        stderr: Progress {
            observed: 0,
            observed_exact,
            reason,
            preview: Vec::new(),
        },
        stdout_preview: Vec::new(),
        stderr_preview: Vec::new(),
        truncated: true,
    }
}

pub(super) async fn run(
    spec: process::ProcessSpec,
    preview_limit: usize,
    stdout: File,
    stderr: File,
    cancel: CancellationToken,
    lease: Arc<RunLease>,
) -> ResultData {
    if cancel.is_cancelled() {
        return no_process(Status::NotStarted(
            "Command cancelled before native dispatch.".into(),
        ));
    }
    let mut child = match process::ManagedProcess::spawn_uncaptured(&spec) {
        Ok(child) => child,
        // Native supervision setup can fail after a Windows child was spawned.
        // Do not mislabel this generic error as proof of non-dispatch.
        Err(error) => {
            return no_process(Status::Interrupted(
                format!("Native client could not establish supervised execution: {error}"),
                EnvironmentCaptureReason::Interrupted,
            ));
        }
    };
    let stdout = Reader::spawn(child.take_stdout(), stdout, preview_limit, lease.clone());
    let stderr = Reader::spawn(child.take_stderr(), stderr, preview_limit, lease);
    let status = tokio::select! {
        status = child.wait() => {
            child.finish_tree();
            match status {
                Ok(status) => Status::Exited { success: status.success(), code: status.code() },
                Err(error) => Status::Interrupted(format!("Native client exit could not be confirmed: {error}"), EnvironmentCaptureReason::Interrupted),
            }
        },
        _ = cancel.cancelled() => {
            let _ = child.stop().await;
            Status::Interrupted("Native client cancelled".into(), EnvironmentCaptureReason::Cancelled)
        },
        _ = tokio::time::sleep(spec.timeout) => {
            let _ = child.stop().await;
            Status::Interrupted("Native client timed out".into(), EnvironmentCaptureReason::TimedOut)
        },
    };
    let interruption = match &status {
        Status::Interrupted(_, reason) => Some(*reason),
        _ => None,
    };
    let (mut stdout, mut stderr) =
        tokio::join!(stdout.finish(interruption), stderr.finish(interruption));
    let stdout_preview = std::mem::take(&mut stdout.preview);
    let stderr_preview = std::mem::take(&mut stderr.preview);
    let truncated = stdout.observed > stdout_preview.len() as u64
        || stderr.observed > stderr_preview.len() as u64
        || stdout.reason != EnvironmentCaptureReason::Eof
        || stderr.reason != EnvironmentCaptureReason::Eof;
    ResultData {
        status,
        stdout,
        stderr,
        stdout_preview,
        stderr_preview,
        truncated,
    }
}

#[cfg(test)]
#[path = "capture_pipes/tests.rs"]
mod tests;
