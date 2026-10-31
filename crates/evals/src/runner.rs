//! Runs one trial of a task against the real engine.
//!
//! Each trial gets a fresh temporary profile (database, vault, artifacts), a
//! fresh workspace built from the task's fixtures and a new conversation, so
//! trials never share state. The engine runs exactly as the worker runs it,
//! with one difference: its provider passes through a [`RecordingProvider`],
//! and offline trials substitute a [`ScriptedProvider`] for the network one.
//! Approval requests are answered by the task's [`ApprovalPolicy`].

use crate::{
    invariants,
    matrix::LiveModel,
    metrics::TrialMetrics,
    recorder::{ExchangeLog, RecordingProvider},
    scripted::Script,
    scripted::ScriptedProvider,
    task::{ApprovalPolicy, Check, OUTSIDE, PROFILE, Task, WORKSPACE},
    trace::{ModelLabel, RecordedAction, TRACE_VERSION, Trace},
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use xylo_core::{
    ApiFamily, Conversation, EngineBackend, EngineEvent, ProviderConfig, TitleSource, TurnRequest,
    WorkerCredential, now_ms,
};
use xylo_engine::{
    AgentEngine, AppPaths, ModelProvider, ProviderLayer,
    tools::{PermissionCell, ToolSession},
};
use xylo_services::{CredentialVault, Store, VaultKey};

/// Where a trial's model replies come from.
#[derive(Clone)]
pub enum ModelSource {
    /// The task's script, offline and deterministic.
    Scripted,
    /// A configured real provider (opt-in; see [`crate::matrix`]).
    Live(Box<LiveModel>),
}

impl ModelSource {
    /// The label reports and traces use for this source.
    pub fn label(&self) -> ModelLabel {
        match self {
            Self::Scripted => ModelLabel {
                label: "scripted".into(),
                model: "scripted".into(),
            },
            Self::Live(model) => ModelLabel {
                label: model.label.clone(),
                model: model.provider.model.clone(),
            },
        }
    }
}

/// One approval request and the simulated user's answer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApprovalDecision {
    pub tool: String,
    pub arguments: Value,
    pub allowed: bool,
}

/// A finished trial, with its workspace still on disk for grading.
pub struct Trial {
    pub task: String,
    pub trial: u32,
    /// Whether a real model produced the replies.
    pub live: bool,
    /// The trial's project directory, still on disk.
    pub workspace: PathBuf,
    /// The trial's directory outside the project.
    pub outside: PathBuf,
    /// The trial's record, with paths in placeholder form.
    pub trace: Trace,
    pub approvals: Vec<ApprovalDecision>,
    /// Active memory records of the project after the turn, as JSON.
    pub memories: Vec<Value>,
    /// Journal actions still pending, running or of unknown outcome.
    pub unresolved_actions: usize,
    /// The trial hit its deadline and was cancelled.
    pub timed_out: bool,
    /// After cancellation the engine did not end its event stream within
    /// [`CANCEL_GRACE`]; the trial was abandoned.
    pub hung: bool,
    /// The engine returned an error instead of a terminal event.
    pub engine_error: Option<String>,
    _directory: tempfile::TempDir,
}

impl Trial {
    /// The turn's terminal event, if it sent one.
    pub fn terminal(&self) -> Option<&EngineEvent> {
        self.trace.events.iter().find(|event| {
            matches!(
                event,
                EngineEvent::Completed { .. } | EngineEvent::Cancelled | EngineEvent::Error { .. }
            )
        })
    }

    /// The final answer of a completed turn.
    pub fn answer(&self) -> Option<&str> {
        match self.terminal()? {
            EngineEvent::Completed { text, .. } => Some(text),
            _ => None,
        }
    }

    /// A workspace file's text, if it exists.
    pub fn file(&self, relative: &str) -> Option<String> {
        std::fs::read_to_string(self.workspace.join(relative)).ok()
    }

    /// Journal records of the actions named `tool`.
    pub fn actions<'a>(&'a self, tool: &'a str) -> impl Iterator<Item = &'a RecordedAction> {
        self.trace
            .actions
            .iter()
            .filter(move |action| action.name == tool)
    }

    /// A short description of how the turn ended.
    pub fn outcome(&self) -> String {
        if self.hung {
            return "hung after cancellation".into();
        }
        if self.timed_out {
            return "timed out".into();
        }
        match self.terminal() {
            Some(EngineEvent::Completed { .. }) => "completed".into(),
            Some(EngineEvent::Cancelled) => "cancelled".into(),
            Some(EngineEvent::Error { kind, message, .. }) => {
                format!("error ({kind:?}): {}", first_line(message))
            }
            _ => match &self.engine_error {
                Some(error) => format!("engine error: {}", first_line(error)),
                None => "no terminal event".into(),
            },
        }
    }
}

