//! Worker processes: spawn and protocol handshake, the event reader, the warm
//! idle pool, stop escalation and routing of replies to control commands.
//!
//! Invariants:
//! - `Unlock` is the first command a worker receives, then `Hello`. A worker
//!   is used only after it answers that `Hello` with `Ready` in this host's
//!   protocol version; anything else fails the spawn with a clear message.
//! - Events of the active run reach the UI with per-session host sequence
//!   numbers. A run's state (`active`, `run_request`, …) is cleared only after
//!   its terminal event is in the UI channel.
//! - Replies to control commands (Approve, SetPermissions, Cancel…) are routed
//!   by request id instead of being dropped.
//! - A Stop the worker does not honor within [`STOP_GRACE`] kills the process;
//!   the reaper then records interrupted actions and reports a cancellation.
//! - Idle workers stay warm for [`IDLE_WORKER_TTL`], at most
//!   [`MAX_IDLE_WORKERS`] of them, least recently used retired first.
use super::*;
use std::{collections::VecDeque, sync::atomic::AtomicU64, time::Instant};
use tokio::{io::AsyncBufRead, process::ChildStdout};

/// How long a cancelled run may take to reach a terminal event before the
/// host ends its process.
pub(super) const STOP_GRACE: Duration = if cfg!(test) {
    Duration::from_millis(300)
} else {
    Duration::from_secs(10)
};
/// Idle workers kept warm for fast chat switches.
pub(super) const MAX_IDLE_WORKERS: usize = 4;
/// Idle workers older than this are retired.
pub(super) const IDLE_WORKER_TTL: Duration = Duration::from_secs(5 * 60);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
/// Replies to control commands arrive quickly; older entries are forgotten.
const CONTROL_LEDGER_CAPACITY: usize = 128;
const FORCED_STOP_NOTICE: &str = "The task did not stop in time, so Xylo ended it. Actions it had started are listed for review.";
const LOST_DECISION: &str = "The task is no longer running, so this decision was not applied.";

/// The mode a turn runs with. Scheduled turns run unattended, so Full access
/// (no sandbox, no approvals) becomes Approve for me for them; every other
/// mode, and every interactive or queued turn, keeps the setting unchanged.
pub(super) fn effective_mode(mode: PermissionMode, scheduled: bool) -> PermissionMode {
    match mode {
        PermissionMode::FullAccess if scheduled => PermissionMode::ApproveForMe,
        mode => mode,
    }
}

/// An action waiting for the user's decision, as announced by the worker.
#[derive(Clone, Debug)]
pub(super) struct PendingApproval {
    pub(super) name: String,
    pub(super) rule_key: Option<String>,
    pub(super) rule: Option<String>,
    pub(super) parent_id: Option<Uuid>,
    /// Request id of the first decision sent; later ones are duplicates.
    pub(super) decision: Option<Uuid>,
}

/// What a non-Run command sent to a worker was, so its reply can be routed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Control {
    Approve { action_id: Uuid },
    Cancel,
    Permissions,
    Other,
}

impl Control {
    fn of(command: &WorkerCommand) -> Self {
        match command {
            WorkerCommand::Approve { action_id, .. } => Control::Approve {
                action_id: *action_id,
            },
            WorkerCommand::Cancel => Control::Cancel,
            WorkerCommand::SetPermissions { .. } => Control::Permissions,
            _ => Control::Other,
        }
    }
}

/// Monotonic per-session sequence numbers for every event the host publishes,
/// whether forwarded from a worker or synthesized by the host.
#[derive(Clone, Default)]
pub(super) struct SessionSequences(Arc<parking_lot::Mutex<HashMap<Uuid, u64>>>);

impl SessionSequences {
    pub(super) fn next(&self, session: Uuid) -> u64 {
        let mut sequences = self.0.lock();
        let sequence = sequences.entry(session).or_insert(0);
        *sequence += 1;
        *sequence
    }

    /// A host event for `session` carrying the next sequence number.
    pub(super) fn engine(&self, request_id: Uuid, session: Uuid, event: EngineEvent) -> HostEvent {
        HostEvent::Engine(Envelope::new(
            request_id,
            Some(session),
            self.next(session),
            event,
        ))
    }
}

