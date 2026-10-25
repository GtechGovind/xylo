//! Native language-server process supervision and bounded JSON-RPC correlation.

use super::{LspEvent, transport};
use anyhow::{Context, Result, ensure};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::Path,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, BufReader, BufWriter},
    sync::{Semaphore, mpsc, oneshot, watch},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const CAPACITY: usize = 64;
type Reply = std::result::Result<Value, String>;

#[derive(Default)]
struct Shared {
    closed: AtomicBool,
    pending: Mutex<HashMap<String, oneshot::Sender<Reply>>>,
}
impl Shared {
    fn fail(&self, message: &str) {
        self.closed.store(true, Ordering::Release);
        for (_, sender) in self.pending.lock().drain() {
            let _ = sender.send(Err(message.into()));
        }
    }
}

struct Inner {
    shared: Arc<Shared>,
    output: mpsc::Sender<Value>,
    slots: Semaphore,
    stop: CancellationToken,
    exited: watch::Receiver<bool>,
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

struct Pending {
    id: String,
    shared: Arc<Shared>,
    output: mpsc::Sender<Value>,
    completed: bool,
}
impl Drop for Pending {
    fn drop(&mut self) {
        self.shared.pending.lock().remove(&self.id);
        if !self.completed {
            let _ = self.output.try_send(
                json!({"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":self.id}}),
            );
        }
    }
}

#[derive(Clone)]
pub(super) struct Client {
    inner: Arc<Inner>,
}

