//! Small native client for the capability helper. This crate deliberately has
//! no dependency on Chromium, document engines, Wasmtime, or model runtimes.

pub mod protocol;

use anyhow::{Context, Result, ensure};
use protocol::{Command, Outcome, PROTOCOL_VERSION, QUEUE_CAPACITY, Request, Response};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::Path,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{BufReader, BufWriter},
    sync::{Semaphore, mpsc, oneshot, watch},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

type Reply = std::result::Result<Value, String>;

#[derive(Default)]
struct Shared {
    closed: AtomicBool,
    background_jobs: AtomicUsize,
    pending: Mutex<HashMap<Uuid, oneshot::Sender<Reply>>>,
}

impl Shared {
    fn fail(&self, reason: &str) {
        self.closed.store(true, Ordering::Release);
        self.background_jobs.store(0, Ordering::Release);
        if let Ok(mut pending) = self.pending.lock() {
            for (_, reply) in pending.drain() {
                let _ = reply.send(Err(reason.to_owned()));
            }
        }
    }
}

struct Inner {
    shared: Arc<Shared>,
    requests: mpsc::Sender<Request>,
    control: mpsc::Sender<Request>,
    slots: Semaphore,
    control_slots: Semaphore,
    stop: CancellationToken,
    exited: watch::Receiver<bool>,
    pid: u32,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Prefer normal service cleanup even if the owner forgot shutdown().
        let _ = self.control.try_send(Request::new(Command::Shutdown));
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let stop = self.stop.clone();
            runtime.spawn(async move {
                tokio::time::sleep(Duration::from_secs(8)).await;
                stop.cancel();
            });
        } else {
            self.stop.cancel();
        }
    }
}

struct PendingGuard {
    shared: Arc<Shared>,
    id: Uuid,
    cancel_on_drop: Option<mpsc::Sender<Request>>,
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Some(control) = &self.cancel_on_drop {
            let _ = control.try_send(Request::new(Command::Cancel {
                request_id: self.id,
            }));
        }
        if let Ok(mut pending) = self.shared.pending.lock() {
            pending.remove(&self.id);
        }
    }
}

/// Clones share one supervised helper and its bounded queues. A helper is tied
/// to one canonical workspace; create another client to change workspace.
#[derive(Clone)]
pub struct HelperClient {
    inner: Arc<Inner>,
}