/// The host's handle on one worker process. Clones share the same state.
#[derive(Clone)]
pub(super) struct WorkerHandle {
    pub(super) credentials:
        Arc<parking_lot::Mutex<Option<Arc<credential_control::RunCredentials>>>>,
    pub(super) commands: mpsc::Sender<Envelope<WorkerCommand>>,
    pub(super) active: Arc<AtomicBool>,
    pub(super) queue_epoch: Arc<AtomicU64>,
    pub(super) queue_stopped: Arc<AtomicBool>,
    pub(super) lease: Arc<Mutex<Option<ResourceLease>>>,
    pub(super) stop: CancellationToken,
    pub(super) exited: tokio::sync::watch::Receiver<bool>,
    pub(super) run_request: Arc<parking_lot::Mutex<Option<Uuid>>>,
    pub(super) run_turn: Arc<parking_lot::Mutex<Option<Uuid>>>,
    /// Canonical project of the chat; standing approvals are saved here.
    pub(super) workspace: Arc<parking_lot::Mutex<Option<PathBuf>>>,
    pub(super) model_activity: Arc<Mutex<Option<ModelActivityLease>>>,
    pub(super) approvals: Arc<parking_lot::Mutex<HashMap<Uuid, PendingApproval>>>,
    controls: Arc<parking_lot::Mutex<VecDeque<(Uuid, Control)>>>,
    pub(super) last_used: Arc<parking_lot::Mutex<Instant>>,
    /// Permission mode last sent to the worker, after [`effective_mode`].
    pub(super) mode: Arc<parking_lot::Mutex<PermissionMode>>,
    /// Set when the host killed the process because it ignored Stop.
    pub(super) forced_stop: Arc<AtomicBool>,
    /// The run a Stop was already sent for; repeated Stops send nothing.
    stop_sent: Arc<parking_lot::Mutex<Option<Uuid>>>,
    /// Retire the process when its current turn ends (a standing approval of
    /// its project was revoked, or the vault key it holds was replaced).
    retire_after_run: Arc<AtomicBool>,
}

impl WorkerHandle {
    pub(super) fn new(
        commands: mpsc::Sender<Envelope<WorkerCommand>>,
        exited: tokio::sync::watch::Receiver<bool>,
    ) -> Self {
        Self {
            credentials: Arc::default(),
            commands,
            active: Arc::default(),
            queue_epoch: Arc::default(),
            queue_stopped: Arc::default(),
            lease: Arc::default(),
            stop: CancellationToken::new(),
            exited,
            run_request: Arc::default(),
            run_turn: Arc::default(),
            workspace: Arc::default(),
            model_activity: Arc::default(),
            approvals: Arc::default(),
            controls: Arc::default(),
            last_used: Arc::new(parking_lot::Mutex::new(Instant::now())),
            mode: Arc::default(),
            forced_stop: Arc::default(),
            stop_sent: Arc::default(),
            retire_after_run: Arc::default(),
        }
    }

    /// The process ended, is ending, or can no longer receive commands.
    pub(super) fn dead(&self) -> bool {
        self.stop.is_cancelled() || *self.exited.borrow() || self.commands.is_closed()
    }

    fn idle(&self) -> bool {
        !self.active.load(Ordering::Acquire) && !self.dead()
    }

    fn record_control(&self, request_id: Uuid, control: Control) {
        let mut controls = self.controls.lock();
        if controls.len() >= CONTROL_LEDGER_CAPACITY {
            controls.pop_front();
        }
        controls.push_back((request_id, control));
    }

    fn take_control(&self, request_id: Uuid) -> Option<Control> {
        let mut controls = self.controls.lock();
        let index = controls.iter().position(|(id, _)| *id == request_id)?;
        controls.remove(index).map(|(_, control)| control)
    }

    /// Sends a control command, remembering it so a reply can be routed.
    pub(super) async fn send_control(
        &self,
        session: Uuid,
        request_id: Uuid,
        command: WorkerCommand,
    ) -> Result<()> {
        self.record_control(request_id, Control::of(&command));
        self.commands
            .send(Envelope::new(request_id, Some(session), 0, command))
            .await
            .context("This task is no longer running")
    }

    /// Asks the process to exit and ends it. Idle workers have no turn to
    /// lose; the reaper still reconciles the journal before reporting exit.
    pub(super) fn retire(&self, session: Uuid) {
        let _ = self.commands.try_send(Envelope::new(
            Uuid::new_v4(),
            Some(session),
            0,
            WorkerCommand::Shutdown,
        ));
        self.stop.cancel();
    }
}