impl Trial {
    /// A trial made of `trace` alone, for unit tests of graders.
    #[cfg(test)]
    pub(crate) fn from_trace(trace: Trace) -> Self {
        let directory = tempfile::tempdir().expect("create test trial directory");
        Self {
            task: trace.task.clone(),
            trial: trace.trial,
            live: false,
            workspace: directory.path().join("workspace"),
            outside: directory.path().join("outside"),
            trace,
            approvals: Vec::new(),
            memories: Vec::new(),
            unresolved_actions: 0,
            timed_out: false,
            hung: false,
            engine_error: None,
            _directory: directory,
        }
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

/// A graded trial, ready for a report.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrialResult {
    pub task: String,
    pub trial: u32,
    pub model: ModelLabel,
    pub outcome: String,
    /// Every task check passed or was skipped.
    pub task_passed: bool,
    /// Every harness invariant held or was skipped.
    pub invariants_held: bool,
    /// Both of the above: the trial counts as a success.
    pub passed: bool,
    pub checks: Vec<Check>,
    pub invariants: Vec<Check>,
    pub metrics: TrialMetrics,
    /// Where the trace was written, when traces are kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<PathBuf>,
}

impl TrialResult {
    /// Grades `trial` with its task's checks and the harness invariants.
    pub fn grade(task: &Task, trial: &Trial) -> Self {
        let checks = (task.grade)(trial);
        let invariants = invariants::check(trial);
        let task_passed = !checks.iter().any(Check::failed);
        let invariants_held = !invariants.iter().any(Check::failed);
        Self {
            task: trial.task.clone(),
            trial: trial.trial,
            model: trial.trace.model.clone(),
            outcome: trial.outcome(),
            task_passed,
            invariants_held,
            passed: task_passed && invariants_held,
            checks,
            invariants,
            metrics: TrialMetrics::of(trial),
            trace: None,
        }
    }
}

/// Runs one trial of `task`. `deadline` bounds the whole turn; a trial that
/// passes it is cancelled and reported as timed out.
pub async fn run_trial(
    task: &Task,
    source: &ModelSource,
    trial: u32,
    deadline: Duration,
) -> Result<Trial> {
    run_trial_with(task, source, trial, deadline, None).await
}

/// How long a cancelled trial may take to end its event stream before it
/// is reported as hung.
pub const CANCEL_GRACE: Duration = Duration::from_secs(10);

/// What a replay substitutes for the task's own setup.
pub(crate) struct Replay {
    /// The recorded replies, in placeholder form.
    pub script: Script,
    /// The provider configuration the recording ran with.
    pub provider: ProviderConfig,
}

/// [`run_trial`], or with `replay`, a scripted run of the recorded replies
/// under the recorded provider configuration.
pub(crate) async fn run_trial_with(
    task: &Task,
    source: &ModelSource,
    trial: u32,
    deadline: Duration,
    replay: Option<Replay>,
) -> Result<Trial> {
    let directory = tempfile::Builder::new()
        .prefix("xylo-eval-")
        .tempdir()
        .context("Create trial directory")?;
    // Canonical paths keep the engine's project-boundary checks honest on
    // systems whose temporary directory is behind a symlink.
    let root = directory.path().canonicalize()?;
    let places = TrialPaths {
        workspace: root.join("workspace"),
        outside: root.join("outside"),
        profile: root.join("profile"),
    };
    places.write_fixtures(&places.workspace, &task.files)?;
    places.write_fixtures(&places.outside, &task.outside_files)?;
    let workspace = places.workspace.clone();

    let paths = AppPaths::at(places.profile.clone())?;
    let store = Store::open(&paths.database)?;
    let session = Uuid::new_v4();
    let prompt = places.resolve(&task.prompt);
    store.upsert_conversation(&Conversation {
        id: session,
        title: prompt.chars().take(40).collect(),
        workspace: workspace.clone(),
        updated_at: now_ms(),
        title_source: TitleSource::Prompt,
        pinned: false,
        archived: false,
    })?;
    let request = TurnRequest {
        session_id: session,
        turn_id: Uuid::new_v4(),
        prompt: prompt.clone(),
        workspace: workspace.clone(),
        provider: match &replay {
            Some(replay) => replay.provider.clone(),
            None => provider_config(task, source),
        },
        permissions: task.mode,
        attachments: Vec::new(),
        approved_rules: Vec::new(),
    };
    store.register_user_turn(&request)?;

    let tools = ToolSession::new(
        PermissionCell::new(task.mode),
        CredentialVault::unlocked(&paths.data, VaultKey::from_bytes(random_key()))?,
    );
    let log = ExchangeLog::default();
    let script = places.resolve_json(
        replay
            .as_ref()
            .map_or(&task.script, |replay| &replay.script),
    )?;
    let served_window = script.served_window;
    let provider = crate::trace::sanitized(&request.provider);
    // Offline, the engine still builds its network provider before the
    // layer replaces it, and a remote endpoint needs a credential to build.
    // This placeholder is never sent: the scripted layer has no network.
    let credential = match source {
        ModelSource::Scripted => Some("scripted-offline".to_owned()),
        ModelSource::Live(model) => model.credential.clone(),
    };
    let engine = AgentEngine::new(store.clone(), paths, tools.clone())
        .with_provider_layer(provider_layer(source, script, log.clone()))
        .with_provider_credential(WorkerCredential(credential));

    let (events, mut incoming) = mpsc::channel(256);
    let cancel = CancellationToken::new();
    let running = tokio::spawn({
        let cancel = cancel.clone();
        async move { engine.run(request, events, cancel).await }
    });
    let mut recorded = Vec::new();
    let mut approvals = Vec::new();
    let ending = collect_events(&mut incoming, deadline, CANCEL_GRACE, &cancel, |event| {
        if let EngineEvent::ApprovalRequired {
            action_id,
            name,
            arguments,
            ..
        } = &event
        {
            approvals.push(answer(&tools, &task.approvals, *action_id, name, arguments));
        }
        recorded.push(event);
    })
    .await;
    let engine_error = if ending.hung {
        running.abort();
        Some("The engine did not stop after cancellation".to_owned())
    } else {
        match running.await {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(format!("{error:#}")),
            Err(error) => Some(format!("Engine task stopped: {error}")),
        }
    };

    let actions = journal_actions(&store, &recorded)?;
    let memories = store
        .memory_page(&workspace.to_string_lossy(), None, 100)
        .map(|page| {
            page.records
                .iter()
                .filter_map(|record| serde_json::to_value(record).ok())
                .collect()
        })
        .unwrap_or_default();
    let checkpoint = store
        .checkpoint(session)?
        .map(|checkpoint| checkpoint.messages);
    let unresolved_actions = store.unresolved_actions(session)?.len();
    let exchanges = std::mem::take(&mut *log.lock());
    let trace = places.portable_json(&Trace {
        version: TRACE_VERSION,
        task: task.id.into(),
        trial,
        model: source.label(),
        provider,
        served_window,
        prompt,
        exchanges,
        events: recorded,
        actions,
        checkpoint,
    })?;
    Ok(Trial {
        task: task.id.into(),
        trial,
        live: matches!(source, ModelSource::Live(_)),
        workspace,
        outside: places.outside,
        trace,
        approvals,
        memories,
        unresolved_actions,
        timed_out: ending.timed_out,
        hung: ending.hung,
        engine_error,
        _directory: directory,
    })
}

/// How a trial's event stream ended.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Ending {
    /// The deadline passed and the turn was cancelled.
    pub timed_out: bool,
    /// The stream stayed open for `grace` after the cancellation.
    pub hung: bool,
}

