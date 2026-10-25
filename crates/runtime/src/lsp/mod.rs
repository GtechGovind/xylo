//! Application-owned language-server sessions. Servers start on first editor use,
//! share a bounded native transport, and are released after their documents close.
//! Project files never select executable commands; overrides live in app data.

mod client;
mod service;
mod transport;
pub use service::{Command, Service};

use anyhow::{Context, Result, bail, ensure};
use client::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const MAX_SERVERS: usize = 16;
const MAX_DOCUMENTS: usize = 64;
const MAX_TEXT: usize = 4 * 1024 * 1024;
const IDLE: Duration = Duration::from_secs(300);

#[derive(Clone, Debug)]
pub enum LspEvent {
    Diagnostics {
        path: PathBuf,
        version: Option<i32>,
        diagnostics: Value,
    },
    Response {
        request_id: Uuid,
        result: std::result::Result<Value, String>,
    },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerConfig {
    languages: Vec<String>,
    executable: PathBuf,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    initialization_options: Value,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    servers: Vec<ServerConfig>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct ServerKey {
    workspace: PathBuf,
    language: String,
}

struct Session {
    client: Client,
    last_used: tokio::time::Instant,
}
type ServerSlot = Arc<Mutex<Option<Session>>>;

struct Document {
    version: i32,
    digest: [u8; 32],
    key: ServerKey,
    client: Client,
}

pub struct Manager {
    data: PathBuf,
    events: async_channel::Sender<LspEvent>,
    servers: Mutex<HashMap<ServerKey, ServerSlot>>,
    documents: Mutex<HashMap<PathBuf, Document>>,
    stopping: CancellationToken,
}

impl Manager {
    pub fn new(data: PathBuf, events: async_channel::Sender<LspEvent>) -> Self {
        Self {
            data,
            events,
            servers: Mutex::new(HashMap::new()),
            documents: Mutex::new(HashMap::new()),
            stopping: CancellationToken::new(),
        }
    }

    async fn server(&self, key: &ServerKey, cancel: &CancellationToken) -> Result<Client> {
        ensure!(!self.stopping.is_cancelled(), "The editor is closing");
        let slot = {
            let mut servers = self.servers.lock().await;
            ensure!(
                servers.len() < MAX_SERVERS || servers.contains_key(key),
                "Close an unused project before starting another language server"
            );
            servers
                .entry(key.clone())
                .or_insert_with(|| Arc::new(Mutex::new(None)))
                .clone()
        };
        let mut session = tokio::select! { _ = cancel.cancelled()=>bail!("Editor action cancelled"), session=slot.lock()=>session };
        if let Some(session) = session.as_mut()
            && !session.client.closed()
        {
            session.last_used = tokio::time::Instant::now();
            return Ok(session.client.clone());
        }
        let data = self.data.clone();
        let language = key.language.clone();
        let config =
            tokio::task::spawn_blocking(move || resolve_server(&data, &language)).await??;
        ensure!(
            !cancel.is_cancelled() && !self.stopping.is_cancelled(),
            "Editor action cancelled"
        );
        let client = Client::spawn(
            &config.executable,
            &config.args,
            &key.workspace,
            self.events.clone(),
        )
        .await?;
        let uri = file_uri(&key.workspace)?;
        let initialize = json!({
            "processId":std::process::id(), "clientInfo":{"name":workbench_core::APPLICATION_NAME,"version":env!("CARGO_PKG_VERSION")},
            "rootUri":uri, "workspaceFolders":[{"uri":uri,"name":key.workspace.file_name().unwrap_or_default().to_string_lossy()}],
            "capabilities":{
                "general":{"positionEncodings":["utf-16"]},
                "workspace":{"configuration":false,"workspaceFolders":true,"applyEdit":false},
                "textDocument":{
                    "synchronization":{"dynamicRegistration":false,"didSave":false},
                    "completion":{"dynamicRegistration":false,"completionItem":{"snippetSupport":false,"documentationFormat":["markdown","plaintext"]}},
                    "hover":{"dynamicRegistration":false,"contentFormat":["markdown","plaintext"]},
                    "publishDiagnostics":{"versionSupport":true,"relatedInformation":true}
                }
            },
            "initializationOptions":config.initialization_options,
            "trace":"off"
        });
        let result = client
            .request(Uuid::new_v4(), "initialize", initialize, cancel.clone())
            .await;
        let initialized = match result {
            Ok(value) => value,
            Err(error) => {
                client.shutdown().await;
                return Err(error);
            }
        };
        let encoding = initialized
            .pointer("/capabilities/positionEncoding")
            .and_then(Value::as_str)
            .unwrap_or("utf-16");
        if encoding != "utf-16" {
            client.shutdown().await;
            bail!("This language server requires an unsupported position encoding");
        }
        client.notify("initialized", json!({})).await?;
        ensure!(!self.stopping.is_cancelled(), "The editor is closing");
        *session = Some(Session {
            client: client.clone(),
            last_used: tokio::time::Instant::now(),
        });
        Ok(client)
    }

    /// Synchronize the complete editor snapshot before asking the server about it.
    /// The monotonic version check also handles out-of-order host task scheduling.
    pub async fn change(
        &self,
        workspace: &Path,
        path: &Path,
        version: i32,
        text: &str,
        cancel: &CancellationToken,
    ) -> Result<()> {
        ensure!(
            !cancel.is_cancelled() && !self.stopping.is_cancelled(),
            "Editor action cancelled"
        );
        ensure!(
            text.len() <= MAX_TEXT,
            "Code intelligence supports files up to 4 MiB"
        );
        ensure!(version > 0, "Editor document version must be positive");
        let workspace = tokio::fs::canonicalize(workspace).await?;
        let path = tokio::fs::canonicalize(path).await?;
        ensure!(
            path.starts_with(&workspace),
            "Code intelligence file leaves the selected project"
        );
        let language =
            language(&path).context("No language server is configured for this file type")?;
        let key = ServerKey {
            workspace,
            language: language.into(),
        };
        let client = self.server(&key, cancel).await?;
        let digest: [u8; 32] = Sha256::digest(text.as_bytes()).into();
        let mut documents = self.documents.lock().await;
        ensure!(!cancel.is_cancelled(), "Editor action cancelled");
        if let Some(document) = documents.get(&path)
            && !document.client.closed()
        {
            ensure!(
                version >= document.version,
                "Editor result was superseded by a newer change"
            );
            if version == document.version {
                ensure!(
                    digest == document.digest,
                    "Editor version was reused for different text"
                );
                return Ok(());
            }
            client.notify("textDocument/didChange",json!({"textDocument":{"uri":file_uri(&path)?,"version":version},"contentChanges":[{"text":text}]})).await?;
        } else {
            ensure!(
                documents.len() < MAX_DOCUMENTS || documents.contains_key(&path),
                "Close an unused editor before opening more files"
            );
            client.notify("textDocument/didOpen",json!({"textDocument":{"uri":file_uri(&path)?,"languageId":language,"version":version,"text":text}})).await?;
        }
        documents.insert(
            path,
            Document {
                version,
                digest,
                key,
                client,
            },
        );
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn request(
        &self,
        id: Uuid,
        workspace: &Path,
        path: &Path,
        version: i32,
        text: &str,
        method: &str,
        mut params: Value,
        cancel: CancellationToken,
    ) -> Result<Value> {
        ensure!(
            matches!(
                method,
                "textDocument/completion"
                    | "completionItem/resolve"
                    | "textDocument/hover"
                    | "textDocument/definition"
                    | "textDocument/references"
                    | "textDocument/signatureHelp"
                    | "textDocument/documentSymbol"
            ),
            "This editor action is not supported"
        );
        self.change(workspace, path, version, text, &cancel).await?;
        let path = tokio::fs::canonicalize(path).await?;
        let client = self
            .documents
            .lock()
            .await
            .get(&path)
            .context("Editor document closed")?
            .client
            .clone();
        let object = params
            .as_object_mut()
            .context("Editor action parameters must be an object")?;
        if method != "completionItem/resolve" {
            object.insert("textDocument".into(), json!({"uri":file_uri(&path)?}));
        }
        let result = client.request(id, method, params, cancel).await?;
        ensure!(
            self.documents
                .lock()
                .await
                .get(&path)
                .is_some_and(|document| document.version == version),
            "Editor result was superseded by a newer change"
        );
        Ok(result)
    }

    pub async fn close(&self, path: &Path) -> Result<()> {
        let path = tokio::fs::canonicalize(path)
            .await
            .unwrap_or_else(|_| path.to_owned());
        let mut documents = self.documents.lock().await;
        if let Some(document) = documents.remove(&path)
            && !document.client.closed()
        {
            document
                .client
                .notify(
                    "textDocument/didClose",
                    json!({"textDocument":{"uri":file_uri(&path)?}}),
                )
                .await?;
        }
        Ok(())
    }

    /// Keep background language servers bounded without interrupting open files.
    pub async fn release_idle(&self) {
        let keys: Vec<_> = self
            .documents
            .lock()
            .await
            .values()
            .map(|document| document.key.clone())
            .collect();
        let mut servers = self.servers.lock().await;
        let mut released = Vec::new();
        servers.retain(|key, slot| {
            if keys.contains(key) {
                return true;
            }
            let Ok(mut session) = slot.try_lock() else {
                return true;
            };
            if session.as_ref().is_none_or(|session| {
                session.client.closed() || session.last_used.elapsed() >= IDLE
            }) {
                if let Some(session) = session.take() {
                    released.push(session.client);
                }
                return false;
            }
            true
        });
        drop(servers);
        futures_util::future::join_all(released.iter().map(Client::shutdown)).await;
    }

    pub async fn shutdown(&self) {
        self.stopping.cancel();
        self.reload().await;
    }

    /// Apply an explicitly activated native server without restarting the app.
    /// Editor snapshots reopen lazily on the next completion, hover or edit.
    pub async fn reload(&self) {
        let paths: Vec<_> = self
            .documents
            .lock()
            .await
            .drain()
            .map(|(path, _)| path)
            .collect();
        for path in paths {
            let _ = self.events.try_send(LspEvent::Diagnostics {
                path,
                version: None,
                diagnostics: json!([]),
            });
        }
        let slots: Vec<_> = self
            .servers
            .lock()
            .await
            .drain()
            .map(|(_, slot)| slot)
            .collect();
        futures_util::future::join_all(slots.into_iter().map(|slot| async move {
            if let Some(session) = slot.lock().await.take() {
                session.client.shutdown().await;
            }
        }))
        .await;
    }
}

fn file_uri(path: &Path) -> Result<String> {
    Ok(url::Url::from_file_path(path)
        .map_err(|_| anyhow::anyhow!("Editor path cannot be represented as a file URI"))?
        .into())
}

fn language(path: &Path) -> Option<&'static str> {
    Some(match path.extension()?.to_str()? {
        "rs" => "rust",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        "m" => "objective-c",
        "mm" => "objective-cpp",
        "py" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "go" => "go",
        "java" => "java",
        "json" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        _ => return None,
    })
}

fn resolve_server(data: &Path, language: &str) -> Result<ServerConfig> {
    let path = data.join("language-servers.json");
    if path.is_file() {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 1024 * 1024,
            "Language-server configuration is too large"
        );
        let config: Configuration =
            serde_json::from_slice(&bytes).context("Invalid language-server configuration")?;
        ensure!(
            config.servers.len() <= MAX_SERVERS,
            "Too many language-server overrides"
        );
        if let Some(mut server) = config
            .servers
            .into_iter()
            .find(|server| server.languages.iter().any(|item| item == language))
        {
            ensure!(
                server.executable.is_absolute(),
                "Choose an absolute language-server executable path"
            );
            ensure!(
                server.args.len() <= 64
                    && server.args.iter().map(String::len).sum::<usize>() <= 32768,
                "Language-server arguments exceed their limit"
            );
            server.executable = std::fs::canonicalize(&server.executable)
                .context("Language-server executable is missing")?;
            ensure!(
                server.executable.is_file(),
                "Language-server executable is not a file"
            );
            return Ok(server);
        }
    }
    let (name, args, options) = match language {
        "rust" => (
            "rust-analyzer",
            vec![],
            json!({"cargo":{"buildScripts":{"enable":false}},"procMacro":{"enable":false},"checkOnSave":false}),
        ),
        "c" | "cpp" | "objective-c" | "objective-cpp" => (
            "clangd",
            vec![
                "--background-index=false".into(),
                "--header-insertion=never".into(),
                "--limit-results=200".into(),
            ],
            Value::Null,
        ),
        _ => bail!(
            "Install a native language server for {language} in Settings to enable code intelligence"
        ),
    };
    let executable = find_native_program(name)
        .with_context(|| format!("Install {name} to enable code intelligence for this project"))?;
    Ok(ServerConfig {
        languages: vec![language.into()],
        executable,
        args,
        initialization_options: options,
    })
}

fn find_native_program(name: &str) -> Option<PathBuf> {
    let filename = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    };
    let search = std::env::var_os("PATH")?;
    std::env::split_paths(&search)
        .filter_map(|directory| std::fs::canonicalize(directory.join(&filename)).ok())
        .find(|path| {
            // Rustup shims without an installed component can recursively invoke
            // themselves; only launch a concrete language-server binary.
            path.is_file() && !path.file_stem().is_some_and(|stem| stem == "rustup")
        })
}

#[cfg(test)]
mod tests;
