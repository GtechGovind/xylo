//! MCP client sessions with bounded concurrency, shared across turns.
//!
//! A [`ConnectorBroker`] keeps one connection per connector and reuses it for
//! later calls as long as the connector configuration and local launch scope
//! are unchanged. Clones share the connection pool, so a worker can hold one
//! broker for its session and hand a scoped clone to every turn. Connections
//! unused for [`MCP_IDLE_TIMEOUT`] are closed by a background reaper that lives
//! only as long as the pool.
//!
//! Closing is graceful: an in-flight request is first withdrawn with
//! `notifications/cancelled`, then the client is closed (for stdio servers rmcp
//! closes stdin and waits briefly for the server to exit), and only then is the
//! sandboxed process group killed. The first lines a local server writes to
//! stderr are kept and attached to connection errors.
use crate::{CredentialVault, Store};
use anyhow::{Context, Result, anyhow, bail, ensure};
use rmcp::{
    RoleClient, ServiceExt,
    model::{
        CallToolRequest, CallToolRequestParams, CallToolResult, ClientRequest,
        PaginatedRequestParams, ServerResult, ToolAnnotations,
    },
    service::{PeerRequestOptions, RunningService},
    transport::{
        StreamableHttpClientTransport, TokioChildProcess,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncReadExt,
    sync::{Mutex, OwnedMutexGuard, Semaphore},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use xylo_core::{ConnectorConfig, ToolDefinition};

/// How long an unused connection stays open before the reaper closes it.
pub const MCP_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// Upper bound between reaper sweeps.
const REAP_INTERVAL: Duration = Duration::from_secs(30);
/// Bounded wait for a graceful client close (rmcp waits 3 s for a stdio server
/// to exit after stdin closes, then kills it).
const CLOSE_GRACE: Duration = Duration::from_secs(5);
/// Bounded wait for delivering `notifications/cancelled`.
const CANCEL_NOTICE_TIMEOUT: Duration = Duration::from_secs(1);
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(20);
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);
const CALL_TIMEOUT: Duration = Duration::from_secs(90);
/// How much of a local server's stderr is kept for error messages.
const STDERR_MAX_LINES: usize = 20;
const STDERR_MAX_BYTES: usize = 4096;

/// Explicit local process scope. Secret values are supplied only by the caller's
/// credential vault; arbitrary parent environment is never inherited.
#[derive(Clone, Default, serde::Serialize)]
pub struct ConnectorLaunchScope {
    pub workspace: PathBuf,
    pub read_roots: Vec<PathBuf>,
    pub protected_paths: Vec<PathBuf>,
    pub allowed_domains: Vec<String>,
    pub environment: BTreeMap<String, String>,
}

/// The sandboxed process group of a stdio server. Dropping it kills the whole
/// group, which also reaches helpers the server spawned; graceful shutdown
/// happens before this backstop.
struct LocalProcess {
    pid: Option<u32>,
    _temporary: tempfile::TempDir,
}
impl Drop for LocalProcess {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid.and_then(|pid| i32::try_from(pid).ok()) {
            // SAFETY: plain syscall on a process group this broker created.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
}

/// The first lines a local server wrote to stderr, bounded in lines and bytes.
/// The pipe keeps being drained after the bound so the server never blocks on it.
#[derive(Clone, Default)]
struct StderrTail(Arc<std::sync::Mutex<StderrState>>);
#[derive(Default)]
struct StderrState {
    bytes: Vec<u8>,
    lines: usize,
    finished: bool,
}
impl StderrTail {
    fn capture(&self, mut stderr: tokio::process::ChildStderr) {
        let state = self.0.clone();
        tokio::spawn(async move {
            let mut chunk = [0u8; 1024];
            loop {
                let read = match stderr.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => read,
                };
                let Ok(mut state) = state.lock() else { break };
                state.append(&chunk[..read]);
            }
            if let Ok(mut state) = state.lock() {
                state.finished = true;
            }
        });
    }

    /// Waits briefly for a dying server's stderr to reach EOF, then returns
    /// what was captured with `secret` redacted.
    async fn snapshot(&self, secret: Option<&str>) -> Option<String> {
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            let finished = self.0.lock().map(|state| state.finished).unwrap_or(true);
            if finished || Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let text = self
            .0
            .lock()
            .ok()
            .map(|state| String::from_utf8_lossy(&state.bytes).trim().to_owned())?;
        let text = match secret {
            Some(secret) if !secret.is_empty() => text.replace(secret, "[REDACTED]"),
            _ => text,
        };
        (!text.is_empty()).then_some(text)
    }

    /// Attaches the captured stderr to a connection error.
    async fn annotate(&self, error: anyhow::Error, secret: Option<&str>) -> anyhow::Error {
        match self.snapshot(secret).await {
            Some(stderr) => anyhow!("{error:#}. Server stderr:\n{stderr}"),
            None => error,
        }
    }
}
impl StderrState {
    fn append(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.lines >= STDERR_MAX_LINES || self.bytes.len() >= STDERR_MAX_BYTES {
                return;
            }
            self.bytes.push(byte);
            if byte == b'\n' {
                self.lines += 1;
            }
        }
    }
}

