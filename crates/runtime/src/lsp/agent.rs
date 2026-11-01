//! Awaitable language intelligence for agent tools, sharing the native LSP transport.

use super::{LspEvent, Manager, file_uri};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

struct State {
    versions: HashMap<PathBuf, i32>,
    next_version: i32,
}
/// Starts servers lazily and synchronizes disk content before every request.
/// Callers must authorize native execution with inherited file and network access;
/// this transport does not sandbox language servers or decide agent permissions.
pub struct AgentService {
    manager: Manager,
    events: async_channel::Receiver<LspEvent>,
    state: Mutex<State>,
}
impl AgentService {
    /// Retains a checkout while a server or an in-flight operation may own it.
    /// Merely constructing this lazy service does not start a process.
    pub fn may_have_processes(&self, workspace: &Path) -> bool {
        if self.manager.stopping.is_cancelled() || self.state.try_lock().is_err() {
            return true;
        }
        let Ok(servers) = self.manager.servers.try_lock() else {
            return true;
        };
        servers.iter().any(|(key, slot)| {
            key.workspace == workspace
                && match slot.try_lock() {
                    Ok(session) => session.is_some(),
                    Err(_) => true,
                }
        })
    }

    /// Uses application-owned language-server configuration from `data`.
    pub fn new(data: PathBuf) -> Self {
        let (events, receiver) = async_channel::bounded(256);
        Self {
            manager: Manager::new(data, events),
            events: receiver,
            state: Mutex::new(State {
                versions: HashMap::new(),
                next_version: 0,
            }),
        }
    }
    /// Requests one supported operation. Positions use zero-based UTF-16 offsets.
    pub async fn request(
        &self,
        workspace: &Path,
        path: &Path,
        operation: &str,
        line: u64,
        character: u64,
        cancel: CancellationToken,
    ) -> Result<Value> {
        ensure!(
            matches!(
                operation,
                "definition"
                    | "references"
                    | "hover"
                    | "symbols"
                    | "workspace_symbols"
                    | "diagnostics"
            ),
            "Unknown LSP operation"
        );
        ensure!(!cancel.is_cancelled(), "Code intelligence cancelled");
        let path = tokio::fs::canonicalize(path).await?;
        use tokio::io::AsyncReadExt;
        let file = tokio::fs::File::open(&path).await?;
        ensure!(
            file.metadata().await?.len() <= super::MAX_TEXT as u64,
            "Code intelligence supports files up to 4 MiB"
        );
        let mut text = String::new();
        file.take(super::MAX_TEXT as u64 + 1)
            .read_to_string(&mut text)
            .await?;
        ensure!(
            text.len() <= super::MAX_TEXT,
            "Code intelligence supports files up to 4 MiB"
        );
        let mut state = tokio::select! {_ = cancel.cancelled()=>bail!("Code intelligence cancelled"),state=self.state.lock()=>state};
        // Agent requests are serialized. Keep only the current disk document,
        // so a long conversation cannot exhaust editor document slots.
        let old: Vec<_> = state
            .versions
            .keys()
            .filter(|old| **old != path)
            .cloned()
            .collect();
        for old in old {
            let _ = self.manager.close(&old).await;
            state.versions.remove(&old);
        }
        self.manager.release_idle().await;
        while self.events.try_recv().is_ok() {}
        // Versions never repeat when a file closes and later reopens, so late
        // diagnostics from its previous lifetime cannot describe a new edit.
        let version = state
            .next_version
            .checked_add(1)
            .context("LSP version exhausted")?;
        self.manager
            .change(workspace, &path, version, &text, &cancel)
            .await?;
        state.next_version = version;
        state.versions.insert(path.clone(), version);
        if operation == "diagnostics" {
            let mut versionless = false;
            let wait = async {
                loop {
                    let event = tokio::select! {_ = cancel.cancelled()=>bail!("Code intelligence cancelled"), event=self.events.recv()=>event?};
                    if let LspEvent::Diagnostics {
                        path: reported,
                        version: reported_version,
                        diagnostics,
                    } = event
                        && reported == path
                    {
                        match reported_version {
                            Some(reported) if reported == version => {
                                return Ok::<_, anyhow::Error>(
                                    json!({"path":path,"diagnostics":diagnostics,"timed_out":false,"freshness":"versioned","version":version}),
                                );
                            }
                            None => versionless = true,
                            _ => {}
                        }
                    }
                }
            };
            return match tokio::time::timeout(Duration::from_millis(1500), wait).await {
                Ok(value) => value,
                Err(_) => Ok(
                    json!({"path":path,"diagnostics":null,"timed_out":true,"freshness":"unavailable","unavailable_reason":if versionless {"The language server did not correlate its diagnostics with the current document revision"}else{"No current-revision diagnostics were received within 1.5 seconds"}}),
                ),
            };
        }
        let method = match operation {
            "definition" => "textDocument/definition",
            "references" => "textDocument/references",
            "hover" => "textDocument/hover",
            "symbols" => "textDocument/documentSymbol",
            "workspace_symbols" => "workspace/symbol",
            _ => unreachable!(),
        };
        let mut params = json!({"position":{"line":line,"character":character}});
        if operation == "references" {
            params["context"] = json!({"includeDeclaration":true});
        }
        if operation == "workspace_symbols" {
            // The symbol under the supplied position is the workspace query.
            let row = text.lines().nth(line as usize).unwrap_or("");
            let mut offset = 0;
            let mut query = String::new();
            for part in row.split_inclusive(|c: char| !c.is_alphanumeric() && c != '_') {
                let end = offset + part.encode_utf16().count();
                if (character as usize) >= offset && (character as usize) < end {
                    query = part
                        .trim_end_matches(|c: char| !c.is_alphanumeric() && c != '_')
                        .to_owned();
                    break;
                }
                offset = end;
            }
            params = json!({"query":query});
            let client = self
                .manager
                .documents
                .lock()
                .await
                .get(&path)
                .context("LSP document closed")?
                .client
                .clone();
            return client.request(Uuid::new_v4(), method, params, cancel).await;
        }
        params["textDocument"] = json!({"uri":file_uri(&path)?});
        self.manager
            .request(
                Uuid::new_v4(),
                workspace,
                &path,
                version,
                &text,
                method,
                params,
                cancel,
            )
            .await
    }
    /// Stops the language servers started for agent requests.
    pub async fn shutdown(&self) {
        self.manager.shutdown().await;
    }
}

#[cfg(test)]
mod tests;