impl WorkerHandle {
    /// Retires an idle worker now and a busy one when its turn ends.
    pub(super) fn retire_when_idle(&self, session: Uuid) {
        if self.active.load(Ordering::Acquire) {
            self.retire_after_run.store(true, Ordering::Release);
        } else {
            self.retire(session);
        }
    }
}

/// Keeps a small warm pool of idle workers. Exited workers are dropped from the
/// map; idle ones older than [`IDLE_WORKER_TTL`] and the least recently used
/// beyond [`MAX_IDLE_WORKERS`] are retired. Retiring processes stay in the map
/// until reaped so quiescence (updates, shutdown) still sees them. Active
/// workers and `keep` are never touched.
pub(super) fn retire_idle_workers(
    workers: &mut HashMap<Uuid, WorkerHandle>,
    keep: Option<Uuid>,
    now: Instant,
) {
    workers.retain(|session, worker| Some(*session) == keep || !*worker.exited.borrow());
    let mut idle: Vec<_> = workers
        .iter()
        .filter(|(session, worker)| Some(**session) != keep && worker.idle())
        .map(|(session, worker)| (*worker.last_used.lock(), *session))
        .collect();
    // Most recently used first: the tail beyond the cap is retired.
    idle.sort_by_key(|(last_used, _)| std::cmp::Reverse(*last_used));
    for (index, (last_used, session)) in idle.into_iter().enumerate() {
        let expired = now.saturating_duration_since(last_used) >= IDLE_WORKER_TTL;
        if (index >= MAX_IDLE_WORKERS || expired)
            && let Some(worker) = workers.get(&session)
        {
            worker.retire(session);
        }
    }
}

/// Reads worker stdout until the reply to `hello`, checking the protocol
/// version of every line. Unsolicited `Ready` events are skipped.
pub(super) async fn handshake<R: AsyncBufRead + Unpin>(output: &mut R, hello: Uuid) -> Result<()> {
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = (&mut *output)
            .take((MAX_PROTOCOL_BYTES + 1) as u64)
            .read_until(b'\n', &mut line)
            .await
            .context("The task runner could not be read during startup")?;
        ensure!(
            read > 0,
            "The task runner exited during startup. Reinstall the complete application if this continues."
        );
        ensure!(
            line.len() <= MAX_PROTOCOL_BYTES,
            "The task runner sent an oversized startup message"
        );
        let value: Value = serde_json::from_slice(&line)
            .context("The task runner sent an unreadable startup message")?;
        let version = value.get("version").and_then(Value::as_u64);
        ensure!(
            version == Some(u64::from(PROTOCOL_VERSION)),
            "The task runner belongs to a different Xylo version (protocol {}, expected {PROTOCOL_VERSION}). Reinstall the complete application.",
            version.map_or_else(|| "unknown".to_owned(), |version| version.to_string())
        );
        let envelope: Envelope<EngineEvent> = serde_json::from_value(value)
            .context("The task runner sent an unreadable startup message")?;
        match envelope.payload {
            EngineEvent::Ready { .. } if envelope.request_id == hello => return Ok(()),
            EngineEvent::Error { message, .. } => {
                anyhow::bail!("The task runner could not start: {message}")
            }
            _ => {}
        }
    }
}

/// Removes bearer tokens, `sk-` keys and `api_key=` values before a worker
/// diagnostic line is logged.
pub(super) fn redact_secrets(line: &str) -> String {
    let mut output = String::with_capacity(line.len());
    let mut rest = line;
    while !rest.is_empty() {
        let boundary = !output.ends_with(|c: char| c.is_alphanumeric() || c == '_');
        if boundary && let Some(length) = secret_prefix(rest) {
            output.push_str(&rest[..length]);
            output.push_str("[REDACTED]");
            rest = &rest[length..];
            let end = rest
                .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '&' | ',' | ';'))
                .unwrap_or(rest.len());
            rest = &rest[end..];
        } else {
            let mut chars = rest.chars();
            if let Some(c) = chars.next() {
                output.push(c);
            }
            rest = chars.as_str();
        }
    }
    output
}

fn starts_with_ignore_case(text: &str, prefix: &str) -> bool {
    text.as_bytes()
        .get(..prefix.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(prefix.as_bytes()))
}