impl Client {
    pub(super) async fn spawn(
        executable: &Path,
        args: &[String],
        workspace: &Path,
        events: async_channel::Sender<LspEvent>,
    ) -> Result<Self> {
        let mut command = tokio::process::Command::new(executable);
        command
            .args(args)
            .current_dir(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().context("Start code intelligence")?;
        #[cfg(unix)]
        let pid = child.id();
        let input = child
            .stdin
            .take()
            .context("Code intelligence input unavailable")?;
        let output = child
            .stdout
            .take()
            .context("Code intelligence output unavailable")?;
        let mut errors = child
            .stderr
            .take()
            .context("Code intelligence logs unavailable")?;
        let shared = Arc::new(Shared::default());
        let stop = CancellationToken::new();
        let (sender, mut incoming) = mpsc::channel::<Value>(CAPACITY);
        let (exited_tx, exited) = watch::channel(false);

        let writer_stop = stop.clone();
        let writer_shared = shared.clone();
        tokio::spawn(async move {
            let mut writer = BufWriter::new(input);
            loop {
                let message = tokio::select! {_ = writer_stop.cancelled()=>break,message=incoming.recv()=>message};
                let Some(message) = message else {
                    break;
                };
                // A queued request that lost its owner is never sent later.
                if message.get("method").is_some()
                    && let Some(id) = message.get("id").and_then(Value::as_str)
                    && !writer_shared.pending.lock().contains_key(id)
                {
                    continue;
                }
                if !matches!(
                    tokio::time::timeout(
                        Duration::from_secs(10),
                        transport::write_frame(&mut writer, &message)
                    )
                    .await,
                    Ok(Ok(()))
                ) {
                    writer_shared.fail("Code intelligence connection closed");
                    writer_stop.cancel();
                    break;
                }
            }
        });

        let reader_stop = stop.clone();
        let reader_shared = shared.clone();
        let replies = sender.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(output);
            let result=async {
                loop {
                    let message=tokio::select!{_ = reader_stop.cancelled()=>break,message=transport::read_frame(&mut reader)=>message?};
                    let Some(message)=message else{break;};
                    if let Some(method)=message.get("method").and_then(Value::as_str) {
                        if let Some(id)=message.get("id") {
                            let response=match method {
                                "workspace/configuration"=>json!({"jsonrpc":"2.0","id":id,"result":message.pointer("/params/items").and_then(Value::as_array).map(|items|items.iter().map(|_|Value::Null).collect::<Vec<_>>()).unwrap_or_default()}),
                                "workspace/workspaceFolders"=>json!({"jsonrpc":"2.0","id":id,"result":null}),
                                "window/workDoneProgress/create"|"client/registerCapability"|"client/unregisterCapability"=>json!({"jsonrpc":"2.0","id":id,"result":null}),
                                "workspace/applyEdit"=>json!({"jsonrpc":"2.0","id":id,"result":{"applied":false,"failureReason":"Workspace edits require an explicit editor action"}}),
                                _=>json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Client method is not supported"}}),
                            };
                            replies.send(response).await.context("Code intelligence response queue closed")?;
                        } else if method=="textDocument/publishDiagnostics"
                            && let Some(uri)=message.pointer("/params/uri").and_then(Value::as_str)
                            && let Ok(uri)=url::Url::parse(uri)
                            && let Ok(path)=uri.to_file_path()
                        {
                            let event=LspEvent::Diagnostics{path,version:message.pointer("/params/version").and_then(Value::as_i64).and_then(|n|i32::try_from(n).ok()),diagnostics:message.pointer("/params/diagnostics").cloned().unwrap_or_else(||json!([]))};
                            if let Err(error)=events.try_send(event){tracing::debug!(%error,"Code intelligence observation queue is full");}
                        }
                        continue;
                    }
                    if let Some(id)=message.get("id").and_then(Value::as_str) {
                        let pending=reader_shared.pending.lock().remove(id);
                        if let Some(pending)=pending {
                            let result=if let Some(error)=message.get("error") {
                                Err(error.get("message").and_then(Value::as_str).unwrap_or("Code intelligence request failed").to_owned())
                            } else {Ok(message.get("result").cloned().unwrap_or(Value::Null))};
                            let _=pending.send(result);
                        }
                    }
                }
                Ok::<_,anyhow::Error>(())
            }.await;
            if let Err(error) = result {
                tracing::warn!(%error,"Code intelligence protocol stopped");
            }
            reader_shared.fail("Code intelligence stopped. Retry the editor action to reconnect.");
            reader_stop.cancel();
        });
        let stderr_stop = stop.clone();
        tokio::spawn(async move {
            let mut bytes = [0; 4096];
            loop {
                let read = tokio::select! {_ = stderr_stop.cancelled()=>break,read=errors.read(&mut bytes)=>read};
                match read {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        tracing::debug!(message=%String::from_utf8_lossy(&bytes[..count]),"Code intelligence log")
                    }
                }
            }
        });
        let monitor_shared = shared.clone();
        let monitor_stop = stop.clone();
        tokio::spawn(async move {
            tokio::select! {_ = child.wait()=>{},_ = monitor_stop.cancelled()=>{let _=child.kill().await;let _=child.wait().await;}}
            #[cfg(unix)]
            if let Some(pid) = pid {
                // SAFETY: process_group(0) created this isolated child group.
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
            monitor_shared.fail("Code intelligence process exited");
            monitor_stop.cancel();
            let _ = exited_tx.send(true);
        });
        Ok(Self {
            inner: Arc::new(Inner {
                shared,
                output: sender,
                slots: Semaphore::new(CAPACITY),
                stop,
                exited,
            }),
        })
    }

    pub(super) fn closed(&self) -> bool {
        self.inner.shared.closed.load(Ordering::Acquire)
    }

    pub(super) async fn request(
        &self,
        id: Uuid,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<Value> {
        ensure!(!self.closed(), "Code intelligence is restarting");
        let _slot = tokio::select! {_ = cancel.cancelled()=>anyhow::bail!("Editor action cancelled"),slot=self.inner.slots.acquire()=>slot?};
        let id = id.to_string();
        let (reply, receiver) = oneshot::channel();
        {
            let mut requests = self.inner.shared.pending.lock();
            ensure!(!self.closed(), "Code intelligence is restarting");
            ensure!(
                !requests.contains_key(&id),
                "Editor action is already running"
            );
            requests.insert(id.clone(), reply);
        }
        let mut pending = Pending {
            id: id.clone(),
            shared: self.inner.shared.clone(),
            output: self.inner.output.clone(),
            completed: false,
        };
        tokio::select! {
            _ = cancel.cancelled()=>anyhow::bail!("Editor action cancelled"),
            sent=self.inner.output.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))=>sent.context("Code intelligence request queue closed")?,
        }
        let result = tokio::select! {
            _ = cancel.cancelled()=>anyhow::bail!("Editor action cancelled"),
            _ = tokio::time::sleep(Duration::from_secs(30))=>anyhow::bail!("Code intelligence took too long to respond"),
            result=receiver=>result.context("Code intelligence response lost")?.map_err(anyhow::Error::msg),
        };
        pending.completed = true;
        result
    }

    pub(super) async fn notify(&self, method: &str, params: Value) -> Result<()> {
        ensure!(!self.closed(), "Code intelligence is not connected");
        tokio::time::timeout(
            Duration::from_secs(5),
            self.inner
                .output
                .send(json!({"jsonrpc":"2.0","method":method,"params":params})),
        )
        .await
        .context("Code intelligence queue is busy")??;
        Ok(())
    }

    pub(super) async fn shutdown(&self) {
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            let _ = self
                .request(
                    Uuid::new_v4(),
                    "shutdown",
                    Value::Null,
                    CancellationToken::new(),
                )
                .await;
            let _ = self.notify("exit", Value::Null).await;
        })
        .await;
        let mut exited = self.inner.exited.clone();
        if !*exited.borrow()
            && tokio::time::timeout(Duration::from_secs(2), exited.changed())
                .await
                .is_err()
        {
            self.inner.stop.cancel();
        }
    }
}