/// Feeds every event to `each` until the engine closes the stream. At
/// `deadline` the turn is cancelled; a stream still open `grace` later is
/// abandoned as hung, so a stuck engine never stalls a suite.
pub(crate) async fn collect_events(
    incoming: &mut mpsc::Receiver<EngineEvent>,
    deadline: Duration,
    grace: Duration,
    cancel: &CancellationToken,
    mut each: impl FnMut(EngineEvent),
) -> Ending {
    let mut ending = Ending::default();
    let timer = tokio::time::sleep(deadline);
    tokio::pin!(timer);
    loop {
        tokio::select! {
            event = incoming.recv() => match event {
                Some(event) => each(event),
                None => return ending,
            },
            () = &mut timer => {
                if ending.timed_out {
                    ending.hung = true;
                    return ending;
                }
                ending.timed_out = true;
                cancel.cancel();
                timer.as_mut().reset(tokio::time::Instant::now() + grace);
            }
        }
    }
}

/// The provider configuration of a trial. Offline trials name an HTTPS
/// endpoint on a reserved domain: nothing is ever sent there, and a
/// non-loopback endpoint keeps the engine's local-model admission out of the
/// measurement.
fn provider_config(task: &Task, source: &ModelSource) -> ProviderConfig {
    match source {
        ModelSource::Scripted => ProviderConfig {
            name: "Scripted".into(),
            family: ApiFamily::OpenAiChat,
            endpoint: "https://scripted.invalid/v1".into(),
            model: "scripted".into(),
            credential_ref: None,
            context_tokens: task.context_tokens,
            output_tokens: task.output_tokens,
            timeout_seconds: 60,
            ..ProviderConfig::default()
        },
        ModelSource::Live(model) => model.provider.clone(),
    }
}