/// Length of a secret-introducing prefix at the start of `text`, if any.
fn secret_prefix(text: &str) -> Option<usize> {
    if starts_with_ignore_case(text, "bearer ") {
        let value = text["bearer ".len()..].trim_start_matches(' ');
        return (!value.is_empty()).then_some(text.len() - value.len());
    }
    if text.starts_with("sk-")
        && text
            .as_bytes()
            .get(3)
            .is_some_and(u8::is_ascii_alphanumeric)
    {
        return Some(3);
    }
    for key in ["api_key", "api-key", "apikey"] {
        if starts_with_ignore_case(text, key) {
            let after = &text[key.len()..];
            let value = after.trim_start_matches([' ', '=', ':', '"', '\'']);
            let separator = &after[..after.len() - value.len()];
            if separator.contains(['=', ':']) && !value.is_empty() {
                return Some(text.len() - value.len());
            }
        }
    }
    None
}

impl HostState {
    /// Starts the application's worker binary for `session`.
    pub(super) async fn spawn_worker(self: &Arc<Self>, session: Uuid) -> Result<WorkerHandle> {
        let mut command = tokio::process::Command::new(native_executable("xylo-worker")?);
        #[cfg(windows)]
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: worker uses private pipes.
        command.arg("--data-dir").arg(&self.paths.data);
        self.start_worker(session, command).await
    }