type Client = RunningService<RoleClient, ()>;
struct Session {
    client: Option<Client>,
    fingerprint: String,
    touched: Instant,
    process: Option<LocalProcess>,
}
impl Session {
    /// Closes the client gracefully within [`CLOSE_GRACE`], then kills the
    /// process group. The session can reconnect afterwards.
    async fn close(&mut self) -> Result<()> {
        let closed = match self.client.take() {
            Some(mut client) => match client.close_with_timeout(CLOSE_GRACE).await {
                Ok(Some(_)) => Ok(()),
                Ok(None) => Err(anyhow!("MCP shutdown timed out")),
                Err(error) => Err(anyhow!("MCP shutdown failed: {error}")),
            },
            None => Ok(()),
        };
        self.process = None;
        self.fingerprint.clear();
        closed
    }
}

/// Connections shared by every clone of one broker.
struct Pool {
    store: Store,
    vault: CredentialVault,
    sessions: Mutex<HashMap<Uuid, Arc<Mutex<Session>>>>,
    capacity: Semaphore,
    configuration_gates: Mutex<HashMap<Uuid, Weak<Mutex<()>>>>,
    idle_timeout_ms: AtomicU64,
    reaper_started: AtomicBool,
}
impl Pool {
    fn idle_timeout(&self) -> Duration {
        Duration::from_millis(self.idle_timeout_ms.load(Ordering::Relaxed))
    }

    /// Starts the idle reaper once per pool. It holds only a weak reference,
    /// so dropping the last broker clone also ends the reaper.
    fn start_reaper(self: &Arc<Self>) {
        if self.reaper_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            self.reaper_started.store(false, Ordering::Release);
            return;
        };
        let pool = Arc::downgrade(self);
        runtime.spawn(async move {
            loop {
                let interval = match pool.upgrade() {
                    Some(pool) => pool
                        .idle_timeout()
                        .min(REAP_INTERVAL)
                        .max(Duration::from_millis(10)),
                    None => return,
                };
                tokio::time::sleep(interval).await;
                let Some(pool) = pool.upgrade() else { return };
                let idle = pool.idle_timeout();
                pool.reap(idle).await;
            }
        });
    }

    /// Removes sessions unused for `idle` and closes them gracefully. Sessions
    /// busy with a request are left alone.
    async fn reap(&self, idle: Duration) -> usize {
        let stale: Vec<OwnedMutexGuard<Session>> = {
            let mut map = self.sessions.lock().await;
            let mut stale = Vec::new();
            map.retain(|_, slot| match slot.clone().try_lock_owned() {
                Ok(session) if session.touched.elapsed() >= idle => {
                    stale.push(session);
                    false
                }
                _ => true,
            });
            stale
        };
        let count = stale.len();
        for mut session in stale {
            if let Err(error) = session.close().await {
                tracing::debug!("Closing idle MCP connection: {error:#}");
            }
        }
        count
    }
}

