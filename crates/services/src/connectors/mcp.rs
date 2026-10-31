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
use futures_util::{Stream, StreamExt};
use reqwest::{
    StatusCode,
    header::{ACCEPT, CONTENT_TYPE, HeaderName, HeaderValue, RETRY_AFTER},
};
use rmcp::{
    RoleClient, ServiceExt,
    model::{
        CallToolRequest, CallToolRequestParams, CallToolResult, ClientJsonRpcMessage,
        ClientRequest, JsonRpcMessage, PaginatedRequestParams, ServerJsonRpcMessage, ServerResult,
        ToolAnnotations,
    },
    service::{PeerRequestOptions, RunningService},
    transport::{
        StreamableHttpClientTransport, TokioChildProcess,
        common::{
            client_side_sse::BoxedSseResponse,
            http_header::{EVENT_STREAM_MIME_TYPE, HEADER_SESSION_ID, JSON_MIME_TYPE},
        },
        streamable_http_client::{
            HttpStatusError, SseError, StreamableHttpClient, StreamableHttpClientTransportConfig,
            StreamableHttpError, StreamableHttpPostResponse,
        },
    },
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
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
        // Trust changes how Xylo treats the server's hints, not the server:
        // toggling it keeps the connection and the discovered catalog.
        let launch = ConnectorConfig {
            trusted: false,
            ..config.clone()
        };
        Ok(hex::encode(Sha256::digest(serde_json::to_vec(&(
            launch, scope,
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
                    transport.max_sse_event_size = HTTP_BODY_LIMIT;
                    if let Some(secret) = secret.clone() {
                        transport = transport.auth_header(secret);
                    }
                    let http = reqwest::Client::builder()
                        .connect_timeout(Duration::from_secs(10))
                        .retry(reqwest::retry::never())
                        .redirect(reqwest::redirect::Policy::none())
                        .build()?;
                    let client = ()
                        .serve(StreamableHttpClientTransport::with_client(
                            BoundedHttpClient(http),
                            transport,
                        ))
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
        tokio::task::spawn_blocking(move || save_catalog(&store, id, fingerprint, json)).await??;
        Ok(result)
    }
    /// Records `tools` as `config`'s discovered catalog for this broker's
    /// launch scope, as a live discovery would, without starting a server.
    #[cfg(any(test, feature = "test-support"))]
    pub fn cache_tools_for_tests(
        &self,
        config: &ConnectorConfig,
        tools: &[ToolDefinition],
    ) -> Result<()> {
        let fingerprint = Self::fingerprint(config, self.local_scope.as_ref())?;
        save_catalog(
            &self.pool.store,
            config.id,
            fingerprint,
            serde_json::to_string(tools)?,
        )
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

/// Persists one connector's discovered catalog under its fingerprint.
fn save_catalog(store: &Store, id: Uuid, fingerprint: String, json: String) -> Result<()> {
    store.call(move |conn| {
        conn.execute(
            "INSERT INTO connector_tools(id,fingerprint,json,updated_at) VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET fingerprint=excluded.fingerprint,json=excluded.json,updated_at=excluded.updated_at",
            rusqlite::params![id.to_string(), fingerprint, json, xylo_core::now_ms()],
        )?;
        Ok(())
    })
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

/// Most bytes of one remote MCP response body, or of one SSE event, that are
/// ever held in memory.
const HTTP_BODY_LIMIT: usize = 8 * 1024 * 1024;

type HttpError = StreamableHttpError<reqwest::Error>;
/// rmcp's SSE event type. `sse_stream` is not re-exported, so the type is
/// named through rmcp's public stream alias.
type SseEvent = <<BoxedSseResponse as Stream>::Item as OkType>::Ok;

/// Names the success type of a `Result`; only used to spell [`SseEvent`].
trait OkType {
    type Ok;
}

impl<T, E> OkType for Result<T, E> {
    type Ok = T;
}

/// The streamable-HTTP backend for remote MCP servers, built on `reqwest`.
///
/// rmcp's own `reqwest::Client` backend reads JSON replies and error bodies
/// with `Response::bytes`/`text`, which buffer whatever the server sends; its
/// `max_sse_event_size` only bounds SSE events. A hostile server could
/// therefore exhaust memory with one response. POSTs are handled here so every
/// body goes through [`HTTP_BODY_LIMIT`]. GET streams (already parsed by rmcp's
/// bounded SSE parser) and DELETE (whose body is never read) are delegated.
#[derive(Clone)]
struct BoundedHttpClient(reqwest::Client);

impl StreamableHttpClient for BoundedHttpClient {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, HttpError> {
        self.post_message_with_max_sse_event_size(
            uri,
            message,
            session_id,
            auth_header,
            custom_headers,
            HTTP_BODY_LIMIT,
        )
        .await
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<StreamableHttpPostResponse, HttpError> {
        let mut request = self.0.post(uri.as_ref()).header(
            ACCEPT,
            format!("{EVENT_STREAM_MIME_TYPE}, {JSON_MIME_TYPE}"),
        );
        if let Some(token) = auth_header {
            request = request.bearer_auth(token);
        }
        if let Some(session) = &session_id {
            request = request.header(HEADER_SESSION_ID, session.as_ref());
        }
        for (name, value) in custom_headers {
            request = request.header(name, value);
        }
        let response = request.json(&message).send().await?;
        let status = response.status();
        if matches!(status, StatusCode::ACCEPTED | StatusCode::NO_CONTENT) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if status == StatusCode::NOT_FOUND && session_id.is_some() {
            return Err(StreamableHttpError::SessionExpired);
        }
        let headers = response.headers();
        let new_session = headers
            .get(HEADER_SESSION_ID)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let content_type = headers
            .get(CONTENT_TYPE)
            .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned());
        let is_json = content_type
            .as_deref()
            .is_some_and(|value| value.starts_with(JSON_MIME_TYPE));
        // Only requests wait for a reply; other messages tolerate an empty or
        // unusable body, as rmcp's backend does.
        let awaits_reply = matches!(message, ClientJsonRpcMessage::Request(_));

        if !status.is_success() {
            let mut status_error =
                (status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()).then(|| {
                    let retry_after = headers
                        .get(RETRY_AFTER)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned);
                    HttpStatusError::new(status.as_u16(), retry_after)
                });
            // Rate limits and non-JSON failures carry nothing worth reading.
            if let Some(error) =
                status_error.take_if(|_| !is_json || status == StatusCode::TOO_MANY_REQUESTS)
            {
                return Err(error.into());
            }
            let body = read_bounded(response, HTTP_BODY_LIMIT).await?;
            // A JSON-RPC error body is the server's real answer; 401/403 keep
            // their status so they are never mistaken for an ordinary error.
            let auth_failure = matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN);
            if is_json
                && !auth_failure
                && let Ok(error @ JsonRpcMessage::Error(_)) =
                    serde_json::from_slice::<ServerJsonRpcMessage>(&body)
            {
                return Ok(StreamableHttpPostResponse::Json(error, new_session));
            }
            if let Some(error) = status_error {
                return Err(error.into());
            }
            return Err(StreamableHttpError::UnexpectedServerResponse(
                format!("HTTP {status}: {}", body_preview(&body)).into(),
            ));
        }
        // Some servers answer notifications with an empty 200 instead of 202.
        if !awaits_reply && response.content_length() == Some(0) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        match content_type.as_deref() {
            Some(value) if value.starts_with(EVENT_STREAM_MIME_TYPE) => {
                Ok(StreamableHttpPostResponse::Sse(
                    bounded_sse(response, max_sse_event_size),
                    new_session,
                ))
            }
            Some(_) if is_json => {
                let body = read_bounded(response, HTTP_BODY_LIMIT).await?;
                match serde_json::from_slice::<ServerJsonRpcMessage>(&body) {
                    Ok(reply) => Ok(StreamableHttpPostResponse::Json(reply, new_session)),
                    Err(_) if !awaits_reply => Ok(StreamableHttpPostResponse::Accepted),
                    Err(error) => Err(StreamableHttpError::UnexpectedServerResponse(
                        format!(
                            "could not parse JSON-RPC response: {error}: {}",
                            body_preview(&body)
                        )
                        .into(),
                    )),
                }
            }
            _ => Err(StreamableHttpError::UnexpectedContentType(content_type)),
        }
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), HttpError> {
        self.0
            .delete_session(uri, session_id, auth_header, custom_headers)
            .await
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxedSseResponse, HttpError> {
        self.get_stream_with_max_sse_event_size(
            uri,
            session_id,
            last_event_id,
            auth_header,
            custom_headers,
            HTTP_BODY_LIMIT,
        )
        .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<BoxedSseResponse, HttpError> {
        self.0
            .get_stream_with_max_sse_event_size(
                uri,
                session_id,
                last_event_id,
                auth_header,
                custom_headers,
                max_sse_event_size,
            )
            .await
    }
}

fn body_too_large(limit: usize) -> String {
    format!(
        "MCP server response exceeds the {} MiB size limit",
        limit / (1024 * 1024)
    )
}

/// Reads a whole response body, failing once it would exceed `limit` bytes.
///
/// `Content-Length` rejects an honest oversized body before any of it is read;
/// the running total catches chunked bodies that declare no length.
async fn read_bounded(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>, HttpError> {
    let too_large = || StreamableHttpError::UnexpectedServerResponse(body_too_large(limit).into());
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(too_large());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > limit {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// A short, lossy view of a body for error messages.
fn body_preview(body: &[u8]) -> String {
    const PREVIEW: usize = 256;
    let mut preview = String::from_utf8_lossy(&body[..body.len().min(PREVIEW)]).into_owned();
    if body.len() > PREVIEW {
        preview.push('…');
    }
    preview
}

/// Parses an SSE response body while holding at most `limit` bytes of any one
/// event. rmcp's bounded parser is crate-private, so POST streams need this.
fn bounded_sse(response: reqwest::Response, limit: usize) -> BoxedSseResponse {
    let state = (
        response.bytes_stream().boxed(),
        SseParser::new(limit),
        false,
    );
    futures_util::stream::unfold(state, |(mut body, mut parser, failed)| async move {
        if failed {
            return None;
        }
        loop {
            if let Some(event) = parser.ready.pop_front() {
                return Some((Ok(event), (body, parser, false)));
            }
            let error = match body.next().await? {
                Ok(chunk) => match parser.feed(&chunk) {
                    Ok(()) => continue,
                    Err(error) => error,
                },
                Err(error) => SseError::Body(Box::new(error)),
            };
            // Stop after the first error; an incomplete event is never emitted.
            return Some((Err(error), (body, parser, true)));
        }
    })
    .boxed()
}

/// Incremental parser for the `text/event-stream` format.
struct SseParser {
    limit: usize,
    /// Bytes of the line not yet terminated.
    line: Vec<u8>,
    /// The event being assembled and the bytes it retains.
    event: Option<SseEvent>,
    retained: usize,
    /// A chunk ended in `\r`, so a leading `\n` in the next one is its pair.
    skip_lf: bool,
    first_line: bool,
    ready: VecDeque<SseEvent>,
}

impl SseParser {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            line: Vec::new(),
            event: None,
            retained: 0,
            skip_lf: false,
            first_line: true,
            ready: VecDeque::new(),
        }
    }

    fn feed(&mut self, mut chunk: &[u8]) -> Result<(), SseError> {
        if std::mem::take(&mut self.skip_lf) && chunk.first() == Some(&b'\n') {
            chunk = &chunk[1..];
        }
        while !chunk.is_empty() {
            let end = chunk.iter().position(|byte| matches!(byte, b'\r' | b'\n'));
            let (part, rest) = chunk.split_at(end.unwrap_or(chunk.len()));
            // Checked before buffering so a line without a terminator is
            // never held past the limit.
            if self.retained + self.line.len() + part.len() > self.limit {
                return Err(SseError::Body(body_too_large(self.limit).into()));
            }
            self.line.extend_from_slice(part);
            let Some((&delimiter, mut rest)) = rest.split_first() else {
                return Ok(());
            };
            if delimiter == b'\r' {
                match rest.first() {
                    Some(b'\n') => rest = &rest[1..],
                    None => self.skip_lf = true,
                    Some(_) => {}
                }
            }
            let line = std::mem::take(&mut self.line);
            self.finish_line(&line)?;
            self.line = line;
            self.line.clear();
            chunk = rest;
        }
        Ok(())
    }

    fn finish_line(&mut self, mut line: &[u8]) -> Result<(), SseError> {
        if std::mem::take(&mut self.first_line) {
            line = line.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(line);
        }
        if line.is_empty() {
            self.ready.extend(self.event.take());
            self.retained = 0;
            return Ok(());
        }
        let (name, value) = match line.iter().position(|byte| *byte == b':') {
            Some(0) => return Ok(()), // comment
            Some(colon) => (&line[..colon], &line[colon + 1..]),
            None => (line, &[][..]),
        };
        let value = value.strip_prefix(b" ").unwrap_or(value);
        let value = std::str::from_utf8(value).map_err(SseError::Utf8Parse)?;
        let event = self.event.get_or_insert_default();
        match name {
            b"data" => match &mut event.data {
                Some(data) => {
                    data.push('\n');
                    data.push_str(value);
                }
                None => event.data = Some(value.to_owned()),
            },
            b"event" => event.event = Some(value.to_owned()),
            b"id" if !value.contains('\0') => event.id = Some(value.to_owned()),
            b"retry" => event.retry = value.parse().ok().or(event.retry),
            _ => return Ok(()), // unknown fields are ignored per the SSE spec
        }
        self.retained += value.len() + 1;
        Ok(())
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

#[cfg(test)]
mod tests;