    /// Starts `command` as the worker for `session`, unlocks the vault in it
    /// and completes the protocol handshake before returning.
    pub(super) async fn start_worker(
        self: &Arc<Self>,
        session: Uuid,
        mut command: tokio::process::Command,
    ) -> Result<WorkerHandle> {
        let key = {
            let vault = self.vault()?;
            tokio::task::spawn_blocking(move || vault.export_key()).await??
        };
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("The task runner could not start")?;
        let input = child.stdin.take().context("Missing worker stdin")?;
        let output = child.stdout.take().context("Missing worker stdout")?;
        let errors = child.stderr.take().context("Missing worker stderr")?;
        let (tx, rx) = mpsc::channel::<Envelope<WorkerCommand>>(64);
        let (exited_tx, exited) = tokio::sync::watch::channel(false);
        let worker = WorkerHandle::new(tx, exited);
        tokio::spawn(write_commands(input, rx, worker.stop.clone()));
        tokio::spawn(log_diagnostics(errors, session));
        let hello = Uuid::new_v4();
        for (request_id, command) in [
            (
                Uuid::new_v4(),
                WorkerCommand::Unlock {
                    key: WorkerCredential::from(&key),
                },
            ),
            (hello, WorkerCommand::Hello),
        ] {
            worker
                .commands
                .try_send(Envelope::new(request_id, Some(session), 0, command))
                .map_err(|_| anyhow::anyhow!("The task runner could not start"))?;
        }
        let mut output = BufReader::new(output);
        let handshake = tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake(&mut output, hello))
            .await
            .unwrap_or_else(|_| {
                Err(anyhow::anyhow!(
                    "The task runner did not respond during startup"
                ))
            });
        if let Err(error) = handshake {
            worker.stop.cancel();
            let _ = child.kill().await;
            return Err(error);
        }
        self.turn_queue.register(session, &worker);
        let reader = WorkerReader {
            session,
            worker: worker.clone(),
            host: Arc::downgrade(self),
            events: self.events.clone(),
            sequences: self.sequences.clone(),
            store: self.store.clone(),
            notices: self.notices.clone(),
            foreground: self.foreground.clone(),
            scheduler: self.default_scheduler()?,
            scheduled: self.scheduled.clone(),
            accounts: self.accounts.clone(),
            vault: self.vault()?,
            renewal_capacity: Arc::new(tokio::sync::Semaphore::new(4)),
        };
        tokio::spawn(reader.run(output, child, exited_tx));
        Ok(worker)
    }

    /// Sends Stop to the active run of `session` once, and ends the process
    /// when Stop cannot be delivered or the run does not reach a terminal
    /// event within [`STOP_GRACE`].
    pub(super) async fn stop_active_run(&self, session: Uuid) {
        let worker = {
            let workers = self.workers.lock().await;
            match workers.get(&session) {
                Some(worker) if worker.active.load(Ordering::Acquire) => worker.clone(),
                _ => return,
            }
        };
        let Some(request) = *worker.run_request.lock() else {
            return;
        };
        if worker.stop_sent.lock().replace(request) == Some(request) {
            return;
        }
        if let Some(credentials) = worker.credentials.lock().as_ref() {
            credentials.cancel.cancel();
        }
        tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + STOP_GRACE;
            let delivered = tokio::time::timeout_at(
                deadline,
                worker.send_control(session, Uuid::new_v4(), WorkerCommand::Cancel),
            )
            .await;
            if matches!(delivered, Ok(Ok(()))) {
                while tokio::time::Instant::now() < deadline {
                    if *worker.run_request.lock() != Some(request) || *worker.exited.borrow() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
            if *worker.run_request.lock() == Some(request) && !*worker.exited.borrow() {
                tracing::warn!(%session, "Task did not honor Stop; ending its process");
                worker.forced_stop.store(true, Ordering::Release);
                worker.stop.cancel();
            }
        });
    }

    /// Sends the current permission mode to every live worker. Running turns
    /// apply it at their next tool call.
    pub(super) async fn forward_permissions(&self, setting: PermissionMode) {
        let scheduled: std::collections::HashSet<Uuid> =
            self.scheduled.lock().await.keys().copied().collect();
        let workers: Vec<_> = self
            .workers
            .lock()
            .await
            .iter()
            .filter(|(_, worker)| !worker.dead())
            .map(|(session, worker)| (*session, worker.clone()))
            .collect();
        for (session, worker) in workers {
            let mode = effective_mode(setting, scheduled.contains(&session));
            *worker.mode.lock() = mode;
            let delivered = tokio::time::timeout(
                Duration::from_secs(2),
                worker.send_control(
                    session,
                    Uuid::new_v4(),
                    WorkerCommand::SetPermissions { mode },
                ),
            )
            .await;
            if !matches!(delivered, Ok(Ok(()))) && worker.active.load(Ordering::Acquire) {
                let _ = self
                    .events
                    .send(HostEvent::Error(
                        "A running task could not receive the new permission mode. Stop it to apply the change."
                            .into(),
                    ))
                    .await;
            }
        }
    }

    /// Every live worker was unlocked with a vault key that no longer opens
    /// the vault. Idle ones are retired now, busy ones when their turn ends,
    /// so the next turn starts a worker that receives the new key.
    pub(super) async fn retire_workers_for_new_key(&self) {
        for (session, worker) in self.workers.lock().await.iter() {
            worker.retire_when_idle(*session);
        }
    }

    /// Settles an approval card whose decision cannot reach a worker.
    pub(super) async fn report_lost_decision(
        &self,
        session: Uuid,
        action_id: Uuid,
        pending: Option<PendingApproval>,
    ) -> Result<()> {
        let pending = pending.unwrap_or(PendingApproval {
            name: "approval".into(),
            rule_key: None,
            rule: None,
            parent_id: None,
            decision: None,
        });
        self.events
            .send(self.sequences.engine(
                Uuid::new_v4(),
                session,
                EngineEvent::ToolFinished {
                    action_id,
                    name: pending.name,
                    result: LOST_DECISION.into(),
                    success: false,
                    parent_id: pending.parent_id,
                },
            ))
            .await?;
        Ok(())
    }
}

async fn write_commands(
    input: tokio::process::ChildStdin,
    mut commands: mpsc::Receiver<Envelope<WorkerCommand>>,
    stop: CancellationToken,
) {
    let mut input = BufWriter::new(input);
    loop {
        let command = tokio::select! {
            _ = stop.cancelled() => break,
            command = commands.recv() => command,
        };
        let Some(command) = command else {
            break;
        };
        let shutdown = matches!(command.payload, WorkerCommand::Shutdown);
        let Ok(mut bytes) = serde_json::to_vec(&command) else {
            break;
        };
        bytes.push(b'\n');
        if input.write_all(&bytes).await.is_err() || input.flush().await.is_err() {
            break;
        }
        if shutdown {
            break;
        }
    }
}

async fn log_diagnostics(errors: tokio::process::ChildStderr, session: Uuid) {
    let mut errors = BufReader::new(errors);
    let mut line = Vec::new();
    loop {
        line.clear();
        match (&mut errors)
            .take(16 * 1024)
            .read_until(b'\n', &mut line)
            .await
        {
            Ok(0) | Err(_) => break,
            _ => {
                let message = redact_secrets(&String::from_utf8_lossy(&line));
                tracing::debug!(worker = %session, %message, "native worker");
            }
        }
    }
}