/// A single connection per connector with bounded aggregate concurrency. Construction
/// starts no processes and does no network or credential-store work. Clones share
/// connections; each clone carries its own local launch scope.
#[derive(Clone)]
pub struct ConnectorBroker {
    pool: Arc<Pool>,
    local_scope: Option<ConnectorLaunchScope>,
}
impl ConnectorBroker {
    /// Creates an empty pool with the default [`MCP_IDLE_TIMEOUT`].
    pub fn new(store: Store, vault: CredentialVault) -> Self {
        Self {
            pool: Arc::new(Pool {
                store,
                vault,
                sessions: Mutex::new(HashMap::new()),
                capacity: Semaphore::new(8),
                configuration_gates: Mutex::new(HashMap::new()),
                idle_timeout_ms: AtomicU64::new(duration_ms(MCP_IDLE_TIMEOUT)),
                reaper_started: AtomicBool::new(false),
            }),
            local_scope: None,
        }
    }
    /// Changes how long unused connections stay open. Applies to every clone.
    pub fn with_idle_timeout(self, idle: Duration) -> Self {
        self.pool
            .idle_timeout_ms
            .store(duration_ms(idle), Ordering::Relaxed);
        self
    }
    fn fingerprint(
        config: &ConnectorConfig,
        scope: Option<&ConnectorLaunchScope>,
    ) -> Result<String> {
        let scope = if config.transport == "stdio" {
            scope
        } else {
            None
        };
        Ok(hex::encode(Sha256::digest(serde_json::to_vec(&(
            config, scope,
        ))?)))
    }
    /// Serializes configuration changes for one connector.
    pub async fn configuration_guard(&self, id: Uuid) -> Result<OwnedMutexGuard<()>> {
        let gate = {
            let mut gates = self.pool.configuration_gates.lock().await;
            gates.retain(|_, gate| gate.strong_count() > 0);
            if let Some(gate) = gates.get(&id).and_then(Weak::upgrade) {
                gate
            } else {
                ensure!(
                    gates.len() < 32,
                    "Maximum concurrent connector configuration operations reached"
                );
                let gate = Arc::new(Mutex::new(()));
                gates.insert(id, Arc::downgrade(&gate));
                gate
            }
        };
        Ok(gate.lock_owned().await)
    }
    /// Sets the sandbox scope local (stdio) servers launched by this clone run in.
    pub fn with_local_scope(mut self, scope: ConnectorLaunchScope) -> Self {
        self.local_scope = Some(scope);
        self
    }
    async fn session(&self, config: &ConnectorConfig) -> Result<Arc<Mutex<Session>>> {
        let mut map = self.pool.sessions.lock().await;
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
                    process: None,
                }))
            })
            .clone())
    }
    async fn ensure_connected(
        &self,
        session: &mut Session,
        config: &ConnectorConfig,
        cancel: &CancellationToken,
        local_scope: Option<&ConnectorLaunchScope>,
    ) -> Result<()> {
        ensure!(
            !cancel.is_cancelled(),
            "Connector initialization cancelled before dispatch"
        );
        let fingerprint = Self::fingerprint(config, local_scope)?;
        if session.fingerprint == fingerprint
            && session.client.as_ref().is_some_and(|c| !c.is_closed())
        {
            session.touched = Instant::now();
            return Ok(());
        }
        if let Err(error) = session.close().await {
            tracing::debug!("Closing replaced MCP connection: {error:#}");
        }
        let secret = if let Some(reference) = config.credential_ref.clone() {
            let vault = self.pool.vault.clone();
            Some(
                tokio::task::spawn_blocking(move || vault.get(&reference))
                    .await??
                    .context("Connector credential is missing")?,
            )
        } else {
            None
        };
        let stderr = StderrTail::default();
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
                    let scope = local_scope
                        .context("Local MCP needs an explicit workspace sandbox scope")?;
                    let mut prepared = crate::sandbox::prepare_with_policy(
                        &executable.to_string_lossy(),
                        &config.args,
                        &scope.workspace,
                        &scope.read_roots,
                        &scope.protected_paths,
                        &scope.allowed_domains,
                    )?;
                    let mut environment = scope.environment.clone();
                    if let Some(secret) = secret.clone() {
                        environment.insert("MCP_ACCESS_TOKEN".into(), secret);
                    }
                    crate::sandbox::set_child_environment(&mut prepared, &environment)?;
                    #[cfg(unix)]
                    prepared.command.process_group(0);
                    #[cfg(windows)]
                    prepared.command.creation_flags(0x0800_0000);
                    ensure!(
                        !cancel.is_cancelled(),
                        "Connector initialization cancelled before process start"
                    );
                    let (transport, child_stderr) = TokioChildProcess::builder(prepared.command)
                        .stderr(std::process::Stdio::piped())
                        .spawn()?;
                    if let Some(child_stderr) = child_stderr {
                        stderr.capture(child_stderr);
                    }
                    let owned = LocalProcess {
                        pid: transport.id(),
                        _temporary: prepared.temporary,
                    };
                    let client =
                        ().serve(transport)
                            .await
                            .context("Initialize sandboxed local MCP server")?;
                    Ok((client, Some(owned)))
                }
                "http" | "streamable_http" => {
                    validate_endpoint(&config.endpoint)?;
                    let mut transport =
                        StreamableHttpClientTransportConfig::with_uri(config.endpoint.clone())
                            .max_concurrent_requests(4);
                    transport.reinit_on_expired_session = false; // never implicitly replay a write
                    transport.max_sse_event_size = 8 * 1024 * 1024;
                    if let Some(secret) = secret.clone() {
                        transport = transport.auth_header(secret);
                    }
                    let http = reqwest::Client::builder()
                        .connect_timeout(Duration::from_secs(10))
                        .retry(reqwest::retry::never())
                        .redirect(reqwest::redirect::Policy::none())
                        .build()?;
                    let client =
                        ().serve(StreamableHttpClientTransport::with_client(http, transport))
                            .await
                            .context("Initialize remote MCP server")?;
                    Ok((client, None))
                }
                other => bail!("Unsupported MCP transport {other}"),
            }
        };
        let connected = tokio::select! {
            biased;
            _ = cancel.cancelled() => bail!("Connector initialization cancelled"),
            result = tokio::time::timeout(INITIALIZE_TIMEOUT, connect) => {
                result.map_err(|_| anyhow!("MCP initialization timed out")).and_then(|result| result)
            }
        };
        let (client, process) = match connected {
            Ok(connected) => connected,
            Err(error) => return Err(stderr.annotate(error, secret.as_deref()).await),
        };
        session.client = Some(client);
        session.process = process;
        session.fingerprint = fingerprint;
        session.touched = Instant::now();
        self.pool.start_reaper();
        Ok(())
    }
    /// Explicit refresh. Tool schemas are persisted but not inserted into prompts.
    pub async fn tools(
        &self,
        config: &ConnectorConfig,
        cancel: &CancellationToken,
    ) -> Result<Vec<ToolDefinition>> {
        self.tools_with_scope(config, cancel, self.local_scope.as_ref())
            .await
    }
    /// Like [`Self::tools`], with an explicit local launch scope.
    pub async fn tools_scoped(
        &self,
        config: &ConnectorConfig,
        scope: &ConnectorLaunchScope,
        cancel: &CancellationToken,
    ) -> Result<Vec<ToolDefinition>> {
        self.tools_with_scope(config, cancel, Some(scope)).await
    }
    async fn tools_with_scope(
        &self,
        config: &ConnectorConfig,
        cancel: &CancellationToken,
        local_scope: Option<&ConnectorLaunchScope>,
    ) -> Result<Vec<ToolDefinition>> {
        let _permit = tokio::select! {_ = cancel.cancelled()=>bail!("Connector discovery cancelled"),p=self.pool.capacity.acquire()=>p?};
        let slot = self.session(config).await?;
        let mut session = tokio::select! {_ = cancel.cancelled()=>bail!("Connector discovery cancelled"),s=slot.lock()=>s};
        self.ensure_connected(&mut session, config, cancel, local_scope)
            .await?;
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
                    output.push(ToolDefinition {
                        name: tool.name.to_string(),
                        description: tool.description.as_deref().unwrap_or("").to_owned(),
                        parameters,
                        writes: tool_writes(tool.annotations.as_ref()),
                    });
                }
                cursor = page.next_cursor;
                if cursor.is_none() {
                    break;
                }
            }
            Ok::<_, anyhow::Error>(output)
        };
        let result = tokio::select! {_ = cancel.cancelled()=>Err(anyhow!("Connector discovery cancelled")),result=tokio::time::timeout(DISCOVERY_TIMEOUT,list)=>result.context("Connector discovery timed out").and_then(|result| result)};
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                if let Err(close) = session.close().await {
                    tracing::debug!("Closing failed MCP connection: {close:#}");
                }
                return Err(error);
            }
        };
        session.touched = Instant::now();
        drop(session);
        let store = self.pool.store.clone();
        let id = config.id;
        let json = serde_json::to_string(&result)?;
        let fingerprint = Self::fingerprint(config, local_scope)?;
        tokio::task::spawn_blocking(move||store.call(move|conn|{conn.execute("INSERT INTO connector_tools(id,fingerprint,json,updated_at) VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET fingerprint=excluded.fingerprint,json=excluded.json,updated_at=excluded.updated_at",rusqlite::params![id.to_string(),fingerprint,json,xylo_core::now_ms()])?;Ok(())})).await??;
        Ok(result)
    }
    /// The catalog from the last discovery with the same configuration and scope.
    pub fn cached_tools(&self, config: &ConnectorConfig) -> Result<Vec<ToolDefinition>> {
        self.cached_tools_with_scope(config, self.local_scope.as_ref())
    }
    fn cached_tools_with_scope(
        &self,
        config: &ConnectorConfig,
        scope: Option<&ConnectorLaunchScope>,
    ) -> Result<Vec<ToolDefinition>> {
        use rusqlite::OptionalExtension;
        let id = config.id;
        let fingerprint = Self::fingerprint(config, scope)?;
        self.pool.store.call(move |conn| {
            let json: Option<String> = conn
                .query_row(
                    "SELECT json FROM connector_tools WHERE id=?1 AND fingerprint=?2",
                    rusqlite::params![id.to_string(), fingerprint],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(match json {
                Some(json) => serde_json::from_str(&json)?,
                None => Vec::new(),
            })
        })
    }
    /// A caller must journal/authorize writes before entering this method. A timeout
    /// does not prove a remote write failed; return it to the journal as unknown.
    /// On cancellation or timeout the request is withdrawn with
    /// `notifications/cancelled` and the connection is closed gracefully.
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
        let _permit = tokio::select! {_ = cancel.cancelled()=>bail!("Connector call cancelled"),p=self.pool.capacity.acquire()=>p?};
        let slot = self.session(config).await?;
        let mut session = tokio::select! {_ = cancel.cancelled()=>bail!("Connector call cancelled"),s=slot.lock()=>s};
        self.ensure_connected(&mut session, config, cancel, self.local_scope.as_ref())
            .await?;
        let result = Self::call_tool(&session, name, args, cancel).await;
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                if let Err(close) = session.close().await {
                    tracing::debug!("Closing interrupted MCP connection: {close:#}");
                }
                return Err(error);
            }
        };
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
    /// Sends one `tools/call`. When the caller cancels or the call times out,
    /// the server is told with `notifications/cancelled` before returning.
    async fn call_tool(
        session: &Session,
        name: &str,
        arguments: serde_json::Map<String, Value>,
        cancel: &CancellationToken,
    ) -> Result<CallToolResult> {
        let client = session.client.as_ref().context("MCP client unavailable")?;
        let params = CallToolRequestParams::new(name.to_owned()).with_arguments(arguments);
        let mut handle = client
            .send_cancellable_request(
                ClientRequest::CallToolRequest(CallToolRequest::new(params)),
                PeerRequestOptions::no_options(),
            )
            .await?;
        let interrupted = tokio::select! {
            _ = cancel.cancelled() => "Connector call cancelled; remote outcome may be unknown",
            _ = tokio::time::sleep(CALL_TIMEOUT) => "Connector call timed out; remote outcome may be unknown",
            response = &mut handle.rx => {
                return match response.map_err(|_| anyhow!("MCP connection closed during the call"))?? {
                    ServerResult::CallToolResult(result) => Ok(result),
                    _ => Err(anyhow!("MCP server answered tools/call with an unsupported result")),
                };
            }
        };
        let notice = handle.cancel(Some(interrupted.to_owned()));
        if tokio::time::timeout(CANCEL_NOTICE_TIMEOUT, notice)
            .await
            .is_err()
        {
            tracing::debug!("MCP cancellation notice was not delivered in time");
        }
        bail!(interrupted)
    }
    /// Abort a cancelled scope without waiting for an untrusted shutdown reply.
    pub async fn abort(&self, id: Uuid) {
        let slot = self.pool.sessions.lock().await.remove(&id);
        if let Some(slot) = slot {
            let mut session = slot.lock().await;
            session.process = None;
            session.client = None;
            session.fingerprint.clear();
        }
    }
    /// Closes one connector's connection gracefully.
    pub async fn disconnect(&self, id: Uuid) -> Result<()> {
        let slot = { self.pool.sessions.lock().await.remove(&id) };
        if let Some(slot) = slot {
            slot.lock().await.close().await?;
        }
        Ok(())
    }
    /// Closes connections unused for at least `idle`; returns how many closed.
    /// The background reaper calls this with the pool's idle timeout.
    pub async fn reap_idle(&self, idle: Duration) -> usize {
        self.pool.reap(idle).await
    }
    /// Closes every connection gracefully, for application or worker exit.
    /// A connection busy past the grace period is killed when its call ends.
    pub async fn shutdown(&self) {
        let slots: Vec<_> = self
            .pool
            .sessions
            .lock()
            .await
            .drain()
            .map(|(_, slot)| slot)
            .collect();
        for slot in slots {
            if let Ok(mut session) = tokio::time::timeout(CLOSE_GRACE, slot.lock()).await
                && let Err(error) = session.close().await
            {
                tracing::debug!("Closing MCP connection at shutdown: {error:#}");
            }
        }
    }
}

/// MCP tool annotations are hints from the server. A tool counts as read-only
/// only when it says so and does not also claim to be destructive; anything
/// unannotated is treated as a write and goes through the mutation gate.
fn tool_writes(annotations: Option<&ToolAnnotations>) -> bool {
    !annotations.is_some_and(|annotations| {
        annotations.read_only_hint == Some(true) && annotations.destructive_hint != Some(true)
    })
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
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

#[cfg(test)]
mod tests;
