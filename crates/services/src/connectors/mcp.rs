use crate::{CredentialVault, Store};
use anyhow::{Context, Result, bail, ensure};
use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, PaginatedRequestParams},
    service::RunningService,
    transport::{
        StreamableHttpClientTransport, TokioChildProcess,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    process::Command,
    sync::{Mutex, Semaphore},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use xylo_core::{ConnectorConfig, ToolDefinition};

type Client = RunningService<RoleClient, ()>;
struct Session {
    client: Option<Client>,
    fingerprint: String,
    touched: Instant,
}
/// A single connection per connector with bounded aggregate concurrency. Construction
/// starts no processes and does no network or credential-store work.
pub struct ConnectorBroker {
    store: Store,
    vault: CredentialVault,
    sessions: Mutex<HashMap<Uuid, Arc<Mutex<Session>>>>,
    capacity: Semaphore,
}
impl ConnectorBroker {
    pub fn new(store: Store, vault: CredentialVault) -> Self {
        Self {
            store,
            vault,
            sessions: Mutex::new(HashMap::new()),
            capacity: Semaphore::new(8),
        }
    }
    async fn session(&self, config: &ConnectorConfig) -> Result<Arc<Mutex<Session>>> {
        let mut map = self.sessions.lock().await;
        ensure!(
            map.contains_key(&config.id) || map.len() < 32,
            "Maximum active MCP connectors reached; disconnect an idle connector"
        );
        Ok(map
            .entry(config.id)
            .or_insert_with(|| {
                Arc::new(Mutex::new(Session {
                    client: None,
                    fingerprint: String::new(),
                    touched: Instant::now(),
                }))
            })
            .clone())
    }
    async fn ensure_connected(
        &self,
        session: &mut Session,
        config: &ConnectorConfig,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let fingerprint = format!("{:x}", Sha256::digest(serde_json::to_vec(config)?));
        if session.fingerprint == fingerprint
            && session.client.as_ref().is_some_and(|c| !c.is_closed())
        {
            session.touched = Instant::now();
            return Ok(());
        }
        if let Some(mut old) = session.client.take() {
            let _ = tokio::time::timeout(Duration::from_secs(3), old.close()).await;
        }
        let secret = if let Some(reference) = config.credential_ref.clone() {
            let vault = self.vault.clone();
            Some(
                tokio::task::spawn_blocking(move || vault.get(&reference))
                    .await??
                    .context("Connector credential is missing")?,
            )
        } else {
            None
        };
        let connect = async {
            match config.transport.as_str() {
                "stdio" => {
                    let executable = config
                        .command
                        .as_ref()
                        .context("Choose a local MCP executable")?;
                    ensure!(
                        executable.is_absolute() && executable.is_file(),
                        "MCP executable must be an existing absolute path"
                    );
                    let mut command = Command::new(executable);
                    // Background pipes must not create a separate Windows console.
                    #[cfg(windows)]
                    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
                    command.args(&config.args).kill_on_drop(true);
                    // A single explicit credential variable avoids inferring provider-specific names.
                    // Servers requiring other names should use a user-owned launcher.
                    if let Some(secret) = secret {
                        command.env("MCP_ACCESS_TOKEN", secret);
                    }
                    ().serve(TokioChildProcess::new(command)?)
                        .await
                        .context("Initialize local MCP server")
                }
                "http" | "streamable_http" => {
                    validate_endpoint(&config.endpoint)?;
                    let mut transport =
                        StreamableHttpClientTransportConfig::with_uri(config.endpoint.clone())
                            .max_concurrent_requests(4);
                    transport.reinit_on_expired_session = false; // never implicitly replay a write
                    transport.max_sse_event_size = 8 * 1024 * 1024;
                    if let Some(secret) = secret {
                        transport = transport.auth_header(secret);
                    }
                    let http = reqwest::Client::builder()
                        .connect_timeout(Duration::from_secs(10))
                        .redirect(reqwest::redirect::Policy::none())
                        .build()?;
                    ().serve(StreamableHttpClientTransport::with_client(http, transport))
                        .await
                        .context("Initialize remote MCP server")
                }
                other => bail!("Unsupported MCP transport {other}"),
            }
        };
        let client = tokio::select! {_ = cancel.cancelled()=>bail!("Connector initialization cancelled"),result=tokio::time::timeout(Duration::from_secs(20),connect)=>result.context("MCP initialization timed out")??};
        session.client = Some(client);
        session.fingerprint = fingerprint;
        session.touched = Instant::now();
        Ok(())
    }
    /// Explicit refresh. Tool schemas are persisted but not inserted into prompts.
    pub async fn tools(
        &self,
        config: &ConnectorConfig,
        cancel: &CancellationToken,
    ) -> Result<Vec<ToolDefinition>> {
        let _permit = tokio::select! {_ = cancel.cancelled()=>bail!("Connector discovery cancelled"),p=self.capacity.acquire()=>p?};
        let slot = self.session(config).await?;
        let mut session = tokio::select! {_ = cancel.cancelled()=>bail!("Connector discovery cancelled"),s=slot.lock()=>s};
        self.ensure_connected(&mut session, config, cancel).await?;
        let list = async {
            let client = session.client.as_ref().context("MCP client unavailable")?;
            let mut cursor = None;
            let mut output = Vec::new();
            let mut size = 0;
            loop {
                let page = client
                    .list_tools(Some(PaginatedRequestParams::default().with_cursor(cursor)))
                    .await?;
                for tool in page.tools {
                    let parameters = serde_json::to_value(&tool.input_schema)?;
                    size += parameters.to_string().len();
                    ensure!(
                        output.len() < 512 && size <= 2 * 1024 * 1024,
                        "Connector catalog exceeds the configured discovery budget"
                    );
                    let read_only = tool
                        .annotations
                        .as_ref()
                        .and_then(|a| a.read_only_hint)
                        .unwrap_or(false);
                    output.push(ToolDefinition {
                        name: tool.name.to_string(),
                        description: tool.description.as_deref().unwrap_or("").to_owned(),
                        parameters,
                        writes: !read_only,
                    });
                }
                cursor = page.next_cursor;
                if cursor.is_none() {
                    break;
                }
            }
            Ok::<_, anyhow::Error>(output)
        };
        let result = tokio::select! {_ = cancel.cancelled()=>bail!("Connector discovery cancelled"),result=tokio::time::timeout(Duration::from_secs(30),list)=>result.context("Connector discovery timed out")??};
        session.touched = Instant::now();
        drop(session);
        let store = self.store.clone();
        let id = config.id;
        let json = serde_json::to_string(&result)?;
        let fingerprint = format!("{:x}", Sha256::digest(serde_json::to_vec(config)?));
        tokio::task::spawn_blocking(move||store.call(move|conn|{conn.execute_batch("CREATE TABLE IF NOT EXISTS connector_tools(id TEXT PRIMARY KEY,fingerprint TEXT NOT NULL,json TEXT NOT NULL,updated_at INTEGER NOT NULL)")?;conn.execute("INSERT INTO connector_tools(id,fingerprint,json,updated_at) VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET fingerprint=excluded.fingerprint,json=excluded.json,updated_at=excluded.updated_at",rusqlite::params![id.to_string(),fingerprint,json,xylo_core::now_ms()])?;Ok(())})).await??;
        Ok(result)
    }
    pub fn cached_tools(&self, config: &ConnectorConfig) -> Result<Vec<ToolDefinition>> {
        use rusqlite::OptionalExtension;
        let id = config.id;
        let fingerprint = format!("{:x}", Sha256::digest(serde_json::to_vec(config)?));
        self.store.call(move|conn|{conn.execute_batch("CREATE TABLE IF NOT EXISTS connector_tools(id TEXT PRIMARY KEY,fingerprint TEXT NOT NULL,json TEXT NOT NULL,updated_at INTEGER NOT NULL)")?;let json:Option<String>=conn.query_row("SELECT json FROM connector_tools WHERE id=?1 AND fingerprint=?2",rusqlite::params![id.to_string(),fingerprint],|r|r.get(0)).optional()?;Ok(match json{Some(j)=>serde_json::from_str(&j)?,None=>Vec::new()})})
    }
    /// A caller must journal/authorize writes before entering this method. A timeout
    /// does not prove a remote write failed; return it to the journal as unknown.
    pub async fn call(
        &self,
        config: &ConnectorConfig,
        name: &str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let args = arguments
            .as_object()
            .context("Tool arguments must be a JSON object")?
            .clone();
        ensure!(
            serde_json::to_vec(&args)?.len() <= 1024 * 1024,
            "Tool arguments exceed 1 MiB"
        );
        let _permit = tokio::select! {_ = cancel.cancelled()=>bail!("Connector call cancelled"),p=self.capacity.acquire()=>p?};
        let slot = self.session(config).await?;
        let mut session = tokio::select! {_ = cancel.cancelled()=>bail!("Connector call cancelled"),s=slot.lock()=>s};
        self.ensure_connected(&mut session, config, cancel).await?;
        let request = CallToolRequestParams::new(name.to_owned()).with_arguments(args);
        let call = session
            .client
            .as_ref()
            .context("MCP client unavailable")?
            .call_tool(request);
        let result = tokio::select! {_ = cancel.cancelled()=>bail!("Connector call cancelled; remote outcome may be unknown"),result=tokio::time::timeout(Duration::from_secs(90),call)=>result.context("Connector call timed out; remote outcome may be unknown")??};
        session.touched = Instant::now();
        ensure!(
            result.is_error != Some(true),
            "MCP tool reported an execution error"
        );
        let result = serde_json::to_value(result)?;
        ensure!(
            serde_json::to_vec(&result)?.len() <= 8 * 1024 * 1024,
            "Connector result exceeds 8 MiB; retrieve a smaller page"
        );
        Ok(result)
    }
    pub async fn disconnect(&self, id: Uuid) -> Result<()> {
        let slot = { self.sessions.lock().await.remove(&id) };
        if let Some(slot) = slot {
            let mut session = slot.lock().await;
            if let Some(mut client) = session.client.take() {
                tokio::time::timeout(Duration::from_secs(5), client.close())
                    .await
                    .context("MCP shutdown timed out")??;
            }
        }
        Ok(())
    }
    pub async fn reap_idle(&self, idle: Duration) -> usize {
        let mut map = self.sessions.lock().await;
        let stale: Vec<_> = map
            .iter()
            .filter_map(|(id, slot)| {
                slot.try_lock()
                    .ok()
                    .filter(|s| s.touched.elapsed() >= idle)
                    .map(|_| *id)
            })
            .collect();
        for id in &stale {
            map.remove(id);
        }
        stale.len()
    }
}
pub(crate) fn validate_endpoint(endpoint: &str) -> Result<url::Url> {
    let url = url::Url::parse(endpoint)?;
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "Credentials must not be embedded in URLs"
    );
    ensure!(
        url.scheme() == "https"
            || (url.scheme() == "http"
                && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))),
        "Use HTTPS or loopback HTTP for a connector"
    );
    Ok(url)
}