/// Forwards one worker's stdout to the UI and reaps the process.
struct WorkerReader {
    session: Uuid,
    worker: WorkerHandle,
    host: std::sync::Weak<HostState>,
    events: Sender<HostEvent>,
    sequences: SessionSequences,
    store: Store,
    notices: Sender<crate::notifications::NoticeEvent>,
    foreground: Arc<AtomicBool>,
    scheduler: crate::scheduler::Scheduler,
    scheduled: Arc<Mutex<HashMap<Uuid, Uuid>>>,
    accounts: xylo_services::providers::ChatGptSignInService,
    vault: CredentialVault,
    renewal_capacity: Arc<tokio::sync::Semaphore>,
}

impl WorkerReader {
    async fn run(
        self,
        mut output: BufReader<ChildStdout>,
        mut child: tokio::process::Child,
        exited: tokio::sync::watch::Sender<bool>,
    ) {
        let mut line = Vec::new();
        loop {
            line.clear();
            let mut limited = (&mut output).take((MAX_PROTOCOL_BYTES + 1) as u64);
            let read = tokio::select! {
                _ = self.worker.stop.cancelled() => break,
                read = limited.read_until(b'\n', &mut line) => read,
            };
            if matches!(read, Ok(0) | Err(_)) {
                break;
            }
            if line.len() > MAX_PROTOCOL_BYTES {
                let _ = self
                    .events
                    .send(HostEvent::Error(
                        "Worker exceeded protocol payload limit".into(),
                    ))
                    .await;
                break;
            }
            let envelope = match serde_json::from_slice::<Envelope<EngineEvent>>(&line) {
                Ok(envelope) if envelope.validate().is_ok() => envelope,
                _ => {
                    let _ = self
                        .events
                        .send(HostEvent::Error("Invalid native worker message".into()))
                        .await;
                    break;
                }
            };
            if !self.handle(envelope).await {
                break;
            }
        }
        self.reap(&mut child).await;
        let _ = exited.send(true);
    }

    /// Returns false when the UI channel closed.
    async fn handle(&self, mut envelope: Envelope<EngineEvent>) -> bool {
        envelope.session_id = Some(self.session);
        match &envelope.payload {
            EngineEvent::Steering { receipt } => {
                if let Err(error) = steering_control::emit_receipt(
                    self.store.clone(),
                    &self.events,
                    self.session,
                    receipt.clone(),
                )
                .await
                {
                    tracing::warn!(%error, "Read durable guidance receipt");
                }
                return true;
            }
            // The handshake consumed the reply to Hello; later Ready events
            // carry no information for the UI.
            EngineEvent::Ready { .. } => return true,
            // A generated title follows its run's terminal event, so it is
            // applied whether or not that run is still current.
            EngineEvent::TitleUpdated { .. } => return self.publish_title().await,
            _ => {}
        }
        let active_request = *self.worker.run_request.lock();
        if active_request != Some(envelope.request_id) {
            return self.route_reply(envelope, active_request).await;
        }
        match &envelope.payload {
            EngineEvent::ProviderCredentialRequested {
                request_id,
                generation,
                force,
            } => {
                self.renew_credentials(envelope.request_id, *request_id, *generation, *force);
                return true;
            }
            EngineEvent::ApprovalRequired {
                action_id,
                name,
                rule,
                rule_key,
                parent_id,
                ..
            } => {
                self.worker.approvals.lock().insert(
                    *action_id,
                    PendingApproval {
                        name: name.clone(),
                        rule_key: rule_key.clone(),
                        rule: rule.clone(),
                        parent_id: *parent_id,
                        decision: None,
                    },
                );
                if self.is_scheduled().await {
                    let mode = self.worker.mode.lock().label();
                    let notice = format!(
                        "A scheduled task is waiting for your approval in its chat (mode: {mode})"
                    );
                    if self.events.send(HostEvent::Status(notice)).await.is_err() {
                        return false;
                    }
                }
            }
            EngineEvent::ToolFinished { action_id, .. } => {
                self.worker.approvals.lock().remove(action_id);
            }
            _ => {}
        }
        self.forward_run_event(envelope).await
    }