/// The layer that records traffic and, offline, replaces the network model.
fn provider_layer(
    source: &ModelSource,
    script: crate::scripted::Script,
    log: ExchangeLog,
) -> ProviderLayer {
    match source {
        ModelSource::Scripted => Arc::new(move |_network: Arc<dyn ModelProvider>| {
            let scripted: Arc<dyn ModelProvider> = Arc::new(ScriptedProvider::new(script.clone()));
            Arc::new(RecordingProvider::new(scripted, log.clone())) as Arc<dyn ModelProvider>
        }),
        ModelSource::Live(_) => Arc::new(move |network: Arc<dyn ModelProvider>| {
            Arc::new(RecordingProvider::new(network, log.clone())) as Arc<dyn ModelProvider>
        }),
    }
}

fn answer(
    tools: &ToolSession,
    policy: &ApprovalPolicy,
    action: Uuid,
    tool: &str,
    arguments: &Value,
) -> ApprovalDecision {
    let allowed = policy.allows(tool);
    // A decision for an action that already stopped is not an error here:
    // the trace still shows what was asked.
    let _ = tools.approvals().decide_with(action, allowed, false);
    ApprovalDecision {
        tool: tool.into(),
        arguments: arguments.clone(),
        allowed,
    }
}

/// The journal records of every action the events mention, in order.
fn journal_actions(store: &Store, events: &[EngineEvent]) -> Result<Vec<RecordedAction>> {
    let mut seen = std::collections::HashSet::new();
    let mut actions = Vec::new();
    for event in events {
        let (id, name, arguments) = match event {
            EngineEvent::ToolStarted {
                action_id,
                name,
                arguments,
                ..
            }
            | EngineEvent::ApprovalRequired {
                action_id,
                name,
                arguments,
                ..
            } => (*action_id, name, arguments),
            _ => continue,
        };
        if !seen.insert(id) {
            continue;
        }
        actions.push(match store.action(id)? {
            Some(action) => RecordedAction {
                id,
                name: action.intent.name,
                arguments: action.intent.arguments,
                state: action.state,
                result: action.result,
                error: action.error,
            },
            None => RecordedAction {
                id,
                name: name.clone(),
                arguments: arguments.clone(),
                state: "missing".into(),
                result: None,
                error: None,
            },
        });
    }
    Ok(actions)
}

/// The directories of one trial, and the placeholders that stand for them
/// in tasks and traces.
struct TrialPaths {
    workspace: PathBuf,
    outside: PathBuf,
    profile: PathBuf,
}

impl TrialPaths {
    fn places(&self) -> [(&'static str, &Path); 3] {
        [
            (WORKSPACE, &self.workspace),
            (OUTSIDE, &self.outside),
            (PROFILE, &self.profile),
        ]
    }

    /// `text` with placeholders replaced by this trial's paths.
    fn resolve(&self, text: &str) -> String {
        self.places()
            .iter()
            .fold(text.to_owned(), |text, (placeholder, path)| {
                text.replace(placeholder, &path.to_string_lossy())
            })
    }

    /// `value` with placeholders in its strings replaced by this trial's
    /// paths, JSON-escaped where they land inside a string literal.
    fn resolve_json<T: Serialize + DeserializeOwned>(&self, value: &T) -> Result<T> {
        let text = self.places().iter().fold(
            serde_json::to_string(value)?,
            |text, (placeholder, path)| text.replace(placeholder, &json_escaped(path)),
        );
        Ok(serde_json::from_str(&text)?)
    }

    /// `value` with this trial's paths replaced by placeholders, in plain
    /// and in JSON-escaped form (tool results are JSON inside strings).
    fn portable_json<T: Serialize + DeserializeOwned>(&self, value: &T) -> Result<T> {
        let mut text = serde_json::to_string(value)?;
        for (placeholder, path) in self.places() {
            let plain = path.to_string_lossy();
            let escaped = json_escaped(path);
            let twice = json_escaped(Path::new(&escaped));
            for form in [twice, escaped, plain.into_owned()] {
                text = text.replace(&form, placeholder);
            }
        }
        Ok(serde_json::from_str(&text)?)
    }

    fn write_fixtures(&self, directory: &Path, files: &[(&'static str, String)]) -> Result<()> {
        std::fs::create_dir_all(directory)?;
        for (relative, text) in files {
            let path = directory.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, self.resolve(text))
                .with_context(|| format!("Write fixture {}", path.display()))?;
        }
        Ok(())
    }
}

/// A path as it appears inside a JSON string literal.
fn json_escaped(path: &Path) -> String {
    let quoted = serde_json::to_string(&path.to_string_lossy()).unwrap_or_default();
    quoted
        .strip_prefix('"')
        .and_then(|text| text.strip_suffix('"'))
        .unwrap_or_default()
        .to_owned()
}

/// A fresh vault key for one trial's private vault.
fn random_key() -> [u8; 32] {
    let mut key = [0; 32];
    key[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    key[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    key
}

#[cfg(test)]
mod tests;