impl HelperClient {
    pub async fn spawn(
        executable: impl AsRef<Path>,
        data_dir: impl AsRef<Path>,
        workspace: impl AsRef<Path>,
    ) -> Result<Self> {
        let executable = executable.as_ref();
        ensure!(
            executable.is_absolute(),
            "Helper executable must be absolute"
        );
        let executable =
            std::fs::canonicalize(executable).context("Locate native helper executable")?;
        ensure!(executable.is_file(), "Helper executable is not a file");
        ensure!(
            data_dir.as_ref().is_absolute(),
            "Helper data directory must be absolute"
        );
        tokio::fs::create_dir_all(data_dir.as_ref()).await?;
        let data_dir = tokio::fs::canonicalize(data_dir.as_ref()).await?;
        let workspace = tokio::fs::canonicalize(workspace.as_ref())
            .await
            .context("Locate helper workspace")?;
        ensure!(workspace.is_dir(), "Helper workspace must be a directory");
        let mut process = tokio::process::Command::new(&executable);
        // Background pipes must not create a separate Windows console.
        #[cfg(windows)]
        process.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        process
            .arg("--data-dir")
            .arg(data_dir)
            .arg("--workspace")
            .arg(&workspace)
            .current_dir(&workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = process.spawn().context("Start native capability helper")?;
        let pid = child.id().context("Helper did not expose a process ID")?;
        let stdin = child.stdin.take().context("Helper stdin unavailable")?;
        let stdout = child.stdout.take().context("Helper stdout unavailable")?;
        let shared = Arc::new(Shared::default());
        let stop = CancellationToken::new();
        let (requests, mut incoming) = mpsc::channel::<Request>(QUEUE_CAPACITY);
        let (control, mut controls) = mpsc::channel::<Request>(QUEUE_CAPACITY);
        let (exited_tx, exited) = watch::channel(false);

        let writer_stop = stop.clone();
        let writer_shared = shared.clone();
        tokio::spawn(async move {
            let mut writer = BufWriter::new(stdin);
            loop {
                let request = tokio::select! {
                    biased;
                    _ = writer_stop.cancelled() => break,
                    message = controls.recv() => message,
                    message = incoming.recv() => message,
                };
                let Some(request) = request else {
                    break;
                };
                let result = tokio::time::timeout(
                    Duration::from_secs(10),
                    protocol::write_frame(&mut writer, &request),
                )
                .await;
                if !matches!(result, Ok(Ok(()))) {
                    writer_shared.fail("Native helper request pipe failed or timed out");
                    writer_stop.cancel();
                    break;
                }
            }
        });

        let reader_stop = stop.clone();
        let reader_shared = shared.clone();
        tokio::spawn(async move {
            let read = async {
                let mut reader = BufReader::new(stdout);
                while let Some(frame) = protocol::read_frame(&mut reader).await? {
                    let response: Response =
                        serde_json::from_slice(&frame).context("Invalid helper response")?;
                    ensure!(
                        response.protocol_version == PROTOCOL_VERSION,
                        "Unsupported helper protocol version"
                    );
                    reader_shared
                        .background_jobs
                        .store(response.background_jobs.min(64), Ordering::Release);
                    let result = match response.outcome {
                        Outcome::Completed { value } => Ok(value),
                        Outcome::Failed { code, message } => Err(format!("{code}: {message}")),
                    };
                    if let Some(reply) = reader_shared
                        .pending
                        .lock()
                        .map_err(|_| anyhow::anyhow!("Helper response state poisoned"))?
                        .remove(&response.id)
                    {
                        let _ = reply.send(result);
                    }
                }
                Ok::<_, anyhow::Error>(())
            };
            let reason = tokio::select! {
                result = read => match result { Ok(()) => "Native helper closed its response pipe".into(), Err(error) => format!("Native helper protocol failed: {error:#}") },
                _ = reader_stop.cancelled() => "Native helper stopped".into(),
            };
            reader_shared.fail(&reason);
            reader_stop.cancel();
        });

        let monitor_stop = stop.clone();
        let monitor_shared = shared.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = child.wait() => (),
                _ = monitor_stop.cancelled() => { let _ = child.kill().await; let _ = child.wait().await; },
            }
            monitor_shared.fail("Native helper exited");
            monitor_stop.cancel();
            let _ = exited_tx.send(true);
        });

        let client = Self {
            inner: Arc::new(Inner {
                shared,
                requests,
                control,
                slots: Semaphore::new(QUEUE_CAPACITY),
                control_slots: Semaphore::new(8),
                stop,
                exited,
                pid,
            }),
        };
        let version = tokio::time::timeout(
            Duration::from_secs(10),
            client.execute("helper_version", &json!({}), CancellationToken::new()),
        )
        .await
        .context("Native helper handshake timed out")??;
        ensure!(
            version.get("protocol_version").and_then(Value::as_u64)
                == Some(u64::from(PROTOCOL_VERSION)),
            "Native helper handshake version mismatch"
        );
        Ok(client)
    }

    pub fn pid(&self) -> u32 {
        self.inner.pid
    }
    pub fn is_closed(&self) -> bool {
        self.inner.shared.closed.load(Ordering::Acquire)
    }
    /// Active calls plus background asset jobs, for host idle eviction.
    pub fn pending_count(&self) -> usize {
        self.inner
            .shared
            .pending
            .lock()
            .map(|pending| pending.len())
            .unwrap_or(QUEUE_CAPACITY)
            .saturating_add(self.inner.shared.background_jobs.load(Ordering::Acquire))
    }

    pub async fn execute(
        &self,
        name: &str,
        args: &Value,
        cancel: CancellationToken,
    ) -> Result<Value> {
        ensure!(
            !name.is_empty()
                && name.len() <= 128
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "Invalid native helper command name"
        );
        let priority = matches!(
            name,
            "helper_audio_record_stop" | "helper_audio_record_cancel" | "helper_audio_play_stop"
        );
        self.call(
            Command::Execute {
                name: name.to_owned(),
                args: args.clone(),
            },
            cancel,
            priority,
        )
        .await
    }

    async fn call(
        &self,
        command: Command,
        cancel: CancellationToken,
        priority: bool,
    ) -> Result<Value> {
        ensure!(!cancel.is_cancelled(), "Native helper request cancelled");
        ensure!(!self.is_closed(), "Native helper is closed");
        let _slot = tokio::select! {
            slot = async {if priority{self.inner.control_slots.acquire().await}else{self.inner.slots.acquire().await}} => slot?,
            _ = cancel.cancelled() => anyhow::bail!("Native helper request cancelled"),
            _ = self.inner.stop.cancelled() => anyhow::bail!("Native helper is closed"),
        };
        let request = Request::new(command);
        protocol::encode(&request)?;
        let id = request.id;
        let (reply, response) = oneshot::channel();
        self.inner
            .shared
            .pending
            .lock()
            .map_err(|_| anyhow::anyhow!("Helper request state poisoned"))?
            .insert(id, reply);
        let mut pending = PendingGuard {
            shared: self.inner.shared.clone(),
            id,
            cancel_on_drop: None,
        };
        let queue = if priority {
            &self.inner.control
        } else {
            &self.inner.requests
        };
        tokio::select! {
            result = queue.send(request) => result.context("Native helper request queue closed")?,
            _ = cancel.cancelled() => anyhow::bail!("Native helper request cancelled before dispatch"),
            _ = self.inner.stop.cancelled() => anyhow::bail!("Native helper is closed"),
        }
        pending.cancel_on_drop = Some(self.inner.control.clone());
        let (result, needs_cancel) = tokio::select! {
            result = response => (result.context("Native helper lost its response")?.map_err(anyhow::Error::msg),false),
            _ = cancel.cancelled() => (Err(anyhow::anyhow!("Native helper request cancelled")),true),
            _ = tokio::time::sleep(Duration::from_secs(3605)) => (Err(anyhow::anyhow!("Native helper request deadline exceeded")),true),
        };
        if needs_cancel {
            // Independent priority channel: cancellation cannot sit behind a
            // full queue of new tool calls. It requests cleanup, never replay.
            let message = Request::new(Command::Cancel { request_id: id });
            if matches!(
                tokio::time::timeout(Duration::from_secs(1), self.inner.control.send(message))
                    .await,
                Ok(Ok(()))
            ) {
                pending.cancel_on_drop = None;
            }
        } else {
            pending.cancel_on_drop = None;
        }
        result
    }

    pub async fn shutdown(&self) -> Result<()> {
        if !self.is_closed() {
            // Shutdown does not compete for an execution slot: all 64 slots
            // may be occupied by the work it needs to cancel.
            let request = Request::new(Command::Shutdown);
            let id = request.id;
            let (reply, response) = oneshot::channel();
            self.inner
                .shared
                .pending
                .lock()
                .map_err(|_| anyhow::anyhow!("Helper request state poisoned"))?
                .insert(id, reply);
            let _pending = PendingGuard {
                shared: self.inner.shared.clone(),
                id,
                cancel_on_drop: None,
            };
            let _ = tokio::time::timeout(Duration::from_secs(5), async {
                self.inner.control.send(request).await?;
                response.await?.map_err(anyhow::Error::msg)?;
                Ok::<_, anyhow::Error>(())
            })
            .await;
        }
        let mut exited = self.inner.exited.clone();
        let graceful = async {
            while !*exited.borrow_and_update() {
                exited.changed().await?;
            }
            Ok::<_, anyhow::Error>(())
        };
        if tokio::time::timeout(Duration::from_secs(5), graceful)
            .await
            .is_err()
        {
            self.inner.stop.cancel();
        }
        let mut exited = self.inner.exited.clone();
        if !*exited.borrow() {
            let _ = tokio::time::timeout(Duration::from_secs(2), exited.changed()).await;
        }
        ensure!(
            *exited.borrow(),
            "Native helper did not exit after shutdown"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn audio_stop_bypasses_full_normal_execution_slots() {
        let shared = Arc::new(Shared::default());
        let (requests, _incoming) = mpsc::channel(QUEUE_CAPACITY);
        let (control, mut controls) = mpsc::channel(QUEUE_CAPACITY);
        let (_exited_tx, exited) = watch::channel(true);
        let client = HelperClient {
            inner: Arc::new(Inner {
                shared: shared.clone(),
                requests,
                control,
                slots: Semaphore::new(QUEUE_CAPACITY),
                control_slots: Semaphore::new(8),
                stop: CancellationToken::new(),
                exited,
                pid: 0,
            }),
        };
        let _all_slots = client
            .inner
            .slots
            .acquire_many(QUEUE_CAPACITY as u32)
            .await
            .unwrap();
        let peer = tokio::spawn(async move {
            let message = controls.recv().await.unwrap();
            assert!(
                matches!(message.command,Command::Execute{name,..} if name=="helper_audio_record_cancel")
            );
            shared
                .pending
                .lock()
                .unwrap()
                .remove(&message.id)
                .unwrap()
                .send(Ok(json!({"state":"cancelled"})))
                .unwrap();
        });
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            client.execute(
                "helper_audio_record_cancel",
                &json!({}),
                CancellationToken::new(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result["state"], "cancelled");
        peer.await.unwrap();
    }
}