    /// Whether this session is a scheduled occurrence. The guard is released
    /// before returning, so no event send ever waits while holding it.
    async fn is_scheduled(&self) -> bool {
        self.scheduled.lock().await.contains_key(&self.session)
    }

    async fn forward_run_event(&self, mut envelope: Envelope<EngineEvent>) -> bool {
        let terminal = matches!(
            envelope.payload,
            EngineEvent::Completed { .. } | EngineEvent::Cancelled | EngineEvent::Error { .. }
        );
        let scheduled = terminal && self.is_scheduled().await;
        if matches!(envelope.payload, EngineEvent::Completed { .. })
            && let Err(error) =
                acknowledge_schedule(&self.scheduler, &self.scheduled, self.session).await
        {
            tracing::error!(%error, "Acknowledge completed schedule");
        }
        let current_turn = *self.worker.run_turn.lock();
        if let Some(turn_id) = current_turn {
            notice_control::emit(
                &self.notices,
                &self.foreground,
                self.session,
                turn_id,
                &envelope.payload,
                scheduled,
            );
        }
        let terminal_epoch = self.worker.queue_epoch.load(Ordering::Acquire);
        if terminal {
            if let Some(credentials) = self.worker.credentials.lock().take() {
                credentials.cancel.cancel();
            }
            if let Err(error) = steering_control::finish_turn(
                self.store.clone(),
                &self.events,
                self.session,
                current_turn,
            )
            .await
            {
                tracing::warn!(%error, "Settle ended task guidance");
            }
        }
        let successful = matches!(envelope.payload, EngineEvent::Completed { .. });
        envelope.sequence = self.sequences.next(self.session);
        if self.events.send(HostEvent::Engine(envelope)).await.is_err() {
            return false;
        }
        if terminal {
            // Only now may a new Send be admitted: the UI has the terminal event.
            self.clear_run();
            self.worker.lease.lock().await.take();
            self.worker.model_activity.lock().await.take();
            if let Some(host) = self.host.upgrade() {
                tokio::spawn(host.settle_turn_queue(
                    self.session,
                    successful,
                    self.worker.queue_epoch.clone(),
                    terminal_epoch,
                    self.worker.queue_stopped.clone(),
                ));
            }
        }
        true
    }

    fn clear_run(&self) {
        self.worker.run_turn.lock().take();
        self.worker.run_request.lock().take();
        self.worker.approvals.lock().clear();
        *self.worker.last_used.lock() = Instant::now();
        self.worker.active.store(false, Ordering::Release);
        if self.worker.retire_after_run.load(Ordering::Acquire) {
            self.worker.retire(self.session);
        }
    }

    /// Replies to control commands and events of finished runs.
    async fn route_reply(&self, envelope: Envelope<EngineEvent>, active: Option<Uuid>) -> bool {
        let control = self.worker.take_control(envelope.request_id);
        let event = match (control, envelope.payload) {
            (Some(Control::Approve { action_id }), EngineEvent::Error { message, .. }) => {
                // Only a failed first decision leaves the card waiting.
                let failed = {
                    let mut approvals = self.worker.approvals.lock();
                    let first = approvals
                        .get(&action_id)
                        .is_some_and(|pending| pending.decision == Some(envelope.request_id));
                    first.then(|| approvals.remove(&action_id)).flatten()
                };
                match failed {
                    Some(pending) => self.sequences.engine(
                        envelope.request_id,
                        self.session,
                        EngineEvent::ToolFinished {
                            action_id,
                            name: pending.name,
                            result: format!("Your decision was not applied: {message}"),
                            success: false,
                            parent_id: pending.parent_id,
                        },
                    ),
                    None => HostEvent::Status(format!("Your decision was not applied: {message}")),
                }
            }
            (Some(Control::Cancel), payload @ EngineEvent::Status { .. }) => {
                self.sequences
                    .engine(envelope.request_id, self.session, payload)
            }
            (Some(_), EngineEvent::Error { message, .. }) => HostEvent::Error(message),
            (None, EngineEvent::Error { message, .. }) if active.is_none() => {
                HostEvent::Error(message)
            }
            _ => return true,
        };
        self.events.send(event).await.is_ok()
    }

