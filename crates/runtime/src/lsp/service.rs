//! Ordered editor synchronization with cancellable, concurrent read requests.

use super::{LspEvent, Manager};
use anyhow::{Result, bail};
use parking_lot::Mutex;
use serde_json::Value;
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub enum Command {
    Request {
        request_id: Uuid,
        workspace: PathBuf,
        path: PathBuf,
        version: i32,
        text: String,
        method: String,
        params: Value,
    },
    Change {
        workspace: PathBuf,
        path: PathBuf,
        version: i32,
        text: String,
    },
    Close(PathBuf),
    Cancel(Uuid),
    Reload,
}

struct Pending {
    path: PathBuf,
    cancel: CancellationToken,
}
type PendingMap = Arc<Mutex<HashMap<Uuid, Pending>>>;
struct Guard {
    id: Uuid,
    pending: PendingMap,
    _text_bytes: tokio::sync::OwnedSemaphorePermit,
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.pending.lock().remove(&self.id);
    }
}

struct Queued {
    command: Command,
    cancel: CancellationToken,
    guard: Option<Guard>,
}

pub struct Service {
    manager: Arc<Manager>,
    commands: async_channel::Sender<Queued>,
    pending: PendingMap,
    stop: CancellationToken,
    text_budget: Arc<tokio::sync::Semaphore>,
}

impl Service {
    /// Call inside the application's Tokio runtime; no process starts here.
    pub fn new(data: PathBuf, events: async_channel::Sender<LspEvent>) -> Self {
        let manager = Arc::new(Manager::new(data, events.clone()));
        let (commands, incoming) = async_channel::bounded::<Queued>(64);
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let stop = CancellationToken::new();
        let actor_manager = manager.clone();
        let actor_events = events.clone();
        let actor_stop = stop.clone();
        tokio::spawn(async move {
            loop {
                let queued = tokio::select! {_ = actor_stop.cancelled()=>break,queued=incoming.recv()=>queued};
                let Ok(queued) = queued else {
                    break;
                };
                let Queued {
                    command,
                    cancel,
                    guard,
                } = queued;
                match command {
                    Command::Change {
                        workspace,
                        path,
                        version,
                        text,
                    } => {
                        if let Err(error) = actor_manager
                            .change(&workspace, &path, version, &text, &cancel)
                            .await
                        {
                            tracing::debug!(%error,"Editor assistance unavailable");
                        }
                        drop(guard);
                    }
                    Command::Close(path) => {
                        let _ = actor_manager.close(&path).await;
                    }
                    Command::Reload => actor_manager.reload().await,
                    Command::Request {
                        request_id,
                        workspace,
                        path,
                        version,
                        text,
                        method,
                        params,
                    } => {
                        let synchronized = actor_manager
                            .change(&workspace, &path, version, &text, &cancel)
                            .await;
                        let manager = actor_manager.clone();
                        let events = actor_events.clone();
                        let stop = actor_stop.clone();
                        tokio::spawn(async move {
                            let _guard = guard;
                            let result = match synchronized {
                                Ok(()) => {
                                    manager
                                        .request(
                                            request_id, &workspace, &path, version, &text, &method,
                                            params, cancel,
                                        )
                                        .await
                                }
                                Err(error) => Err(error),
                            }
                            .map_err(|error| error.to_string());
                            tokio::select! {_ = stop.cancelled()=>{},_ = events.send(LspEvent::Response{request_id,result})=>{}}
                        });
                    }
                    Command::Cancel(_) => {}
                }
            }
        });
        Self {
            manager,
            commands,
            pending,
            stop,
            text_budget: Arc::new(tokio::sync::Semaphore::new(32 * 1024 * 1024)),
        }
    }

    /// Enqueue in host command order. Cancellation takes effect immediately,
    /// even while an earlier initialization request is still awaiting a server.
    pub fn enqueue(&self, command: Command) -> Result<()> {
        if let Command::Cancel(id) = command {
            if let Some(pending) = self.pending.lock().get(&id) {
                pending.cancel.cancel();
            }
            return Ok(());
        }
        if let Command::Close(path) = &command {
            for pending in self
                .pending
                .lock()
                .values()
                .filter(|pending| pending.path == *path)
            {
                pending.cancel.cancel();
            }
        }
        let id = match &command {
            Command::Request { request_id, .. } => Some(*request_id),
            Command::Change { .. } => Some(Uuid::new_v4()),
            _ => None,
        };
        let cancel = self.stop.child_token();
        let guard = if let Some(id) = id {
            let text = match &command {
                Command::Request { text, .. } | Command::Change { text, .. } => text,
                _ => unreachable!(),
            };
            anyhow::ensure!(
                text.len() <= super::MAX_TEXT,
                "Code intelligence supports files up to 4 MiB"
            );
            let text_bytes = self
                .text_budget
                .clone()
                .try_acquire_many_owned(text.len().max(1) as u32)
                .map_err(|_| anyhow::anyhow!("Editor assistance is busy; retry shortly"))?;
            let path = match &command {
                Command::Request { path, .. } | Command::Change { path, .. } => path.clone(),
                _ => unreachable!(),
            };
            let mut pending = self.pending.lock();
            if pending.len() >= 64 || pending.contains_key(&id) {
                bail!("Editor assistance is busy; retry shortly");
            }
            pending.insert(
                id,
                Pending {
                    path,
                    cancel: cancel.clone(),
                },
            );
            Some(Guard {
                id,
                pending: self.pending.clone(),
                _text_bytes: text_bytes,
            })
        } else {
            None
        };
        let queued = Queued {
            command,
            cancel,
            guard,
        };
        if self.commands.try_send(queued).is_err() {
            bail!("Editor assistance is busy; retry shortly");
        }
        Ok(())
    }

    pub async fn release_idle(&self) {
        self.manager.release_idle().await;
    }
    pub async fn reload(&self) -> Result<()> {
        for pending in self.pending.lock().values() {
            pending.cancel.cancel();
        }
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.commands.send(Queued {
                command: Command::Reload,
                cancel: self.stop.child_token(),
                guard: None,
            }),
        )
        .await
        .map_err(|_| anyhow::anyhow!("Editor assistance is still restarting"))??;
        Ok(())
    }
    pub async fn shutdown(&self) {
        self.stop.cancel();
        self.commands.close();
        self.manager.shutdown().await;
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.stop.cancel();
        self.commands.close();
    }
}