    /// The engine stored a generated title; publish the stored conversation.
    async fn publish_title(&self) -> bool {
        let store = self.store.clone();
        let session = self.session;
        match tokio::task::spawn_blocking(move || store.conversation(session)).await {
            Ok(Ok(Some(conversation))) => self
                .events
                .send(HostEvent::ConversationRenamed(conversation))
                .await
                .is_ok(),
            result => {
                tracing::debug!(?result, "Read generated conversation title");
                true
            }
        }
    }

    fn renew_credentials(&self, run: Uuid, request_id: Uuid, generation: u64, force: bool) {
        let Some(renewal) = self.worker.credentials.lock().clone() else {
            return;
        };
        let commands = self.worker.commands.clone();
        let accounts = self.accounts.clone();
        let vault = self.vault.clone();
        let session = self.session;
        let permit = self.renewal_capacity.clone().try_acquire_owned();
        tokio::spawn(async move {
            let result = match permit {
                Ok(_permit) => renewal
                    .resolve(run, generation, force, accounts, vault)
                    .await
                    .map_err(|_| {
                        "ChatGPT access could not be renewed. Reconnect the account in Settings → Providers."
                            .to_string()
                    }),
                Err(_) => Err("Too many pending credential requests".into()),
            };
            tracing::debug!(%session, %run, %request_id, force, success = result.is_ok(), "Provider credential renewal finished");
            let envelope = Envelope::new(
                run,
                Some(session),
                0,
                WorkerCommand::ProviderCredentialUpdated { request_id, result },
            );
            tokio::select! {
                biased;
                _ = renewal.cancel.cancelled() => {},
                _ = tokio::time::timeout(Duration::from_secs(5), commands.send(envelope)) => {},
            }
        });
    }

    /// Ends the process, reconciles its journal and reports a run that never
    /// reached a terminal event. Only such a run settles the queue as failed:
    /// retiring an idle worker ran nothing, and a finished run already
    /// settled the queue at its terminal event.
    async fn reap(&self, child: &mut tokio::process::Child) {
        let _ = child.kill().await;
        let _ = child.wait().await;
        let unfinished = self.worker.active.swap(false, Ordering::AcqRel);
        let session = self.session;
        let ended_turn = *self.worker.run_turn.lock();
        if let Err(error) =
            steering_control::finish_turn(self.store.clone(), &self.events, session, ended_turn)
                .await
        {
            tracing::warn!(%error, "Recover reaped task guidance");
        }
        let store = self.store.clone();
        let recovered =
            tokio::task::spawn_blocking(move || store.recover_session_interrupted(session)).await;
        if !matches!(recovered, Ok(Ok(_))) {
            tracing::error!(?recovered, %session, "Recover reaped task journal");
        }
        if unfinished && let Some(host) = self.host.upgrade() {
            tokio::spawn(host.settle_turn_queue(
                session,
                false,
                self.worker.queue_epoch.clone(),
                self.worker.queue_epoch.load(Ordering::Acquire),
                self.worker.queue_stopped.clone(),
            ));
        }
        self.worker.stop.cancel();
        if let Some(credentials) = self.worker.credentials.lock().take() {
            credentials.cancel.cancel();
        }
        let crashed_turn = self.worker.run_turn.lock().take();
        let request = self.worker.run_request.lock().take();
        self.worker.approvals.lock().clear();
        if unfinished {
            self.report_unfinished_run(crashed_turn, request.unwrap_or_else(Uuid::new_v4))
                .await;
        }
        self.worker.lease.lock().await.take();
        self.worker.model_activity.lock().await.take();
    }

    async fn report_unfinished_run(&self, turn: Option<Uuid>, request: Uuid) {
        let forced = self.worker.forced_stop.load(Ordering::Acquire);
        let event = if forced {
            EngineEvent::Cancelled
        } else {
            EngineEvent::Error {
                message: "This task stopped unexpectedly. Resume the chat to continue.".into(),
                retryable: true,
                kind: ErrorKind::Internal,
            }
        };
        if let Some(turn) = turn {
            let scheduled = self.is_scheduled().await;
            notice_control::emit(
                &self.notices,
                &self.foreground,
                self.session,
                turn,
                &event,
                scheduled,
            );
        }
        let _ = self
            .events
            .send(self.sequences.engine(request, self.session, event))
            .await;
        if forced {
            let _ = self
                .events
                .send(HostEvent::Status(FORCED_STOP_NOTICE.into()))
                .await;
        }
    }
}

#[cfg(test)]
mod tests;
