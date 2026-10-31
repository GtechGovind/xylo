//! Event-driven application host. UI methods enqueue bounded commands and return
//! immediately; database, credentials, process I/O and networking stay off the UI.
//!
//! - Settings are the source of truth for the permission mode: every admission
//!   uses the current mode and a change is forwarded to live workers. Scheduled
//!   turns run unattended and so never get Full access (see `effective_mode`).
//! - Standing approvals ("always allow") are saved per project and loaded into
//!   every admitted turn.
//! - The host alone opens the secret vault; workers receive its key over their
//!   private pipe (see `worker_control`).
//! - Stop is captured at intake (`capture_stop`) and is the only cancel path.

use crate::AppPaths;
use anyhow::{Context, Result, ensure};
use async_channel::{Receiver, Sender};
use fs2::FileExt;
use serde_json::Value;
use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter},
    sync::{Mutex, mpsc},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use xylo_core::*;
use xylo_helper_client::HelperClient;
use xylo_resources::{ResourceLease, ResourceManager, TaskDemand};
use xylo_services::{ConnectorBroker, CredentialVault, SqliteMemory, Store};
mod action_control;
mod credential_control;
mod document_control;
mod fact_control;
mod image_control;
mod notice_control;
mod provider_control;
mod settings_control;
mod steering_control;
mod turn_queue_control;
mod update_control;
mod worker_control;
use worker_control::{SessionSequences, WorkerHandle, effective_mode, retire_idle_workers};

#[derive(Clone)]
pub struct ConnectorMutationReceipt {
    pub saved: Option<ConnectorConfig>,
    pub removed: Option<Uuid>,
}

#[derive(Clone)]
pub enum HostEvent {
    ActionReviews {
        request_id: Uuid,
        result: std::result::Result<crate::ActionReviewPage, String>,
    },
    ActionReview {
        request_id: Uuid,
        result: Box<std::result::Result<crate::ActionReviewDetail, String>>,
    },
    ActionResolved {
        request_id: Uuid,
        result: std::result::Result<crate::ActionResolutionReceipt, String>,
    },
    ActionResolution {
        request_id: Uuid,
        result: std::result::Result<Option<crate::ActionResolutionReceipt>, String>,
    },
    ActionReviewForm {
        request_id: Uuid,
        result: std::result::Result<crate::ActionReviewFormSnapshot, String>,
    },
    ActionReviewFormSaved {
        request_id: Uuid,
        result: std::result::Result<crate::ActionReviewFormSave, String>,
    },
    DocumentDraftRelinked {
        request_id: Uuid,
        result: Box<std::result::Result<crate::DocumentDraftRelinkReceipt, String>>,
    },
    DocumentDraftRelinkStatus {
        request_id: Uuid,
        result: Box<std::result::Result<Option<crate::DocumentDraftRelinkReceipt>, String>>,
    },
    DocumentDraftLoaded {
        request_id: Uuid,
        result: std::result::Result<crate::DocumentDraftSnapshot, String>,
    },
    DocumentDraftSaved {
        request_id: Uuid,
        result: std::result::Result<crate::DocumentDraftSave, String>,
    },
    DocumentDrafts {
        request_id: Uuid,
        result: std::result::Result<crate::DocumentDraftPage, String>,
    },
    NotificationActivation {
        activation: crate::notifications::Activation,
        conversation: Option<Conversation>,
    },
    DraftLoaded {
        request_id: Uuid,
        result: std::result::Result<crate::drafts::DraftSnapshot, String>,
    },
    DraftSaved {
        request_id: Uuid,
        result: std::result::Result<crate::drafts::DraftSave, String>,
    },
    Drafts {
        request_id: Uuid,
        result: std::result::Result<crate::drafts::DraftPage, String>,
    },
    DraftAdmitted {
        request_id: Uuid,
        result: std::result::Result<crate::drafts::DraftAdmission, String>,
    },
    SteeringResult {
        request_id: Uuid,
        result: std::result::Result<crate::SteeringOutcome, String>,
    },
    FactVersions {
        request_id: Uuid,
        result: std::result::Result<crate::FactPage, String>,
    },
    FactDeletePreview {
        request_id: Uuid,
        result: std::result::Result<crate::FactDeletePreview, String>,
    },
    FactDeleted {
        request_id: Uuid,
        result: std::result::Result<crate::FactDeletion, String>,
    },
    MemoryFormLoaded {
        request_id: Uuid,
        result: std::result::Result<crate::MemoryFormSnapshot, String>,
    },
    MemoryFormSaved {
        request_id: Uuid,
        result: std::result::Result<crate::MemoryFormSave, String>,
    },
    ImageGeneration {
        request_id: Uuid,
        result: std::result::Result<ImageGenerationReceipt, String>,
    },
    ImageGenerationProvider {
        request_id: Uuid,
        result: std::result::Result<Option<ImageGenerationProvider>, String>,
    },
    ImageGenerationHistory {
        request_id: Uuid,
        result: std::result::Result<Vec<ImageGenerationReceipt>, String>,
    },
    ImageImported {
        request_id: Uuid,
        session_id: Uuid,
        result: std::result::Result<(ImageAttachment, PathBuf), String>,
    },
    ImagePreview {
        request_id: Uuid,
        session_id: Uuid,
        attachment_id: Uuid,
        result: std::result::Result<PathBuf, String>,
    },
    HistorySearch {
        request_id: Uuid,
        result: std::result::Result<crate::history::HistorySearchPage, String>,
    },
    HistoryWindow(crate::history::HistoryWindow),
    WorkspaceOpened {
        requested: PathBuf,
        root: PathBuf,
    },
    WorkspaceIndexed(Arc<crate::workspace::FileIndex>),
    WorkspaceFiles {
        request_id: Uuid,
        result: std::result::Result<crate::workspace::FilePage, String>,
    },
    WorkspaceUnavailable {
        root: PathBuf,
        message: String,
    },
    ProjectSearch {
        request_id: Uuid,
        result: std::result::Result<crate::workspace::SearchPage, String>,
    },
    Engine(Envelope<EngineEvent>),
    Settings(AppSettings),
    ConversationPage(ConversationPage),
    HistoryPage(HistoryPage),
    LspResponse {
        request_id: Uuid,
        result: std::result::Result<Value, String>,
    },
    LspDiagnostics {
        path: PathBuf,
        version: Option<i32>,
        diagnostics: Value,
    },
    Models {
        generation: u64,
        provider: ProviderConfig,
        models: Vec<String>,
        context_limits: std::collections::BTreeMap<String, u32>,
        reasoning_options: std::collections::BTreeMap<String, Vec<String>>,
        default_reasoning_efforts: std::collections::BTreeMap<String, String>,
    },
    ModelsFailed {
        generation: u64,
        provider: ProviderConfig,
        message: String,
    },
    /// May contain a returning account hint. Open in the system browser; never log.
    AuthorizationUrl(String),
    Error(String),
    Status(String),
    OpenedFile {
        request_id: Uuid,
        path: PathBuf,
        result: std::result::Result<String, String>,
    },
    Saved(PathBuf),
    SaveFailed {
        path: PathBuf,
        message: String,
    },
    GitStatus {
        request_id: Uuid,
        root: PathBuf,
        result: std::result::Result<crate::workspace::git::GitStatus, String>,
    },
    GitFileDiff {
        request_id: Uuid,
        root: PathBuf,
        result: std::result::Result<String, String>,
    },
    MemoryPage {
        request_id: Uuid,
        result: std::result::Result<crate::MemoryPage, String>,
    },
    MemoryMutation {
        request_id: Uuid,
        result: std::result::Result<crate::MemoryMutation, String>,
    },
    MemoryExport {
        request_id: Uuid,
        result: std::result::Result<crate::MemoryExport, String>,
    },
    QueueAdded {
        request_id: Uuid,
        result: Result<crate::turn_queue::QueueReceipt, String>,
    },
    TurnQueue {
        request_id: Uuid,
        session_id: Uuid,
        result: Result<Vec<crate::turn_queue::QueuedTurn>, String>,
    },
    QueuedTurnStarted {
        session_id: Uuid,
        item: crate::turn_queue::QueuedTurn,
    },
    Connectors(Vec<ConnectorConfig>),
    ConnectorMutation {
        request_id: Uuid,
        result: std::result::Result<ConnectorMutationReceipt, String>,
    },
    ConnectorTools {
        request_id: Uuid,
        workspace: Option<PathBuf>,
        connector_id: Uuid,
        tools: Vec<ToolDefinition>,
    },
    ConnectorToolsFailed {
        request_id: Uuid,
        workspace: Option<PathBuf>,
        connector_id: Uuid,
        message: String,
    },
    Schedules(Vec<crate::scheduler::ScheduledTask>),
    ScheduleMutation {
        request_id: Uuid,
        result: Result<(), String>,
    },
    Capability {
        request_id: Uuid,
        name: String,
        result: std::result::Result<Value, String>,
    },
    /// Standing approvals of a project, after a load or a revocation.
    ApprovalRules {
        workspace: PathBuf,
        result: std::result::Result<Vec<xylo_core::ApprovalRule>, String>,
    },
    /// A conversation's title changed (user rename or generated title).
    ConversationRenamed(Conversation),
    Stopped,
}

enum HostCommand {
    PreparedCancel(Vec<turn_queue_control::PreparedStop>),
    RefusedCancel,
    Action(action_control::ActionCommand),
    Document(document_control::DocumentCommand),
    Fact(fact_control::FactCommand),
    Image(image_control::ImageCommand),
    SteerDraft(Uuid, crate::drafts::DraftSubmission),
    Foreground(bool),
    DraftLoad(Uuid, crate::drafts::DraftKey),
    DraftSave(Uuid, crate::drafts::DraftWrite),
    DraftList(
        Uuid,
        Option<PathBuf>,
        Option<crate::drafts::DraftCursor>,
        usize,
    ),
    DraftSubmit(Uuid, TurnRequest, crate::drafts::DraftSubmission),
    ImageImport(Uuid, Uuid, PathBuf, AttachmentSource),
    ImagePreview(Uuid, ImageAttachment),
    SearchHistory(Uuid, String, Option<ConversationCursor>, usize),
    HistoryWindow(Uuid, Uuid, usize),
    HistoryAfter(Uuid, i64, usize),
    WorkspaceRefresh,
    WorkspaceFiles(Uuid, PathBuf, crate::workspace::FileOptions),
    ProjectSearch(Uuid, PathBuf, crate::workspace::SearchOptions),
    CancelProjectSearch(Uuid),
    Worker(Option<Uuid>, WorkerCommand),
    HistoryPage(Uuid, Option<i64>, usize),
    Lsp(crate::lsp::Command),
    Conversations,
    ConversationPage(Option<ConversationCursor>, usize),
    SaveSettings(AppSettings),
    Models(u64, ProviderConfig),
    SaveProvider(u64, Box<AppSettings>, ProviderConfig, Option<String>),
    Workspace(PathBuf),
    OpenFile(Uuid, PathBuf),
    Save(PathBuf, String),
    GitStatus(Uuid, PathBuf),
    GitFileDiff(
        Uuid,
        PathBuf,
        PathBuf,
        Option<PathBuf>,
        crate::workspace::git::ChangeGroup,
    ),
    MemorySearch(Uuid, String, String, Option<crate::MemoryCursor>),
    Remember(Uuid, MemoryRecord),
    QueueDraft(Uuid, TurnRequest, crate::drafts::DraftSubmission),
    ListTurnQueue(Uuid, Uuid),
    RemoveQueuedTurn(Uuid, Uuid, Uuid),
    RunQueuedTurn(Uuid, Uuid, Uuid),
    Connectors,
    SaveConnector(ConnectorConfig),
    MutateConnector(Uuid, Option<ConnectorConfig>, Option<Uuid>, Option<String>),
    ConnectorTools(Uuid, Uuid, Option<PathBuf>),
    Schedules,
    CreateSchedule(String, String, PathBuf),
    CreateScheduleTracked(Uuid, String, String, PathBuf),
    SaveScheduleTracked(Uuid, crate::scheduler::ScheduledTask),
    RemoveScheduleTracked(Uuid, Uuid),
    Capability(Uuid, String, Value),
    CancelCapability(Uuid),
    ApprovalRules(PathBuf),
    RemoveApprovalRule(PathBuf, String),
    RenameConversation(Uuid, String),
    SetConversationPinned(Uuid, bool),
    SetConversationArchived(Uuid, bool),
    RemoveProvider(ProviderConfig),
    Shutdown,
}

#[derive(Clone)]
pub struct HostClient {
    commands: Sender<HostCommand>,
    discovery: Arc<provider_control::DiscoveryControl>,
}

impl HostClient {
    pub fn spawn() -> Result<(Self, Receiver<HostEvent>)> {
        Self::spawn_at(AppPaths::discover()?)
    }

    pub fn spawn_at(paths: AppPaths) -> Result<(Self, Receiver<HostEvent>)> {
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(paths.data.join("host.lock"))?;
        lock.try_lock_exclusive()
            .context("Xylo is already running for this profile")?;
        let discovery = Arc::new(provider_control::DiscoveryControl::default());
        let host_discovery = discovery.clone();
        let (commands, incoming) = async_channel::bounded(128);
        let (events, receiver) = async_channel::bounded(EVENT_QUEUE_CAPACITY);
        std::thread::Builder::new().name("xylo-host".into()).spawn(move || {
            let _profile_lock = lock;
            let result = (|| -> Result<()> {
                let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
                let store = Store::open(&paths.database)?;
                let database_reset = store.reset_notice();
                let recovered = store.recover_interrupted()?;
                store.recover_steering()?;
                store.recover_turn_queue()?;
                let (mut settings, settings_reset) = startup_settings(&store)?;
                let secrets = open_secrets(&paths.data, settings.secret_storage, &store);
                follow_vault_storage(&store, &mut settings, &paths.data, secrets.as_ref().ok())?;
                let secrets = secrets.map_err(|error| format!("{error:#}"));
                let accounts = xylo_services::providers::ChatGptSignInService::new(&paths.data)?;
                let notifications = crate::notifications::Service::new(
                    paths.data.clone(),
                    crate::notifications::AppIdentity {
                        id: APPLICATION_ID.into(),
                        name: APPLICATION_NAME.into(),
                    },
                )?;
                let (notices, notice_incoming) = async_channel::bounded(128);
                let updates =
                    crate::updates::Service::new(paths.data.clone(), std::env::current_exe()?)?;
                let (memory, memory_setup_needed) =
                    match SqliteMemory::configured(store.clone(), &paths.data.join("embedding.json")) {
                        Ok(memory) => (memory, false),
                        Err(error) => {
                            tracing::warn!(%error, "Semantic memory assets need repair");
                            (SqliteMemory::new(store.clone()), true)
                        }
                    };
                let (editor_events, editor_incoming) = async_channel::bounded(128);
                let (workspace_events, workspace_incoming) = async_channel::bounded(16);
                let workspace = crate::workspace::WorkspaceService::with_cache(
                    workspace_events,
                    paths.data.join("workspace-index"),
                );
                let context = runtime.enter();
                let lsp = crate::lsp::Service::new(paths.data.clone(), editor_events);
                drop(context);
                // Read once, right after open: an unreadable vault was replaced.
                let secrets_notice = match &secrets {
                    Ok(secrets) => secrets.vault.reset_notice(),
                    Err(error) => Some(format!(
                        "Saved keys are unavailable: {error}. Choose the same secret storage in Settings to try again, or a different one to start over without them."
                    )),
                };
                let state = Arc::new(HostState {
                    discovery: host_discovery,
                    provider_save_gate: Mutex::new(()),
                    paths,
                    memory,
                    accounts,
                    updates,
                    notifications,
                    notices,
                    foreground: Arc::new(AtomicBool::new(false)),
                    operations: tokio::sync::RwLock::new(()),
                    installing: Arc::new(AtomicBool::new(false)),
                    install_request: Arc::new(parking_lot::Mutex::new(None)),
                    workspace,
                    search_requests: Mutex::new(HashMap::new()),
                    image_import_gate: Mutex::new(()),
                    secrets: parking_lot::RwLock::new(secrets),
                    store,
                    settings: Mutex::new(settings.clone()),
                    workers: Mutex::new(HashMap::new()),
                    sequences: SessionSequences::default(),
                    events: events.clone(),
                    resources: ResourceManager::new(),
                    queued: Arc::new(parking_lot::Mutex::new(HashMap::new())),
                    turn_queue: turn_queue_control::TurnQueueControl::default(),
                    scheduled: Arc::new(Mutex::new(HashMap::new())),
                    stopping: Arc::new(AtomicBool::new(false)),
                    helpers: Mutex::new(HashMap::new()),
                    capability_requests: Mutex::new(HashMap::new()),
                    lsp,
                });
                runtime.block_on(async move {
                    let maintenance_stop=CancellationToken::new();
                    let _maintenance_guard=maintenance_stop.clone().drop_guard();
                    let notification_tasks=state.start_notifications(notice_incoming,maintenance_stop.clone());
                    let learning=xylo_services::LearningService::new(state.store.clone());
                    let learning_stop=maintenance_stop.clone();
                    let learning_task=tokio::spawn(async move {
                        if let Err(error)=learning.run(learning_stop).await {tracing::warn!(%error,"Background procedure evaluation stopped");}
                    });
                    let project_events=state.events.clone();
                    let project_forwarder=tokio::spawn(async move {
                        while let Ok(event)=workspace_incoming.recv().await {
                            let event=match event {
                                crate::workspace::WorkspaceEvent::Indexed(index)=>HostEvent::WorkspaceIndexed(index),
                                crate::workspace::WorkspaceEvent::Unavailable{root,message}=>HostEvent::WorkspaceUnavailable{root,message},
                            };
                            if project_events.send(event).await.is_err(){break;}
                        }
                    });
                    let editor_host_events=state.events.clone();
                    let editor_forwarder=tokio::spawn(async move {
                        while let Ok(event)=editor_incoming.recv().await {
                            let event=match event {
                                crate::lsp::LspEvent::Response{request_id,result}=>HostEvent::LspResponse{request_id,result},
                                crate::lsp::LspEvent::Diagnostics{path,version,diagnostics}=>HostEvent::LspDiagnostics{path,version,diagnostics},
                            };
                            if editor_host_events.send(event).await.is_err(){break;}
                        }
                    });
                    state.events.send(HostEvent::Settings(settings)).await?;
                    if let Some(reset) = database_reset {
                        state.events.send(HostEvent::Status(reset.to_string())).await?;
                    }
                    if settings_reset {
                        state.events.send(HostEvent::Status(SETTINGS_RESET_NOTICE.into())).await?;
                    }
                    if let Some(notice) = secrets_notice {
                        state.events.send(HostEvent::Status(notice)).await?;
                    }
                    if memory_setup_needed {state.events.send(HostEvent::Status("Semantic search needs setup in Downloads. Your saved memories remain searchable.".into())).await?;}
                    if recovered > 0 {state.events.send(HostEvent::Status(format!("Recovered {recovered} interrupted actions; uncertain writes require review"))).await?;}
                    state.send_conversations().await?;
                    let scheduling_state=state.clone();
                    let scheduling=tokio::spawn(async move{scheduling_state.schedule_loop().await;});
                    let settings_commits = settings_control::CommitOrder::default();
                    // Project opens are ordered among themselves, not behind
                    // settings commits, so a slow provider save cannot block them.
                    let workspace_opens = settings_control::CommitOrder::default();
                    while let Ok(command) = incoming.recv().await {
                        if matches!(command,HostCommand::Shutdown) {maintenance_stop.cancel();state.shutdown().await;scheduling.abort();editor_forwarder.abort();project_forwarder.abort();let _=learning_task.await;for task in notification_tasks {let _=task.await;}state.events.send(HostEvent::Stopped).await?;break;}
                        // Reserve at receipt, before any task can race to acquire admission.
                        let settings_commit = matches!(&command, HostCommand::SaveSettings(_) | HostCommand::SaveProvider(..) | HostCommand::RemoveProvider(_)).then(|| settings_commits.reserve());
                        // Preserve document ordering independently of concurrent
                        // host work. The language-server actor owns synchronization.
                        if state.installing.load(Ordering::Acquire) && matches!(&command,HostCommand::Workspace(_)|HostCommand::Lsp(_)) {
                            if let HostCommand::Lsp(crate::lsp::Command::Request{request_id,..})=command {
                                state.events.send(HostEvent::LspResponse{request_id,result:Err("Wait for the update installation to finish".into())}).await?;
                            }
                            continue;
                        }
                        let command=match command {
                            HostCommand::Foreground(active)=>{state.foreground.store(active,Ordering::Release);continue;},
                            HostCommand::Capability(request_id,name,_) if name=="updates_status"=>{
                                state.events.send(HostEvent::Capability{request_id,name,result:Ok(state.updates.status())}).await?;
                                continue;
                            },
                            HostCommand::Workspace(path)=>{
                                let order = workspace_opens.reserve();
                                let state = state.clone();
                                tokio::spawn(async move {
                                    let _order = order.enter().await;
                                    if let Err(error) = state.handle(HostCommand::Workspace(path)).await {
                                        let _ = state.events.send(HostEvent::Error(error.to_string())).await;
                                    }
                                });
                                continue;
                            },
                            HostCommand::Lsp(mut command)=>{
                                // Code intelligence follows every project open received before it.
                                drop(workspace_opens.reserve().enter().await);
                                let request_id=if let crate::lsp::Command::Request{request_id,..}=&command{Some(*request_id)}else{None};
                                if let crate::lsp::Command::Request{workspace,..}|crate::lsp::Command::Change{workspace,..}=&mut command {
                                    if let Some(root)=state.settings.lock().await.last_workspace.clone(){*workspace=root;}
                                    else {
                                        if let Some(request_id)=request_id {state.events.send(HostEvent::LspResponse{request_id,result:Err("Open a project to use code intelligence".into())}).await?;}
                                        continue;
                                    }
                                }
                                if let Err(error)=state.lsp.enqueue(command) {
                                    if let Some(request_id)=request_id {state.events.send(HostEvent::LspResponse{request_id,result:Err(error.to_string())}).await?;}
                                    else {tracing::debug!(%error,"Editor assistance queue is busy");}
                                }
                                continue;
                            },
                            command=>command,
                        };
                        let command=state.capture_stop(command);
                        if !state.prepare_command(&command).await? {continue;}
                        let provider_context = match &command {
                            HostCommand::SaveProvider(generation, _, provider, _) => Some((*generation, provider.clone())),
                            HostCommand::Models(generation, config) => Some((*generation, config.clone())),
                            _ => None,
                        };
                        let state = state.clone();
                        tokio::spawn(async move {
                            let _commit = match settings_commit { Some(receipt) => Some(receipt.enter().await), None => None };
                            if let Err(error) = state.handle(command).await {
                                if let Some((generation, provider)) = provider_context {
                                    if state.discovery.generation() == generation {
                                        let _ = state.events.send(HostEvent::ModelsFailed { generation, message: provider_control::discovery_error(&provider, error), provider }).await;
                                    }
                                } else {
                                    let _ = state.events.send(HostEvent::Error(format!("{error:#}"))).await;
                                }
                            }
                        });
                    }
                    Ok::<_,anyhow::Error>(())
                })
            })();
            if let Err(error) = result {let _ = events.send_blocking(HostEvent::Error(format!("Host stopped: {error:#}")));}
        })?;
        Ok((
            Self {
                commands,
                discovery,
            },
            receiver,
        ))
    }

    fn enqueue(&self, command: HostCommand) -> Result<()> {
        self.commands
            .try_send(command)
            .context("Application host queue is full or disconnected")
    }
    /// GPUI activation/visibility only; no model/tool can synthesize foreground state.
    pub fn foreground(&self, active: bool) -> Result<()> {
        self.enqueue(HostCommand::Foreground(active))
    }
    pub fn load_draft(&self, request_id: Uuid, key: crate::drafts::DraftKey) -> Result<()> {
        self.enqueue(HostCommand::DraftLoad(request_id, key))
    }
    pub fn save_draft(&self, request_id: Uuid, write: crate::drafts::DraftWrite) -> Result<()> {
        self.enqueue(HostCommand::DraftSave(request_id, write))
    }
    pub fn list_drafts(
        &self,
        request_id: Uuid,
        workspace: Option<PathBuf>,
        before: Option<crate::drafts::DraftCursor>,
        limit: usize,
    ) -> Result<()> {
        self.enqueue(HostCommand::DraftList(request_id, workspace, before, limit))
    }
    pub fn submit_draft(
        &self,
        request_id: Uuid,
        turn: TurnRequest,
        submission: crate::drafts::DraftSubmission,
    ) -> Result<()> {
        self.enqueue(HostCommand::DraftSubmit(request_id, turn, submission))
    }
    pub fn import_image(
        &self,
        request_id: Uuid,
        session_id: Uuid,
        path: PathBuf,
        source: AttachmentSource,
    ) -> Result<()> {
        self.enqueue(HostCommand::ImageImport(
            request_id, session_id, path, source,
        ))
    }
    pub fn image_preview(&self, request_id: Uuid, attachment: ImageAttachment) -> Result<()> {
        self.enqueue(HostCommand::ImagePreview(request_id, attachment))
    }
    pub fn send(&self, command: WorkerCommand) -> Result<()> {
        self.enqueue(HostCommand::Worker(None, command))
    }
    pub fn send_to(&self, session: Uuid, command: WorkerCommand) -> Result<()> {
        self.enqueue(HostCommand::Worker(Some(session), command))
    }
    pub fn load_session_page(&self, id: Uuid, before: Option<i64>, limit: usize) -> Result<()> {
        self.enqueue(HostCommand::HistoryPage(id, before, limit))
    }
    #[allow(clippy::too_many_arguments)]
    pub fn lsp_request(
        &self,
        request_id: Uuid,
        path: PathBuf,
        version: i32,
        text: String,
        method: String,
        params: Value,
    ) -> Result<()> {
        self.enqueue(HostCommand::Lsp(crate::lsp::Command::Request {
            request_id,
            workspace: PathBuf::new(),
            path,
            version,
            text,
            method,
            params,
        }))
    }
    pub fn lsp_change(&self, path: PathBuf, version: i32, text: String) -> Result<()> {
        self.enqueue(HostCommand::Lsp(crate::lsp::Command::Change {
            workspace: PathBuf::new(),
            path,
            version,
            text,
        }))
    }
    pub fn lsp_close(&self, path: PathBuf) -> Result<()> {
        self.enqueue(HostCommand::Lsp(crate::lsp::Command::Close(path)))
    }
    pub fn lsp_cancel(&self, request_id: Uuid) -> Result<()> {
        self.enqueue(HostCommand::Lsp(crate::lsp::Command::Cancel(request_id)))
    }
    pub fn list_conversations(&self) -> Result<()> {
        self.enqueue(HostCommand::Conversations)
    }
    /// Loads a project's standing approvals; answered with `HostEvent::ApprovalRules`.
    pub fn approval_rules(&self, workspace: PathBuf) -> Result<()> {
        self.enqueue(HostCommand::ApprovalRules(workspace))
    }
    /// Revokes one standing approval; answered with `HostEvent::ApprovalRules`.
    pub fn remove_approval_rule(&self, workspace: PathBuf, key: String) -> Result<()> {
        self.enqueue(HostCommand::RemoveApprovalRule(workspace, key))
    }
    /// Renames a conversation; answered with `HostEvent::ConversationRenamed`.
    pub fn rename_conversation(&self, id: Uuid, title: String) -> Result<()> {
        self.enqueue(HostCommand::RenameConversation(id, title))
    }
    /// Pins or unpins a conversation; answered with a fresh
    /// `HostEvent::ConversationPage`.
    pub fn set_conversation_pinned(&self, id: Uuid, pinned: bool) -> Result<()> {
        self.enqueue(HostCommand::SetConversationPinned(id, pinned))
    }
    /// Archives or restores a conversation; answered with a fresh
    /// `HostEvent::ConversationPage`.
    pub fn set_conversation_archived(&self, id: Uuid, archived: bool) -> Result<()> {
        self.enqueue(HostCommand::SetConversationArchived(id, archived))
    }
    /// Removes a saved provider connection and, when unshared, its secret;
    /// answered with `HostEvent::Settings`.
    pub fn remove_provider(&self, provider: ProviderConfig) -> Result<()> {
        self.enqueue(HostCommand::RemoveProvider(provider))
    }
    pub fn search_history(
        &self,
        id: Uuid,
        query: String,
        before: Option<ConversationCursor>,
        limit: usize,
    ) -> Result<()> {
        self.enqueue(HostCommand::SearchHistory(id, query, before, limit))
    }
    pub fn history_window(&self, session: Uuid, anchor: Uuid, limit: usize) -> Result<()> {
        self.enqueue(HostCommand::HistoryWindow(session, anchor, limit))
    }
    pub fn history_after(&self, session: Uuid, after: i64, limit: usize) -> Result<()> {
        self.enqueue(HostCommand::HistoryAfter(session, after, limit))
    }
    pub fn list_conversations_page(
        &self,
        before: Option<ConversationCursor>,
        limit: usize,
    ) -> Result<()> {
        self.enqueue(HostCommand::ConversationPage(before, limit))
    }
    pub fn save_settings(&self, settings: AppSettings) -> Result<()> {
        self.enqueue(HostCommand::SaveSettings(settings))
    }
    /// Persist credentials before settings and discovery, within one host operation.
    pub fn save_provider(&self, settings: AppSettings, secret: Option<String>) -> Result<()> {
        let provider = settings.provider.clone();
        self.save_provider_connection(settings, provider, secret)
    }
    /// Save one connection without replacing the active model before its catalog succeeds.
    pub fn save_provider_connection(
        &self,
        settings: AppSettings,
        provider: ProviderConfig,
        secret: Option<String>,
    ) -> Result<()> {
        self.enqueue(HostCommand::SaveProvider(
            self.discovery.begin(),
            Box::new(settings),
            provider,
            secret,
        ))
    }
    pub fn cancel_model_discovery(&self) -> Result<()> {
        self.discovery.begin();
        Ok(())
    }
    pub fn provider_discovery_generation(&self) -> u64 {
        self.discovery.generation()
    }
    pub fn discover_models(&self, config: ProviderConfig) -> Result<()> {
        self.enqueue(HostCommand::Models(self.discovery.begin(), config))
    }
    pub fn open_workspace(&self, path: PathBuf) -> Result<()> {
        self.enqueue(HostCommand::Workspace(path))
    }
    pub fn refresh_workspace(&self) -> Result<()> {
        self.enqueue(HostCommand::WorkspaceRefresh)
    }
    /// Fetch one bounded directory page without materializing the project tree.
    pub fn workspace_files(
        &self,
        request_id: Uuid,
        root: PathBuf,
        options: crate::workspace::FileOptions,
    ) -> Result<()> {
        self.enqueue(HostCommand::WorkspaceFiles(request_id, root, options))
    }
    pub fn search_project(
        &self,
        request_id: Uuid,
        root: PathBuf,
        options: crate::workspace::SearchOptions,
    ) -> Result<()> {
        self.enqueue(HostCommand::ProjectSearch(request_id, root, options))
    }
    pub fn cancel_project_search(&self, request_id: Uuid) -> Result<()> {
        self.enqueue(HostCommand::CancelProjectSearch(request_id))
    }
    pub fn open_file(&self, request_id: Uuid, path: PathBuf) -> Result<()> {
        self.enqueue(HostCommand::OpenFile(request_id, path))
    }
    pub fn save_file(&self, path: PathBuf, content: String) -> Result<()> {
        self.enqueue(HostCommand::Save(path, content))
    }
    pub fn git_status(&self, request_id: Uuid, root: PathBuf) -> Result<()> {
        self.enqueue(HostCommand::GitStatus(request_id, root))
    }
    pub fn git_file_diff(
        &self,
        request_id: Uuid,
        root: PathBuf,
        path: PathBuf,
        original: Option<PathBuf>,
        group: crate::workspace::git::ChangeGroup,
    ) -> Result<()> {
        self.enqueue(HostCommand::GitFileDiff(
            request_id, root, path, original, group,
        ))
    }
    pub fn memory_search(
        &self,
        request_id: Uuid,
        scope: String,
        query: String,
        before: Option<crate::MemoryCursor>,
    ) -> Result<()> {
        self.enqueue(HostCommand::MemorySearch(request_id, scope, query, before))
    }
    pub fn memory_remember(&self, request_id: Uuid, record: MemoryRecord) -> Result<()> {
        self.enqueue(HostCommand::Remember(request_id, record))
    }
    pub fn connectors(&self) -> Result<()> {
        self.enqueue(HostCommand::Connectors)
    }
    pub fn save_connector(&self, config: ConnectorConfig) -> Result<()> {
        self.enqueue(HostCommand::SaveConnector(config))
    }
    pub fn save_connector_authenticated(
        &self,
        request_id: Uuid,
        config: ConnectorConfig,
        secret: Option<String>,
    ) -> Result<()> {
        self.enqueue(HostCommand::MutateConnector(
            request_id,
            Some(config),
            None,
            secret,
        ))
    }
    pub fn remove_connector(&self, request_id: Uuid, id: Uuid) -> Result<()> {
        self.enqueue(HostCommand::MutateConnector(
            request_id,
            None,
            Some(id),
            None,
        ))
    }
    pub fn connector_tools_scoped(
        &self,
        request_id: Uuid,
        id: Uuid,
        workspace: Option<PathBuf>,
    ) -> Result<()> {
        self.enqueue(HostCommand::ConnectorTools(request_id, id, workspace))
    }
    pub fn list_schedules(&self) -> Result<()> {
        self.enqueue(HostCommand::Schedules)
    }
    pub fn create_schedule(&self, prompt: String, cron: String, workspace: PathBuf) -> Result<()> {
        self.enqueue(HostCommand::CreateSchedule(prompt, cron, workspace))
    }
    pub fn create_schedule_tracked(
        &self,
        request_id: Uuid,
        prompt: String,
        cron: String,
        workspace: PathBuf,
    ) -> Result<()> {
        self.enqueue(HostCommand::CreateScheduleTracked(
            request_id, prompt, cron, workspace,
        ))
    }
    pub fn save_schedule_tracked(
        &self,
        request_id: Uuid,
        task: crate::scheduler::ScheduledTask,
    ) -> Result<()> {
        self.enqueue(HostCommand::SaveScheduleTracked(request_id, task))
    }
    pub fn remove_schedule_tracked(&self, request_id: Uuid, id: Uuid) -> Result<()> {
        self.enqueue(HostCommand::RemoveScheduleTracked(request_id, id))
    }
    /// Perform an explicit product action without requiring a model turn.
    pub fn capability(&self, request_id: Uuid, name: String, args: Value) -> Result<()> {
        self.enqueue(HostCommand::Capability(request_id, name, args))
    }
    pub fn cancel_capability(&self, request_id: Uuid) -> Result<()> {
        self.enqueue(HostCommand::CancelCapability(request_id))
    }
    pub fn shutdown(&self) -> Result<()> {
        self.enqueue(HostCommand::Shutdown)
    }
}

struct ModelActivityLease(std::fs::File);
impl Drop for ModelActivityLease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

type PendingRuns = Arc<parking_lot::Mutex<HashMap<Uuid, (Uuid, CancellationToken)>>>;

/// A queued message being admitted: the item, its admitted flag, and whether
/// the queue started it automatically.
type QueuedAdmission = (crate::turn_queue::QueuedTurn, Arc<AtomicBool>, bool);

/// How a Run admission ended.
enum Admission {
    Dispatched(Option<crate::drafts::DraftAdmission>),
    /// The turn had already completed; its stored answer was published.
    Replayed(Option<crate::drafts::DraftAdmission>),
    Cancelled,
}

/// Saved models belong to one connection when everything but the model
/// choice matches.
fn same_connection(a: &ProviderConfig, b: &ProviderConfig) -> bool {
    a.family == b.family
        && a.endpoint == b.endpoint
        && a.credential_ref == b.credential_ref
        && a.discovery_url == b.discovery_url
        && a.headers == b.headers
}

/// Retain a session's starting state until it is either running or fully failed.
/// Cancelling a queued turn leaves the guard in place until admission unwinds.
struct PendingRun {
    session: Uuid,
    pending: PendingRuns,
    id: Uuid,
}

impl Drop for PendingRun {
    fn drop(&mut self) {
        let mut pending = self.pending.lock();
        if pending
            .get(&self.session)
            .is_some_and(|(id, _)| *id == self.id)
        {
            pending.remove(&self.session);
        }
    }
}
/// The secret vault and the connector broker that reads from it. Both are
/// replaced together when the configured secret storage changes.
struct Secrets {
    storage: SecretStorage,
    vault: CredentialVault,
    connectors: Arc<ConnectorBroker>,
}

/// Opens the vault. Its own record of where the key lives wins over
/// `storage`, so [`Secrets::storage`] may differ from the request.
fn open_secrets(data: &std::path::Path, storage: SecretStorage, store: &Store) -> Result<Secrets> {
    secrets_for(CredentialVault::open(data, storage)?, store)
}

/// Opens a vault that was unavailable, after the user chose `storage`.
/// Choosing the storage the vault records retries it; choosing another while
/// the vault still cannot be opened starts over there without the saved keys
/// (the startup notice says so). Returns whether it started over.
fn reopen_secrets(
    data: &std::path::Path,
    storage: SecretStorage,
    store: &Store,
) -> Result<(Secrets, bool)> {
    match open_secrets(data, storage, store) {
        Ok(secrets) => Ok((secrets, false)),
        Err(error) if CredentialVault::recorded_storage(data)? != Some(storage) => {
            tracing::warn!("Starting the secret vault over: {error:#}");
            Ok((
                secrets_for(CredentialVault::start_over(data, storage)?, store)?,
                true,
            ))
        }
        Err(error) => Err(error),
    }
}

fn secrets_for(vault: CredentialVault, store: &Store) -> Result<Secrets> {
    Ok(Secrets {
        storage: vault
            .storage()
            .context("A host vault knows where its key is stored")?,
        connectors: Arc::new(ConnectorBroker::new(store.clone(), vault.clone())),
        vault,
    })
}

/// Makes the saved setting name the storage the vault actually uses (or
/// records, when it could not be opened). The vault's record is the source of
/// truth, so a reset settings database never points the vault at the wrong
/// key.
fn follow_vault_storage(
    store: &Store,
    settings: &mut AppSettings,
    data: &std::path::Path,
    secrets: Option<&Secrets>,
) -> Result<()> {
    let actual = match secrets {
        Some(secrets) => Some(secrets.storage),
        None => CredentialVault::recorded_storage(data).unwrap_or_else(|error| {
            tracing::warn!("Read where the vault key is stored: {error:#}");
            None
        }),
    };
    if let Some(actual) = actual
        && actual != settings.secret_storage
    {
        settings.secret_storage = actual;
        store.save_settings(settings)?;
    }
    Ok(())
}

/// Shown when a storage switch (or a start-over) discarded the saved keys.
const STORAGE_CHANGED_NOTICE: &str =
    "Secret storage changed. Saved keys were removed; reconnect your providers.";

const SETTINGS_RESET_NOTICE: &str =
    "Your saved settings could not be read, so Xylo started with default settings.";

/// Stored settings, or defaults (saved, with a notice flag) when they no
/// longer decode. There is no compatibility parsing of older formats.
fn startup_settings(store: &Store) -> Result<(AppSettings, bool)> {
    match store.settings() {
        Ok(settings) => Ok((settings, false)),
        Err(error) => {
            tracing::warn!(%error, "Stored settings could not be read; using defaults");
            let defaults = AppSettings::default();
            store.save_settings(&defaults)?;
            Ok((defaults, true))
        }
    }
}

struct HostState {
    discovery: Arc<provider_control::DiscoveryControl>,
    provider_save_gate: Mutex<()>,
    workspace: crate::workspace::WorkspaceService,
    search_requests: Mutex<HashMap<Uuid, CancellationToken>>,
    image_import_gate: Mutex<()>,
    paths: AppPaths,
    store: Store,
    memory: SqliteMemory,
    accounts: xylo_services::providers::ChatGptSignInService,
    /// The vault and connector broker, or why the vault could not be opened.
    secrets: parking_lot::RwLock<std::result::Result<Secrets, String>>,
    settings: Mutex<AppSettings>,
    workers: Mutex<HashMap<Uuid, WorkerHandle>>,
    sequences: SessionSequences,
    events: Sender<HostEvent>,
    resources: ResourceManager,
    queued: PendingRuns,
    turn_queue: turn_queue_control::TurnQueueControl,
    scheduled: Arc<Mutex<HashMap<Uuid, Uuid>>>,
    stopping: Arc<AtomicBool>,
    installing: Arc<AtomicBool>,
    install_request: Arc<parking_lot::Mutex<Option<Uuid>>>,
    operations: tokio::sync::RwLock<()>,
    updates: crate::updates::Service,
    notifications: crate::notifications::Service,
    notices: Sender<crate::notifications::NoticeEvent>,
    foreground: Arc<AtomicBool>,
    helpers: Mutex<HashMap<PathBuf, HelperClient>>,
    capability_requests: Mutex<HashMap<Uuid, CancellationToken>>,
    lsp: crate::lsp::Service,
}

impl HostState {
    /// Register cancellation before dispatching asynchronous work. The command
    /// stream is ordered even when its execution tasks are scheduled differently.
    async fn prepare_command(&self, command: &HostCommand) -> Result<bool> {
        if self.installing.load(Ordering::Acquire) {
            match command {
                HostCommand::QueueDraft(id, _, _) => {
                    self.events
                        .send(HostEvent::QueueAdded {
                            request_id: *id,
                            result: Err("Wait for the update installation to finish".into()),
                        })
                        .await?;
                    return Ok(false);
                }
                HostCommand::ListTurnQueue(id, session)
                | HostCommand::RemoveQueuedTurn(id, session, _)
                | HostCommand::RunQueuedTurn(id, session, _) => {
                    self.emit_turn_queue(
                        *id,
                        *session,
                        Some("Wait for the update installation to finish".into()),
                    )
                    .await?;
                    return Ok(false);
                }
                HostCommand::Document(command) => {
                    self.events
                        .send(command.refused("Wait for the update installation to finish"))
                        .await?;
                    return Ok(false);
                }
                HostCommand::Fact(command) => {
                    self.events
                        .send(command.refused("Wait for the update installation to finish"))
                        .await?;
                    return Ok(false);
                }
                HostCommand::Action(command) => {
                    self.events
                        .send(command.refused("Wait for the update installation to finish"))
                        .await?;
                    return Ok(false);
                }
                HostCommand::Image(command) => {
                    self.events
                        .send(command.refused("Wait for the update installation to finish"))
                        .await?;
                    return Ok(false);
                }
                HostCommand::SteerDraft(id, _) => {
                    self.events
                        .send(HostEvent::SteeringResult {
                            request_id: *id,
                            result: Err("Wait for the update installation to finish".into()),
                        })
                        .await?;
                    return Ok(false);
                }
                HostCommand::WorkspaceFiles(id, _, _) | HostCommand::ProjectSearch(id, _, _) => {
                    let message = "Wait for the update installation to finish".to_owned();
                    let event = match command {
                        HostCommand::WorkspaceFiles(_, _, _) => HostEvent::WorkspaceFiles {
                            request_id: *id,
                            result: Err(message),
                        },
                        _ => HostEvent::ProjectSearch {
                            request_id: *id,
                            result: Err(message),
                        },
                    };
                    self.events.send(event).await?;
                    return Ok(false);
                }
                HostCommand::Capability(_, name, _) if name == "updates_status" => {}
                HostCommand::CancelCapability(_) | HostCommand::CancelProjectSearch(_) => {}
                HostCommand::Capability(id, name, _) => {
                    self.events
                        .send(HostEvent::Capability {
                            request_id: *id,
                            name: name.clone(),
                            result: Err("Wait for the update installation to finish".into()),
                        })
                        .await?;
                    return Ok(false);
                }
                HostCommand::DraftLoad(id, _)
                | HostCommand::DraftSave(id, _)
                | HostCommand::DraftList(id, _, _, _)
                | HostCommand::DraftSubmit(id, _, _) => {
                    let message = "Wait for the update installation to finish".to_owned();
                    let event = match command {
                        HostCommand::DraftLoad(_, _) => HostEvent::DraftLoaded {
                            request_id: *id,
                            result: Err(message),
                        },
                        HostCommand::DraftSave(_, _) => HostEvent::DraftSaved {
                            request_id: *id,
                            result: Err(message),
                        },
                        HostCommand::DraftList(_, _, _, _) => HostEvent::Drafts {
                            request_id: *id,
                            result: Err(message),
                        },
                        _ => HostEvent::DraftAdmitted {
                            request_id: *id,
                            result: Err(message),
                        },
                    };
                    self.events.send(event).await?;
                    return Ok(false);
                }
                HostCommand::MemorySearch(id, _, _, _) | HostCommand::Remember(id, _) => {
                    let message = "Wait for the update installation to finish".to_owned();
                    let event = match command {
                        HostCommand::MemorySearch(_, _, _, _) => HostEvent::MemoryPage {
                            request_id: *id,
                            result: Err(message),
                        },
                        _ => HostEvent::MemoryMutation {
                            request_id: *id,
                            result: Err(message),
                        },
                    };
                    self.events.send(event).await?;
                    return Ok(false);
                }
                HostCommand::ImageImport(id, session, _, _) => {
                    self.events
                        .send(HostEvent::ImageImported {
                            request_id: *id,
                            session_id: *session,
                            result: Err("Wait for the update installation to finish".into()),
                        })
                        .await?;
                    return Ok(false);
                }
                HostCommand::ImagePreview(id, image) => {
                    self.events
                        .send(HostEvent::ImagePreview {
                            request_id: *id,
                            session_id: image.session_id,
                            attachment_id: image.id,
                            result: Err("Wait for the update installation to finish".into()),
                        })
                        .await?;
                    return Ok(false);
                }
                _ => {
                    self.events
                        .send(HostEvent::Status(
                            "Wait for the update installation to finish".into(),
                        ))
                        .await?;
                    return Ok(false);
                }
            }
        }
        let control_id = match command {
            HostCommand::Document(command) => command.cancellable_id(),
            HostCommand::Action(command) => command.cancellable_id(),
            HostCommand::Fact(command) => command.cancellable_id(),
            HostCommand::Image(command) => command.cancellable_id(),
            _ => None,
        };
        if let Some(id) = control_id {
            let mut requests = self.capability_requests.lock().await;
            if requests.contains_key(&id) || requests.len() >= 64 {
                drop(requests);
                let event = match command {
                    HostCommand::Document(command) => {
                        command.refused("Wait for the current actions to finish")
                    }
                    HostCommand::Action(command) => {
                        command.refused("Wait for the current actions to finish")
                    }
                    HostCommand::Fact(command) => {
                        command.refused("Wait for the current actions to finish")
                    }
                    HostCommand::Image(command) => {
                        command.refused("Wait for the current actions to finish")
                    }
                    _ => unreachable!(),
                };
                self.events.send(event).await?;
                return Ok(false);
            }
            requests.insert(id, CancellationToken::new());
        }
        match command {
            HostCommand::ProjectSearch(id, _, _) | HostCommand::WorkspaceFiles(id, _, _) => {
                let mut searches = self.search_requests.lock().await;
                if searches.len() >= 8 || searches.contains_key(id) {
                    drop(searches);
                    let message = "Wait for the current project request to finish".to_owned();
                    let event = match command {
                        HostCommand::WorkspaceFiles(_, _, _) => HostEvent::WorkspaceFiles {
                            request_id: *id,
                            result: Err(message),
                        },
                        _ => HostEvent::ProjectSearch {
                            request_id: *id,
                            result: Err(message),
                        },
                    };
                    self.events.send(event).await?;
                    return Ok(false);
                }
                searches.insert(*id, CancellationToken::new());
            }
            HostCommand::CancelProjectSearch(id) => {
                if let Some(token) = self.search_requests.lock().await.get(id) {
                    token.cancel();
                }
                return Ok(false);
            }
            HostCommand::RunQueuedTurn(request, session, item) => {
                let accepted = {
                    let mut queued = self.queued.lock();
                    if let std::collections::hash_map::Entry::Vacant(entry) = queued.entry(*session)
                    {
                        entry.insert((*item, CancellationToken::new()));
                        true
                    } else {
                        false
                    }
                };
                if !accepted {
                    self.emit_turn_queue(
                        *request,
                        *session,
                        Some("This chat is already waiting to start".into()),
                    )
                    .await?;
                    return Ok(false);
                }
            }
            HostCommand::Worker(_, WorkerCommand::Run { turn })
            | HostCommand::DraftSubmit(_, turn, _) => {
                let accepted = {
                    let mut queued = self.queued.lock();
                    if let std::collections::hash_map::Entry::Vacant(entry) =
                        queued.entry(turn.session_id)
                    {
                        entry.insert((turn.turn_id, CancellationToken::new()));
                        true
                    } else {
                        false
                    }
                };
                if !accepted {
                    let event = if let HostCommand::DraftSubmit(request_id, _, _) = command {
                        HostEvent::DraftAdmitted {
                            request_id: *request_id,
                            result: Err("This chat is already waiting to start".into()),
                        }
                    } else {
                        HostEvent::Error("This chat is already waiting to start".into())
                    };
                    self.events.send(event).await?;
                    return Ok(false);
                }
            }
            HostCommand::ImageImport(id, session, _, _) => {
                let mut requests = self.capability_requests.lock().await;
                if requests.contains_key(id) || requests.len() >= 64 {
                    drop(requests);
                    self.events
                        .send(HostEvent::ImageImported {
                            request_id: *id,
                            session_id: *session,
                            result: Err("Wait for the current image import to finish".into()),
                        })
                        .await?;
                    return Ok(false);
                }
                requests.insert(*id, CancellationToken::new());
            }
            HostCommand::Capability(id, name, _) => {
                let mut requests = self.capability_requests.lock().await;
                let error = if requests.contains_key(id) {
                    Some("This action is already running")
                } else if requests.len() >= 64 {
                    Some("Wait for the current actions to finish")
                } else {
                    None
                };
                if let Some(message) = error {
                    drop(requests);
                    self.events
                        .send(HostEvent::Capability {
                            request_id: *id,
                            name: name.clone(),
                            result: Err(message.into()),
                        })
                        .await?;
                    return Ok(false);
                }
                requests.insert(*id, CancellationToken::new());
            }
            HostCommand::CancelCapability(id) => {
                if let Some(cancel) = self.capability_requests.lock().await.get(id) {
                    cancel.cancel();
                }
                return Ok(false);
            }
            _ => {}
        }
        Ok(true)
    }
    async fn send_conversations(&self) -> Result<()> {
        let store = self.store.clone();
        let page =
            tokio::task::spawn_blocking(move || store.conversations_page(None, 100)).await??;
        self.events.send(HostEvent::ConversationPage(page)).await?;
        Ok(())
    }

    async fn handle(self: &Arc<Self>, command: HostCommand) -> Result<()> {
        if let HostCommand::Capability(id, name, _) = &command
            && name == "updates_install"
        {
            let result = self
                .install_update(*id)
                .await
                .map_err(|error| error.to_string());
            self.capability_requests.lock().await.remove(id);
            self.events
                .send(HostEvent::Capability {
                    request_id: *id,
                    name: name.clone(),
                    result,
                })
                .await?;
            return Ok(());
        }
        let _operation = self.operations.read().await;
        match command {
            HostCommand::Document(command) => self.handle_document(command).await?,
            HostCommand::Action(command) => self.handle_action(command).await?,
            HostCommand::Fact(command) => self.handle_fact(command).await?,
            HostCommand::Image(command) => self.handle_image(command).await?,
            HostCommand::QueueDraft(id, turn, submission) => {
                self.handle_queue_draft(id, turn, submission).await?
            }
            HostCommand::ListTurnQueue(id, session) => {
                self.handle_list_turn_queue(id, session).await?
            }
            HostCommand::RemoveQueuedTurn(id, session, item) => {
                self.handle_remove_queued_turn(id, session, item).await?
            }
            HostCommand::RunQueuedTurn(id, session, item) => {
                self.handle_run_queued_turn(id, session, item).await?
            }
            HostCommand::SteerDraft(id, submission) => self.steer_draft(id, submission).await?,
            HostCommand::DraftLoad(request_id, key) => {
                let result = crate::drafts::load(self.store.clone(), key, CancellationToken::new())
                    .await
                    .map_err(|error| error.to_string());
                self.events
                    .send(HostEvent::DraftLoaded { request_id, result })
                    .await?;
            }
            HostCommand::DraftSave(request_id, write) => {
                let result =
                    crate::drafts::save(self.store.clone(), write, CancellationToken::new())
                        .await
                        .map_err(|error| error.to_string());
                self.events
                    .send(HostEvent::DraftSaved { request_id, result })
                    .await?;
            }
            HostCommand::DraftList(request_id, workspace, before, limit) => {
                let result = crate::drafts::list(
                    self.store.clone(),
                    workspace,
                    before,
                    limit,
                    CancellationToken::new(),
                )
                .await
                .map_err(|error| error.to_string());
                self.events
                    .send(HostEvent::Drafts { request_id, result })
                    .await?;
            }
            HostCommand::DraftSubmit(request_id, turn, submission) => {
                let result = self
                    .worker_command_with_draft_and_receipt(
                        Some(turn.session_id),
                        WorkerCommand::Run { turn },
                        Some(submission),
                        Some(request_id),
                    )
                    .await
                    .and_then(|receipt| receipt.context("The draft was not admitted"))
                    .map_err(|error| error.to_string());
                if let Err(error) = result {
                    self.events
                        .send(HostEvent::DraftAdmitted {
                            request_id,
                            result: Err(error),
                        })
                        .await?;
                }
            }
            HostCommand::ImageImport(request_id, session_id, path, source) => {
                let result = self
                    .import_image_file(request_id, session_id, path, source)
                    .await
                    .map_err(|error| error.to_string());
                self.capability_requests.lock().await.remove(&request_id);
                self.events
                    .send(HostEvent::ImageImported {
                        request_id,
                        session_id,
                        result,
                    })
                    .await?;
            }
            HostCommand::ImagePreview(request_id, attachment) => {
                let session_id = attachment.session_id;
                let attachment_id = attachment.id;
                let artifacts = self.paths.artifacts.clone();
                let result = tokio::task::spawn_blocking(move || {
                    xylo_services::attachments::AttachmentStore::new(artifacts)?
                        .preview_path(&attachment)
                })
                .await?
                .map_err(|error| error.to_string());
                self.events
                    .send(HostEvent::ImagePreview {
                        request_id,
                        session_id,
                        attachment_id,
                        result,
                    })
                    .await?;
            }
            HostCommand::SearchHistory(id, query, before, limit) => {
                let store = self.store.clone();
                let result = tokio::task::spawn_blocking(move || {
                    store.search_history(&query, before, limit)
                })
                .await?
                .map_err(|error| error.to_string());
                self.events
                    .send(HostEvent::HistorySearch {
                        request_id: id,
                        result,
                    })
                    .await?;
            }
            HostCommand::HistoryWindow(session, anchor, limit) => {
                let store = self.store.clone();
                let page = tokio::task::spawn_blocking(move || {
                    store.history_window(session, anchor, limit)
                })
                .await??;
                self.events.send(HostEvent::HistoryWindow(page)).await?;
            }
            HostCommand::HistoryAfter(session, after, limit) => {
                let store = self.store.clone();
                let page =
                    tokio::task::spawn_blocking(move || store.history_after(session, after, limit))
                        .await??;
                self.events.send(HostEvent::HistoryWindow(page)).await?;
            }
            HostCommand::WorkspaceRefresh => self.workspace.refresh(),
            HostCommand::WorkspaceFiles(request_id, root, options) => {
                let token = self
                    .search_requests
                    .lock()
                    .await
                    .get(&request_id)
                    .cloned()
                    .context("File request is no longer pending")?;
                let result = self
                    .workspace
                    .files(root, options, token)
                    .await
                    .map_err(|error| error.to_string());
                self.search_requests.lock().await.remove(&request_id);
                self.events
                    .send(HostEvent::WorkspaceFiles { request_id, result })
                    .await?;
            }
            HostCommand::ProjectSearch(request_id, root, options) => {
                let token = self
                    .search_requests
                    .lock()
                    .await
                    .get(&request_id)
                    .cloned()
                    .context("Search is no longer pending")?;
                let result = self
                    .workspace
                    .search(root, options, token)
                    .await
                    .map_err(|error| error.to_string());
                self.search_requests.lock().await.remove(&request_id);
                self.events
                    .send(HostEvent::ProjectSearch { request_id, result })
                    .await?;
            }
            HostCommand::CancelProjectSearch(id) => {
                if let Some(token) = self.search_requests.lock().await.get(&id) {
                    token.cancel();
                }
            }
            HostCommand::PreparedCancel(stops) => self.handle_prepared_stops(stops).await?,
            HostCommand::RefusedCancel => {
                self.events
                    .send(HostEvent::Error(
                        "Select a single active task for this control action".into(),
                    ))
                    .await?;
            }
            HostCommand::Worker(session, command) => self.worker_command(session, command).await?,
            HostCommand::HistoryPage(id, before, limit) => {
                let store = self.store.clone();
                let page =
                    tokio::task::spawn_blocking(move || store.messages_page(id, before, limit))
                        .await??;
                self.events.send(HostEvent::HistoryPage(page)).await?;
            }
            HostCommand::Conversations => self.send_conversations().await?,
            HostCommand::ApprovalRules(workspace) => {
                let result = self.approval_rules(&workspace).await;
                self.events
                    .send(HostEvent::ApprovalRules { workspace, result })
                    .await?;
            }
            HostCommand::RemoveApprovalRule(workspace, key) => {
                let result = match self.revoke_approval_rule(&workspace, key).await {
                    Ok(()) => self.approval_rules(&workspace).await,
                    Err(error) => Err(format!("{error:#}")),
                };
                self.events
                    .send(HostEvent::ApprovalRules { workspace, result })
                    .await?;
            }
            HostCommand::RenameConversation(id, title) => {
                self.rename_conversation(id, title).await?
            }
            HostCommand::SetConversationPinned(id, pinned) => {
                let store = self.store.clone();
                tokio::task::spawn_blocking(move || store.set_conversation_pinned(id, pinned))
                    .await??
                    .context("This chat no longer exists")?;
                self.send_conversations().await?
            }
            HostCommand::SetConversationArchived(id, archived) => {
                let store = self.store.clone();
                tokio::task::spawn_blocking(move || store.set_conversation_archived(id, archived))
                    .await??
                    .context("This chat no longer exists")?;
                self.send_conversations().await?
            }
            HostCommand::RemoveProvider(provider) => self.remove_provider(provider).await?,
            HostCommand::ConversationPage(before, limit) => {
                let store = self.store.clone();
                let page =
                    tokio::task::spawn_blocking(move || store.conversations_page(before, limit))
                        .await??;
                self.events.send(HostEvent::ConversationPage(page)).await?;
            }
            HostCommand::SaveSettings(settings) => {
                self.commit_settings(settings.clone()).await?;
                self.events.send(HostEvent::Settings(settings)).await?;
            }
            HostCommand::SaveProvider(generation, settings, config, secret) => {
                let _save = self.provider_save_gate.lock().await;
                if self.discovery.generation() != generation {
                    return Ok(());
                }
                config.validate_connection()?;
                let normalized = xylo_services::providers::normalize_provider(&config)?;
                if let Some(secret) = secret {
                    let reference = config
                        .credential_ref
                        .clone()
                        .context("Missing credential reference")?;
                    let vault = self.vault()?;
                    tokio::task::spawn_blocking(move || vault.set(&reference, &secret)).await??;
                }
                let settings = *settings;
                self.commit_settings(settings.clone()).await?;
                if self.discovery.generation() != generation {
                    return Ok(());
                }
                self.events.send(HostEvent::Settings(settings)).await?;
                if normalized.models_url.is_some() {
                    // The ordered commit ends with Settings publication. Catalog
                    // HTTP must not retain the settings receipt or save gate.
                    let state = self.clone();
                    tokio::spawn(async move {
                        if let Err(error) = state.discover_models(generation, config).await {
                            tracing::debug!(%error, "Provider catalog delivery ended");
                        }
                    });
                } else if self.discovery.generation() == generation {
                    self.events
                        .send(HostEvent::Models {
                            generation,
                            provider: config,
                            models: Vec::new(),
                            context_limits: Default::default(),
                            reasoning_options: Default::default(),
                            default_reasoning_efforts: Default::default(),
                        })
                        .await?;
                }
            }
            HostCommand::Models(generation, config) => {
                self.discover_models(generation, config).await?
            }
            HostCommand::Workspace(path) => {
                let requested = path;
                let path = tokio::fs::canonicalize(&requested).await?;
                ensure!(path.is_dir(), "Choose a directory");
                {
                    // Persist and publish under one lock so memory and the
                    // database never disagree with a concurrent settings save.
                    let mut settings = self.settings.lock().await;
                    let mut value = settings.clone();
                    value.last_workspace = Some(path.clone());
                    if !value.ui.open_projects.contains(&path) {
                        value.ui.open_projects.push(path.clone());
                    }
                    let store = self.store.clone();
                    let saved = value.clone();
                    tokio::task::spawn_blocking(move || store.save_settings(&saved)).await??;
                    *settings = value;
                }
                self.workspace.open(&path).await?;
                self.events
                    .send(HostEvent::WorkspaceOpened {
                        requested,
                        root: path,
                    })
                    .await?;
                self.events
                    .send(HostEvent::Settings(self.settings.lock().await.clone()))
                    .await?;
            }
            HostCommand::OpenFile(request_id, path) => {
                let result = async {
                    let path = self.authorized_file(path.clone(), false).await?;
                    ensure!(
                        tokio::fs::metadata(&path).await?.len() <= 8 * 1024 * 1024,
                        "File exceeds editor preview limit"
                    );
                    Ok::<_, anyhow::Error>(tokio::fs::read_to_string(path).await?)
                }
                .await
                .map_err(|error| error.to_string());
                self.events
                    .send(HostEvent::OpenedFile {
                        request_id,
                        path,
                        result,
                    })
                    .await?;
            }
            HostCommand::Save(path, content) => {
                let result = self.save_project_file(path.clone(), content).await;
                let event = match result {
                    Ok(path) => HostEvent::Saved(path),
                    Err(error) => HostEvent::SaveFailed {
                        path,
                        message: error.to_string(),
                    },
                };
                self.events.send(event).await?;
            }
            HostCommand::GitStatus(request_id, root) => {
                let result = async {
                    self.authorized_git_root(&root).await?;
                    crate::workspace::git::status(&root).await
                }
                .await
                .map_err(|error: anyhow::Error| format!("{error:#}"));
                self.events
                    .send(HostEvent::GitStatus {
                        request_id,
                        root,
                        result,
                    })
                    .await?;
            }
            HostCommand::GitFileDiff(request_id, root, path, original, group) => {
                let result = async {
                    self.authorized_git_root(&root).await?;
                    crate::workspace::git::diff(&root, &path, original.as_deref(), group).await
                }
                .await
                .map_err(|error: anyhow::Error| format!("{error:#}"));
                self.events
                    .send(HostEvent::GitFileDiff {
                        request_id,
                        root,
                        result,
                    })
                    .await?;
            }
            HostCommand::MemorySearch(request_id, scope, query, before) => {
                let memory = self.memory.clone();
                let store = self.store.clone();
                let result = tokio::task::spawn_blocking(move || {
                    if query.trim().is_empty() {
                        store.memory_page(&scope, before, 50)
                    } else {
                        let records = memory.recall_sync(&scope, &query, 100)?;
                        Ok(crate::MemoryPage {
                            scope,
                            query,
                            records,
                            before: None,
                            has_more: false,
                        })
                    }
                })
                .await?
                .map_err(|error| error.to_string());
                self.events
                    .send(HostEvent::MemoryPage { request_id, result })
                    .await?;
                self.report_embedding_failure().await;
            }
            HostCommand::Remember(request_id, record) => {
                let memory = self.memory.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let receipt = crate::MemoryMutation {
                        scope: record.scope.clone(),
                        id: record.id,
                        deleted: false,
                    };
                    memory.remember_sync(record)?;
                    Ok::<_, anyhow::Error>(receipt)
                })
                .await?
                .map_err(|error| error.to_string());
                self.events
                    .send(HostEvent::MemoryMutation { request_id, result })
                    .await?;
                self.report_embedding_failure().await;
            }
            HostCommand::Connectors => {
                let store = self.store.clone();
                let rows = tokio::task::spawn_blocking(move || store.connectors()).await??;
                self.events.send(HostEvent::Connectors(rows)).await?;
            }
            HostCommand::SaveConnector(config) => {
                let store = self.store.clone();
                tokio::task::spawn_blocking(move || store.save_connector(&config)).await??;
                let store = self.store.clone();
                let rows = tokio::task::spawn_blocking(move || store.connectors()).await??;
                self.events.send(HostEvent::Connectors(rows)).await?;
            }
            HostCommand::MutateConnector(request_id, config, remove, secret) => {
                let id = remove
                    .or_else(|| config.as_ref().map(|config| config.id))
                    .context("Connector mutation has no target")?;
                let connectors = self.connectors()?;
                let _configuration = match connectors.configuration_guard(id).await {
                    Ok(guard) => guard,
                    Err(error) => {
                        self.events
                            .send(HostEvent::ConnectorMutation {
                                request_id,
                                result: Err(error.to_string()),
                            })
                            .await?;
                        return Ok(());
                    }
                };
                let store = self.store.clone();
                let vault = self.vault();
                let result =
                    tokio::task::spawn_blocking(move || -> Result<ConnectorMutationReceipt> {
                        if let Some(config) = &config {
                            if let Some(secret) = secret {
                                vault?.set(
                                    config
                                        .credential_ref
                                        .as_deref()
                                        .context("Missing connector credential reference")?,
                                    &secret,
                                )?;
                            }
                            store.save_connector(config)?;
                        }
                        if let Some(id) = remove {
                            store.delete_connector(id)?;
                        }
                        Ok(ConnectorMutationReceipt {
                            saved: config,
                            removed: remove,
                        })
                    })
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| result.map_err(|error| error.to_string()));
                let invalidated = result.as_ref().ok().and_then(|receipt| {
                    receipt
                        .removed
                        .or_else(|| receipt.saved.as_ref().map(|config| config.id))
                });
                self.events
                    .send(HostEvent::ConnectorMutation { request_id, result })
                    .await?;
                if let Some(id) = invalidated {
                    connectors.disconnect(id).await.context("Connection change saved, but ending its previous inspection session failed")?;
                }
            }
            HostCommand::ConnectorTools(request_id, id, workspace) => {
                let connectors = self.connectors()?;
                let _configuration = match connectors.configuration_guard(id).await {
                    Ok(guard) => guard,
                    Err(error) => {
                        self.events
                            .send(HostEvent::ConnectorToolsFailed {
                                request_id,
                                workspace,
                                connector_id: id,
                                message: error.to_string(),
                            })
                            .await?;
                        return Ok(());
                    }
                };
                let result = async {
                    let store = self.store.clone();
                    let configs = tokio::task::spawn_blocking(move || store.connectors()).await??;
                    let config = configs
                        .into_iter()
                        .find(|config| config.id == id)
                        .context("Connection no longer exists")?;
                    if config.transport == "stdio" {
                        let workspace = workspace
                            .clone()
                            .context("Choose a project before inspecting a local MCP connection")?;
                        let scope = xylo_services::ConnectorLaunchScope {
                            workspace,
                            protected_paths: vec![self.paths.data.clone()],
                            ..Default::default()
                        };
                        connectors
                            .tools_scoped(&config, &scope, &Default::default())
                            .await
                    } else {
                        connectors.tools(&config, &Default::default()).await
                    }
                }
                .await;
                let event = match result {
                    Ok(tools) => HostEvent::ConnectorTools {
                        request_id,
                        workspace,
                        connector_id: id,
                        tools,
                    },
                    Err(error) => HostEvent::ConnectorToolsFailed {
                        request_id,
                        workspace,
                        connector_id: id,
                        message: format!("{error:#}"),
                    },
                };
                self.events.send(event).await?;
            }
            HostCommand::Schedules => self.send_schedules().await?,
            HostCommand::CreateSchedule(prompt, cron, workspace) => {
                self.scheduler(workspace)?.create(prompt, cron).await?;
                self.send_schedules().await?;
            }
            HostCommand::CreateScheduleTracked(request_id, prompt, cron, workspace) => {
                let result = async {
                    self.scheduler(workspace)?.create(prompt, cron).await?;
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                self.schedule_mutated(request_id, result).await?;
            }
            HostCommand::SaveScheduleTracked(request_id, task) => {
                let result = async {
                    self.scheduler(task.workspace.clone())?.save(task).await?;
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                self.schedule_mutated(request_id, result).await?;
            }
            HostCommand::RemoveScheduleTracked(request_id, id) => {
                let result = async {
                    self.default_scheduler()?.remove(id).await?;
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                self.schedule_mutated(request_id, result).await?;
            }
            HostCommand::Capability(request_id, name, args) => {
                let cancel = self
                    .capability_requests
                    .lock()
                    .await
                    .get(&request_id)
                    .cloned()
                    .context("Action is no longer pending")?;
                let result = self.run_capability(request_id, &name, &args, cancel).await;
                self.capability_requests.lock().await.remove(&request_id);
                if let Err(error) = &result {
                    tracing::warn!(action=%name, %error, "Product action failed");
                }
                self.events
                    .send(HostEvent::Capability {
                        request_id,
                        name,
                        result: result.map_err(|error| error.to_string()),
                    })
                    .await?;
            }
            HostCommand::CancelCapability(id) => {
                if let Some(cancel) = self.capability_requests.lock().await.get(&id) {
                    cancel.cancel();
                }
            }
            HostCommand::Foreground(active) => self.foreground.store(active, Ordering::Release),
            HostCommand::Shutdown => {}
            HostCommand::Lsp(_) => {}
        }
        Ok(())
    }

    async fn run_capability(
        &self,
        request_id: Uuid,
        name: &str,
        args: &Value,
        cancel: CancellationToken,
    ) -> Result<Value> {
        ensure!(
            !self.stopping.load(Ordering::Acquire),
            "Application is closing"
        );
        ensure!(!cancel.is_cancelled(), "Action cancelled");
        if name.starts_with("extension_") {
            return crate::extensions::execute(
                self.paths.data.join("extensions"),
                self.store.clone(),
                self.vault(),
                request_id,
                name,
                args,
                cancel,
            )
            .await;
        }
        match name {
            "memory_embedding_status" => {
                return Ok(serde_json::to_value(self.memory.embedding_status())?);
            }
            "updates_status" => return Ok(self.updates.status()),
            "updates_check" => return self.updates.check(cancel).await,
            "updates_download" => return self.updates.stage(cancel).await,
            _ => {}
        }
        if name.starts_with("notifications_") {
            return self.notification_action(name, args, cancel).await;
        }
        if name.starts_with("learning_") {
            return crate::learning::execute(self.store.clone(), name, args, cancel).await;
        }
        if name.starts_with("account_chatgpt_") {
            let reference = args
                .get("credential_ref")
                .and_then(Value::as_str)
                .context("Select a ChatGPT account")?;
            match name {
                "account_chatgpt_sign_in" => {
                    let store = self.store.clone();
                    let host_id =
                        tokio::task::spawn_blocking(move || store.chatgpt_host_id()).await??;
                    let events = self.events.clone();
                    let account = self
                        .accounts
                        .sign_in(
                            &host_id,
                            self.vault()?,
                            reference,
                            cancel,
                            move |url| async move {
                                events
                                    .send(HostEvent::AuthorizationUrl(url))
                                    .await
                                    .context("The application window closed")
                            },
                        )
                        .await?;
                    return Ok(serde_json::to_value(account)?);
                }
                "account_chatgpt_status" => {
                    return Ok(serde_json::to_value(
                        self.accounts.account(self.vault()?, reference).await?,
                    )?);
                }
                "account_chatgpt_sign_out" => {
                    let warning = self
                        .accounts
                        .sign_out(self.vault()?, reference, &cancel)
                        .await?;
                    return Ok(serde_json::json!({"signed_out":true,"warning":warning}));
                }
                _ => anyhow::bail!("This account action is not supported"),
            }
        }
        let workspace = if name.starts_with("helper_audio_") {
            self.paths.data.clone()
        } else {
            self.settings
                .lock()
                .await
                .last_workspace
                .clone()
                .unwrap_or_else(|| self.paths.data.clone())
        };
        // Local reservations coordinate foreground host work. The helper also
        // acquires the shared process-wide pool and retains its OS lease inside
        // native blocking cleanup, even if this RPC is cancelled early.
        let _native_lease = if let Some(demand) = xylo_resources::capability_demand(name) {
            Some(self.resources.acquire(demand, cancel.clone()).await?)
        } else {
            None
        };
        let helper = self.helper_for_workspace(workspace, &cancel).await?;
        ensure!(!cancel.is_cancelled(), "Action cancelled");
        let result = helper.execute(name, args, cancel).await?;
        if name == "helper_asset_activate" {
            if result.get("embedding_changed").and_then(Value::as_bool) == Some(true) {
                let memory = self.memory.clone();
                let config = self.paths.data.join("embedding.json");
                tokio::task::spawn_blocking(move || memory.reload_configuration(&config)).await??;
            }
            if result
                .get("language_servers_changed")
                .and_then(Value::as_bool)
                == Some(true)
            {
                self.lsp.reload().await?;
            }
        }
        Ok(result)
    }

    async fn helper_for_workspace(
        &self,
        workspace: PathBuf,
        cancel: &CancellationToken,
    ) -> Result<HelperClient> {
        ensure!(!cancel.is_cancelled(), "Action cancelled");
        let workspace = tokio::fs::canonicalize(workspace).await?;
        let helper = {
            let mut helpers = self.helpers.lock().await;
            ensure!(
                !self.stopping.load(Ordering::Acquire),
                "Application is closing"
            );
            ensure!(!cancel.is_cancelled(), "Action cancelled");
            if let Some(helper) = helpers.get(&workspace).filter(|client| !client.is_closed()) {
                helper.clone()
            } else {
                if let Some(previous) = helpers.get(&workspace).cloned() {
                    previous.shutdown().await?;
                    helpers.remove(&workspace);
                }
                // Preserve helpers serving a running local chat or action. Once
                // quiescent, foreground project changes release old processes.
                if !model_activity_busy(&self.paths.data.join("local-inference-activity.lock"))? {
                    let idle: Vec<_> = helpers
                        .iter()
                        .filter(|(_, helper)| helper.pending_count() == 0)
                        .map(|(path, _)| path.clone())
                        .collect();
                    for path in idle {
                        if let Some(helper) = helpers.get(&path).cloned() {
                            helper.shutdown().await?;
                            helpers.remove(&path);
                        }
                    }
                }
                ensure!(
                    helpers.len() < 16,
                    "Finish current project actions before opening another tool session"
                );
                let helper = HelperClient::spawn(
                    native_executable("xylo-helper")?,
                    &self.paths.data,
                    &workspace,
                )
                .await?;
                helpers.insert(workspace, helper.clone());
                helper
            }
        };
        Ok(helper)
    }

    async fn authorized_git_root(&self, root: &std::path::Path) -> Result<()> {
        let selected = self
            .settings
            .lock()
            .await
            .last_workspace
            .clone()
            .context("Select a project first")?;
        ensure!(
            tokio::fs::canonicalize(root).await? == tokio::fs::canonicalize(selected).await?,
            "Git request leaves the selected project"
        );
        Ok(())
    }

    async fn authorized_file(&self, path: PathBuf, writing: bool) -> Result<PathBuf> {
        let target = if path.exists() {
            tokio::fs::canonicalize(&path).await?
        } else {
            ensure!(writing, "File does not exist");
            let parent = tokio::fs::canonicalize(path.parent().context("Missing parent")?).await?;
            parent.join(path.file_name().context("Missing filename")?)
        };
        if !writing && target.starts_with(tokio::fs::canonicalize(&self.paths.artifacts).await?) {
            return Ok(target);
        }
        let root = self
            .settings
            .lock()
            .await
            .last_workspace
            .clone()
            .context("Select a project first")?;
        ensure!(
            target.starts_with(tokio::fs::canonicalize(root).await?),
            "File leaves selected workspace"
        );
        Ok(target)
    }

    async fn save_project_file(&self, path: PathBuf, content: String) -> Result<PathBuf> {
        let path = self.authorized_file(path, true).await?;
        ensure!(
            content.len() <= 8 * 1024 * 1024,
            "File exceeds editor save limit"
        );
        let temp = path.with_file_name(format!(".xylo-{}.tmp", Uuid::new_v4()));
        let result = async {
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
                .await?;
            if let Ok(existing) = tokio::fs::metadata(&path).await {
                file.set_permissions(existing.permissions()).await?;
            }
            file.write_all(content.as_bytes()).await?;
            file.sync_all().await?;
            drop(file);
            tokio::fs::rename(&temp, &path).await?;
            #[cfg(unix)]
            tokio::fs::File::open(path.parent().context("Missing project directory")?)
                .await?
                .sync_all()
                .await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&temp).await;
        }
        result?;
        Ok(path)
    }

    async fn import_image_file(
        &self,
        request_id: Uuid,
        session_id: Uuid,
        path: PathBuf,
        source: AttachmentSource,
    ) -> Result<(ImageAttachment, PathBuf)> {
        let cancel = self
            .capability_requests
            .lock()
            .await
            .get(&request_id)
            .cloned()
            .context("Image import is no longer pending")?;
        let _decode = tokio::select! {
            _ = cancel.cancelled() => anyhow::bail!("Image import cancelled"),
            guard = self.image_import_gate.lock() => guard,
        };
        let _lease = self
            .resources
            .acquire(
                TaskDemand {
                    memory_bytes: 384 * 1024 * 1024,
                    cpu_share: 1.0,
                },
                cancel.clone(),
            )
            .await?;
        let artifacts = self.paths.artifacts.clone();
        tokio::task::spawn_blocking(move || {
            let images = xylo_services::attachments::AttachmentStore::new(artifacts)?;
            let attachment = images.import_file(session_id, path, source, &cancel)?;
            let preview = images.preview_path(&attachment)?;
            Ok((attachment, preview))
        })
        .await?
    }

    async fn worker_command(
        self: &Arc<Self>,
        target: Option<Uuid>,
        command: WorkerCommand,
    ) -> Result<()> {
        self.worker_command_with_draft_and_receipt(target, command, None, None)
            .await
            .map(|_| ())
    }

    async fn publish_draft_admission(
        &self,
        request_id: Option<Uuid>,
        receipt: &Option<crate::drafts::DraftAdmission>,
    ) {
        if let (Some(request_id), Some(receipt)) = (request_id, receipt) {
            // Commit owns admission. A closed UI receiver must never prevent the
            // reserved Run from dispatching after its user message was committed.
            if self
                .events
                .send(HostEvent::DraftAdmitted {
                    request_id,
                    result: Ok(receipt.clone()),
                })
                .await
                .is_err()
            {
                tracing::debug!("Draft admission receiver closed after durable commit");
            }
        }
    }

    async fn worker_command_with_draft_and_receipt(
        self: &Arc<Self>,
        target: Option<Uuid>,
        command: WorkerCommand,
        submission: Option<crate::drafts::DraftSubmission>,
        draft_request_id: Option<Uuid>,
    ) -> Result<Option<crate::drafts::DraftAdmission>> {
        self.worker_command_with_queue_receipt(target, command, submission, draft_request_id, None)
            .await
    }

    async fn worker_command_with_queue_receipt(
        self: &Arc<Self>,
        target: Option<Uuid>,
        command: WorkerCommand,
        submission: Option<crate::drafts::DraftSubmission>,
        draft_request_id: Option<Uuid>,
        queued_item: Option<QueuedAdmission>,
    ) -> Result<Option<crate::drafts::DraftAdmission>> {
        match command {
            WorkerCommand::Run { turn } => {
                self.admit_run(turn, submission, draft_request_id, queued_item)
                    .await
            }
            command => {
                self.control_worker(target, command).await?;
                Ok(None)
            }
        }
    }

    /// Delivers a UI control command (Approve, guidance, Shutdown) to a worker.
    /// Stop never arrives here: [`HostState::capture_stop`] owns it.
    async fn control_worker(&self, target: Option<Uuid>, command: WorkerCommand) -> Result<()> {
        ensure!(
            !matches!(
                command,
                WorkerCommand::RunAuthorized { .. }
                    | WorkerCommand::ProviderCredentialUpdated { .. }
                    | WorkerCommand::Unlock { .. }
                    | WorkerCommand::SetPermissions { .. }
                    | WorkerCommand::Hello
                    | WorkerCommand::Cancel
            ),
            "This command is managed by the application host"
        );
        let session = match target {
            Some(session) => session,
            None => {
                let active = self.turn_queue.active_sessions();
                ensure!(
                    active.len() == 1,
                    "Select a single active task for this control action"
                );
                active[0]
            }
        };
        let worker = self
            .workers
            .lock()
            .await
            .get(&session)
            .filter(|worker| !worker.dead())
            .cloned();
        if let WorkerCommand::Approve {
            action_id,
            allow,
            remember,
        } = command
        {
            let Some(worker) = worker else {
                return self.report_lost_decision(session, action_id, None).await;
            };
            return self
                .decide(session, &worker, action_id, allow, remember)
                .await;
        }
        let worker = worker.context("This task is no longer running")?;
        worker.send_control(session, Uuid::new_v4(), command).await
    }

    /// Forwards a decision and, for "always allow", saves the project rule.
    /// Only the first decision for an action counts.
    async fn decide(
        &self,
        session: Uuid,
        worker: &WorkerHandle,
        action_id: Uuid,
        allow: bool,
        remember: bool,
    ) -> Result<()> {
        let request = Uuid::new_v4();
        let first = worker
            .approvals
            .lock()
            .get_mut(&action_id)
            .and_then(|pending| {
                pending.decision.is_none().then(|| {
                    pending.decision = Some(request);
                    pending.clone()
                })
            });
        let command = WorkerCommand::Approve {
            action_id,
            allow,
            remember,
        };
        if worker
            .send_control(session, request, command)
            .await
            .is_err()
        {
            let pending = worker.approvals.lock().remove(&action_id);
            return self.report_lost_decision(session, action_id, pending).await;
        }
        let workspace = worker.workspace.lock().clone();
        if allow
            && remember
            && let Some(pending) = first
            && let Some(key) = pending.rule_key
            && let Some(workspace) = workspace
        {
            let rule = ApprovalRule {
                label: pending.rule.unwrap_or_else(|| key.clone()),
                key,
                workspace,
                created_at: now_ms(),
            };
            let store = self.store.clone();
            let saved = tokio::task::spawn_blocking(move || store.save_approval_rule(&rule)).await;
            if !matches!(saved, Ok(Ok(()))) {
                tracing::warn!(?saved, "Save standing approval");
                self.events
                    .send(HostEvent::Error(
                        "The action was allowed, but \"always allow\" could not be saved for this project."
                            .into(),
                    ))
                    .await?;
            }
        }
        Ok(())
    }

    /// Admits one Run: reserve, prepare (credentials, resources, worker), make
    /// the user message durable, then dispatch. A Run that does not dispatch
    /// after a Stop reports exactly one `Cancelled` for its request.
    async fn admit_run(
        self: &Arc<Self>,
        mut turn: TurnRequest,
        submission: Option<crate::drafts::DraftSubmission>,
        draft_request_id: Option<Uuid>,
        queued_item: Option<QueuedAdmission>,
    ) -> Result<Option<crate::drafts::DraftAdmission>> {
        let session = turn.session_id;
        let cancellation = {
            let mut queued = self.queued.lock();
            match queued.get(&session) {
                Some((id, cancel)) => {
                    ensure!(*id == turn.turn_id, "This task is already waiting to start");
                    cancel.clone()
                }
                None => {
                    let cancel = CancellationToken::new();
                    queued.insert(session, (turn.turn_id, cancel.clone()));
                    cancel
                }
            }
        };
        let _pending = PendingRun {
            session,
            id: turn.turn_id,
            pending: self.queued.clone(),
        };
        let request_id = Uuid::new_v4();
        let outcome = self
            .admit_reserved(
                request_id,
                &mut turn,
                &cancellation,
                submission,
                draft_request_id,
                &queued_item,
            )
            .await;
        let delivered = matches!(
            outcome,
            Ok(Admission::Dispatched(_) | Admission::Replayed(_))
        );
        if !delivered && cancellation.is_cancelled() {
            let _ = self
                .events
                .send(
                    self.sequences
                        .engine(request_id, session, EngineEvent::Cancelled),
                )
                .await;
        }
        Ok(match outcome? {
            Admission::Dispatched(receipt) | Admission::Replayed(receipt) => receipt,
            Admission::Cancelled => None,
        })
    }

    async fn admit_reserved(
        self: &Arc<Self>,
        request_id: Uuid,
        turn: &mut TurnRequest,
        cancellation: &CancellationToken,
        submission: Option<crate::drafts::DraftSubmission>,
        draft_request_id: Option<Uuid>,
        queued_item: &Option<QueuedAdmission>,
    ) -> Result<Admission> {
        let session = turn.session_id;
        if cancellation.is_cancelled() {
            return Ok(Admission::Cancelled);
        }
        // Show activity before credentials, resources or a local model start.
        // Best effort: a closed UI receiver must not block admission.
        let _ = self
            .events
            .send(self.sequences.engine(
                request_id,
                session,
                EngineEvent::Phase {
                    phase: TurnPhase::Preparing,
                },
            ))
            .await;
        // A chat's durable action provenance is bound to its original
        // canonical workspace. Reject redirection before admission or any
        // model/helper/worker dispatch, including a completed-turn replay.
        let workspace = tokio::fs::canonicalize(&turn.workspace)
            .await
            .context("The selected project is unavailable")?;
        ensure!(workspace.is_dir(), "The selected project is unavailable");
        let store = self.store.clone();
        if let Some(conversation) =
            tokio::task::spawn_blocking(move || store.conversation(session)).await??
        {
            ensure!(
                tokio::fs::canonicalize(&conversation.workspace).await? == workspace,
                "This chat belongs to another project"
            );
        }
        // Compare canonical identities without rewriting the submitted
        // spelling: draft CAS and durable admission bind the exact scoped
        // request, which may legitimately use an equivalent path alias.
        ensure!(
            !self.stopping.load(Ordering::Acquire),
            "Application is closing"
        );
        if let Some(receipt) = self
            .replay_completed(turn, submission.clone(), draft_request_id, queued_item)
            .await?
        {
            return Ok(Admission::Replayed(receipt));
        }
        turn.provider.validate()?;
        let credential = WorkerCredential(
            self.accounts
                .resolve_provider_credential(&turn.provider, self.vault()?, cancellation)
                .await?,
        );
        let scheduled = self.scheduled.lock().await.contains_key(&session);
        let store = self.store.clone();
        let rules_workspace = workspace.clone();
        turn.approved_rules =
            tokio::task::spawn_blocking(move || store.approval_rules(&rules_workspace))
                .await??
                .into_iter()
                .map(|rule| rule.key)
                .collect();
        {
            let mut workers = self.workers.lock().await;
            ensure!(
                !workers
                    .get(&session)
                    .is_some_and(|w| w.active.load(Ordering::Acquire)),
                "This task is already working"
            );
            retire_idle_workers(&mut workers, Some(session), std::time::Instant::now());
        }
        let lease = self
            .resources
            .acquire(
                TaskDemand::interactive(turn.provider.context_tokens),
                cancellation.clone(),
            )
            .await?;
        if cancellation.is_cancelled() {
            return Ok(Admission::Cancelled);
        }
        let model_activity = if turn.provider.managed_local {
            Some(self.start_local_model(turn, cancellation).await?)
        } else {
            None
        };
        if cancellation.is_cancelled() {
            return Ok(Admission::Cancelled);
        }
        self.ready_worker(session).await?;
        let workers = self.workers.lock().await;
        ensure!(
            !self.stopping.load(Ordering::Acquire),
            "Application is closing"
        );
        if cancellation.is_cancelled() {
            return Ok(Admission::Cancelled);
        }
        let worker = workers
            .get(&session)
            .filter(|worker| !worker.dead())
            .context("Task could not start")?
            .clone();
        ensure!(
            !worker.active.load(Ordering::Acquire),
            "This task is already working"
        );
        let permit = worker
            .commands
            .clone()
            .try_reserve_owned()
            .context("This task cannot accept another message yet")?;
        ensure!(
            !cancellation.is_cancelled() && !self.stopping.load(Ordering::Acquire),
            "Task cancelled before admission"
        );
        let automatic = queued_item
            .as_ref()
            .is_some_and(|(_, _, automatic)| *automatic);
        // An explicit fresh Run may resume a paused chat. Never reset this
        // latch after an await: Stop during durable commit must survive.
        if !automatic {
            worker.queue_stopped.store(false, Ordering::Release);
        }
        let attachments = turn.attachments.clone();
        let artifacts = self.paths.artifacts.clone();
        tokio::task::spawn_blocking(move || {
            xylo_services::attachments::AttachmentStore::new(artifacts)?
                .validate(session, &attachments)
        })
        .await??;
        ensure!(
            !cancellation.is_cancelled(),
            "Task cancelled before admission"
        );
        // Settings are the source of truth: every admission, queued or not,
        // runs with the current mode (unattended scheduled turns never with
        // Full access). Later changes reach the worker as
        // SetPermissions, ordered after this Run by the workers lock.
        turn.permissions = effective_mode(self.settings.lock().await.permissions, scheduled);
        *worker.mode.lock() = turn.permissions;
        if automatic {
            ensure!(
                !worker.queue_stopped.load(Ordering::Acquire),
                "Task stopped before admission"
            );
        }
        let receipt = self.commit_admission(turn, submission, queued_item).await?;
        // Publish the durable user message before this Run can emit a reply.
        self.publish_draft_admission(draft_request_id, &receipt)
            .await;
        self.publish_queued_admission(queued_item).await;
        // The durable user turn and conditional draft clear have committed.
        // Always dispatch the reserved Run, even if cancellation arrived
        // during SQLite commit; a following Stop ends that exact run.
        *worker.run_request.lock() = Some(request_id);
        *worker.run_turn.lock() = Some(turn.turn_id);
        *worker.workspace.lock() = Some(workspace);
        *worker.last_used.lock() = std::time::Instant::now();
        worker.queue_epoch.fetch_add(1, Ordering::AcqRel);
        worker.active.store(true, Ordering::Release);
        *worker.lease.lock().await = Some(lease);
        *worker.model_activity.lock().await = model_activity;
        // Add authentication only after durable admission. Only the private
        // worker pipe sees this envelope; the persisted TurnRequest has refs.
        *worker.credentials.lock() = Some(Arc::new(credential_control::RunCredentials::new(
            request_id,
            turn.provider.clone(),
            credential.clone(),
            worker.stop.child_token(),
        )));
        let command = WorkerCommand::RunAuthorized {
            turn: turn.clone(),
            credential,
        };
        permit.send(Envelope::new(request_id, Some(session), 0, command));
        let cancelled = {
            let mut starting = self.queued.lock();
            starting.remove(&session);
            cancellation.is_cancelled() || self.stopping.load(Ordering::Acquire)
        };
        drop(workers);
        if cancelled {
            self.stop_active_run(session).await;
        }
        if let Err(error) = self.send_conversations().await {
            tracing::debug!(%error, "Refresh admitted conversation list");
        }
        Ok(Admission::Dispatched(receipt))
    }

    /// A retried turn that already completed is answered from the journal.
    async fn replay_completed(
        &self,
        turn: &TurnRequest,
        submission: Option<crate::drafts::DraftSubmission>,
        draft_request_id: Option<Uuid>,
        queued_item: &Option<QueuedAdmission>,
    ) -> Result<Option<Option<crate::drafts::DraftAdmission>>> {
        let session = turn.session_id;
        let turn_id = turn.turn_id;
        let store = self.store.clone();
        let Some(completed) =
            tokio::task::spawn_blocking(move || store.completed_turn(session, turn_id)).await??
        else {
            return Ok(None);
        };
        self.complete_schedule(session).await?;
        let receipt = match submission {
            Some(submission) => {
                Some(crate::drafts::admit_turn(self.store.clone(), turn.clone(), submission).await?)
            }
            None => None,
        };
        if let Some((item, admitted, _)) = queued_item {
            let store = self.store.clone();
            let item = item.clone();
            tokio::task::spawn_blocking(move || {
                store.admit_queued_turn(item.turn.session_id, item.id, &item.turn)
            })
            .await??;
            admitted.store(true, Ordering::Release);
        }
        self.publish_draft_admission(draft_request_id, &receipt)
            .await;
        self.publish_queued_admission(queued_item).await;
        self.events
            .send(self.sequences.engine(
                turn_id,
                session,
                EngineEvent::Completed {
                    text: completed.text,
                    usage: completed.usage,
                },
            ))
            .await?;
        Ok(Some(receipt))
    }

    /// Makes the user message durable: a draft, a queued item or a plain turn.
    async fn commit_admission(
        &self,
        turn: &TurnRequest,
        submission: Option<crate::drafts::DraftSubmission>,
        queued_item: &Option<QueuedAdmission>,
    ) -> Result<Option<crate::drafts::DraftAdmission>> {
        if let Some(submission) = submission {
            return Ok(Some(
                crate::drafts::admit_turn(self.store.clone(), turn.clone(), submission).await?,
            ));
        }
        let store = self.store.clone();
        let turn = turn.clone();
        let queued = queued_item.clone();
        tokio::task::spawn_blocking(move || match queued {
            Some((item, admitted, _)) => store
                .admit_queued_turn(turn.session_id, item.id, &turn)
                .map(|_| admitted.store(true, Ordering::Release)),
            None => store.register_user_turn(&turn),
        })
        .await??;
        Ok(None)
    }

    /// Starts the application-managed local model and points the turn at it.
    async fn start_local_model(
        &self,
        turn: &mut TurnRequest,
        cancellation: &CancellationToken,
    ) -> Result<ModelActivityLease> {
        let helper = self
            .helper_for_workspace(turn.workspace.clone(), cancellation)
            .await?;
        let activity = acquire_model_activity(
            self.paths.data.join("local-inference-activity.lock"),
            cancellation.clone(),
        )
        .await?;
        let value = helper
            .execute(
                "local_inference_start",
                &serde_json::json!({}),
                cancellation.clone(),
            )
            .await?;
        let summary: Value = serde_json::from_str(
            value
                .get("summary")
                .and_then(Value::as_str)
                .context("Local model did not return connection information")?,
        )?;
        turn.provider.endpoint = summary
            .get("endpoint")
            .and_then(Value::as_str)
            .context("Local model did not become available")?
            .to_owned();
        Ok(activity)
    }

    /// Ensures a live worker for `session` is in the map. A dead one is reaped
    /// first; neither the reap wait nor the spawn holds the workers lock.
    async fn ready_worker(self: &Arc<Self>, session: Uuid) -> Result<()> {
        loop {
            let dead = {
                let workers = self.workers.lock().await;
                match workers.get(&session) {
                    Some(worker) if worker.dead() => Some(worker.clone()),
                    Some(_) => return Ok(()),
                    None => None,
                }
            };
            match dead {
                Some(worker) => {
                    worker.stop.cancel();
                    let mut exited = worker.exited.clone();
                    tokio::time::timeout(Duration::from_secs(8), exited.wait_for(|exited| *exited))
                        .await
                        .context("The previous task is still stopping")?
                        .ok();
                    let mut workers = self.workers.lock().await;
                    if workers
                        .get(&session)
                        .is_some_and(|current| current.exited.same_channel(&worker.exited))
                    {
                        workers.remove(&session);
                    }
                }
                None => {
                    let spawned = self.spawn_worker(session).await?;
                    let mut workers = self.workers.lock().await;
                    match workers.entry(session) {
                        std::collections::hash_map::Entry::Vacant(entry) => {
                            entry.insert(spawned);
                            return Ok(());
                        }
                        std::collections::hash_map::Entry::Occupied(_) => spawned.retire(session),
                    }
                }
            }
        }
    }

    /// Persists a complete settings snapshot and applies its live effects. A
    /// changed secret storage switches the vault; when that fails the
    /// previous choice is restored, and when the vault turns out to keep its
    /// key elsewhere the setting follows the vault. A changed mode reaches
    /// running tasks.
    async fn commit_settings(&self, settings: AppSettings) -> Result<()> {
        let previous = self.settings.lock().await.clone();
        self.save_settings_snapshot(&settings).await?;
        let mut applied = settings.clone();
        match self.apply_secret_storage(settings.secret_storage).await {
            Ok(actual) if actual == settings.secret_storage => {}
            Ok(actual) => {
                applied.secret_storage = actual;
                self.save_settings_snapshot(&applied).await?;
                self.events
                    .send(HostEvent::Settings(applied.clone()))
                    .await?;
            }
            Err(error) if settings.secret_storage == previous.secret_storage => {
                // The vault is still unavailable; the rest of the change stands.
                self.events
                    .send(HostEvent::Error(format!("{error:#}")))
                    .await?;
            }
            Err(error) => {
                // Never leave the saved choice and the live vault disagreeing.
                applied.secret_storage = previous.secret_storage;
                self.save_settings_snapshot(&applied).await?;
                if applied.permissions != previous.permissions {
                    self.forward_permissions(applied.permissions).await;
                }
                self.events.send(HostEvent::Settings(applied)).await?;
                return Err(error);
            }
        }
        if applied.permissions != previous.permissions {
            self.forward_permissions(applied.permissions).await;
        }
        Ok(())
    }

    /// Saves a settings snapshot and makes it current; memory and the
    /// database never disagree.
    async fn save_settings_snapshot(&self, settings: &AppSettings) -> Result<()> {
        let mut current = self.settings.lock().await;
        let store = self.store.clone();
        let value = settings.clone();
        tokio::task::spawn_blocking(move || store.save_settings(&value)).await??;
        *current = settings.clone();
        Ok(())
    }

    /// Switches the vault to `storage` (key file or keychain item) after the
    /// setting is saved and returns the storage now in use;
    /// [`Self::commit_settings`] restores the setting when this fails. The
    /// switch starts an empty vault; nothing old is kept. A vault that could
    /// not be opened at startup is opened again (see [`reopen_secrets`]).
    async fn apply_secret_storage(&self, storage: SecretStorage) -> Result<SecretStorage> {
        let current = match &*self.secrets.read() {
            Ok(secrets) if secrets.storage == storage => return Ok(storage),
            Ok(secrets) => Some(secrets.vault.clone()),
            Err(_) => None,
        };
        let data = self.paths.data.clone();
        let Some(vault) = current else {
            let store = self.store.clone();
            let (secrets, started_over) =
                tokio::task::spawn_blocking(move || reopen_secrets(&data, storage, &store))
                    .await??;
            let actual = secrets.storage;
            let notice = if started_over {
                Some(STORAGE_CHANGED_NOTICE.into())
            } else {
                secrets.vault.reset_notice()
            };
            *self.secrets.write() = Ok(secrets);
            if let Some(notice) = notice {
                self.events.send(HostEvent::Status(notice)).await?;
            }
            return Ok(actual);
        };
        tokio::task::spawn_blocking(move || vault.change_storage(&data, storage))
            .await?
            .context("Secret storage could not be changed")?;
        if let Ok(secrets) = &mut *self.secrets.write() {
            secrets.storage = storage;
        }
        self.retire_workers_for_new_key().await;
        self.events
            .send(HostEvent::Status(STORAGE_CHANGED_NOTICE.into()))
            .await?;
        Ok(storage)
    }

    /// Tells the user once when semantic memory stopped working.
    async fn report_embedding_failure(&self) {
        if let Some(message) = self.memory.take_embedding_failure() {
            let _ = self.events.send(HostEvent::Status(message)).await;
        }
    }

    fn vault(&self) -> Result<CredentialVault> {
        match &*self.secrets.read() {
            Ok(secrets) => Ok(secrets.vault.clone()),
            Err(error) => anyhow::bail!("Saved keys are unavailable: {error}"),
        }
    }

    fn connectors(&self) -> Result<Arc<ConnectorBroker>> {
        match &*self.secrets.read() {
            Ok(secrets) => Ok(secrets.connectors.clone()),
            Err(error) => anyhow::bail!("Saved keys are unavailable: {error}"),
        }
    }

    /// Canonical identity of a project for standing approvals; a project that
    /// is no longer on disk keeps the spelling it was saved under.
    async fn approval_workspace(workspace: &std::path::Path) -> PathBuf {
        tokio::fs::canonicalize(workspace)
            .await
            .unwrap_or_else(|_| workspace.to_owned())
    }

    async fn approval_rules(
        &self,
        workspace: &std::path::Path,
    ) -> std::result::Result<Vec<ApprovalRule>, String> {
        let workspace = Self::approval_workspace(workspace).await;
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || store.approval_rules(&workspace))
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result.map_err(|error| format!("{error:#}")))
    }

    /// Revokes a standing approval. Rules live in the host and seed each
    /// turn through `TurnRequest.approved_rules`; a worker keeps them only
    /// for that turn, so the rule stops applying at the project's next turn
    /// while a running turn keeps the set it started with. Workers of the
    /// project are also retired (idle ones now, running ones when their turn
    /// ends) as defense in depth, so no process that held the rule is reused.
    async fn revoke_approval_rule(&self, workspace: &std::path::Path, key: String) -> Result<()> {
        let workspace = Self::approval_workspace(workspace).await;
        let store = self.store.clone();
        let removed = workspace.clone();
        tokio::task::spawn_blocking(move || store.remove_approval_rule(&removed, &key)).await??;
        let workers = self.workers.lock().await;
        for (session, worker) in workers.iter() {
            if worker.workspace.lock().as_deref() == Some(workspace.as_path()) {
                worker.retire_when_idle(*session);
            }
        }
        Ok(())
    }

    async fn rename_conversation(&self, id: Uuid, title: String) -> Result<()> {
        let title = title.trim().to_owned();
        ensure!(
            (1..=120).contains(&title.chars().count()),
            "Titles must be 1 to 120 characters"
        );
        let store = self.store.clone();
        let conversation = tokio::task::spawn_blocking(move || {
            store.rename_conversation(id, &title, TitleSource::User)
        })
        .await??
        .context("This chat no longer exists")?;
        self.events
            .send(HostEvent::ConversationRenamed(conversation))
            .await?;
        self.send_conversations().await
    }

    /// Removes every saved model of one connection. The active model moves to
    /// the next saved one (or the default), and the connection's secret is
    /// deleted after settings are saved when nothing else uses it.
    async fn remove_provider(&self, provider: ProviderConfig) -> Result<()> {
        let mut settings = self.settings.lock().await.clone();
        let saved = settings.providers.len();
        settings
            .providers
            .retain(|candidate| !same_connection(candidate, &provider));
        let active_removed = same_connection(&settings.provider, &provider);
        ensure!(
            saved != settings.providers.len() || active_removed,
            "This connection is no longer saved"
        );
        if active_removed {
            settings.provider = settings.providers.first().cloned().unwrap_or_default();
        }
        // Account sign-ins are revoked by signing out, not by removing a model.
        let orphaned = provider.credential_ref.clone().filter(|reference| {
            !reference.starts_with("account:")
                && !settings
                    .providers
                    .iter()
                    .chain([&settings.provider])
                    .any(|other| other.credential_ref.as_deref() == Some(reference.as_str()))
        });
        self.commit_settings(settings.clone()).await?;
        self.events.send(HostEvent::Settings(settings)).await?;
        if let Some(reference) = orphaned {
            let vault = self.vault()?;
            let deleted = tokio::task::spawn_blocking(move || vault.delete(&reference)).await;
            if !matches!(deleted, Ok(Ok(()))) {
                tracing::warn!(?deleted, "Delete removed provider secret");
                self.events
                    .send(HostEvent::Error(
                        "The connection was removed, but its saved key could not be deleted."
                            .into(),
                    ))
                    .await?;
            }
        }
        Ok(())
    }

    async fn discover_models(&self, generation: u64, config: ProviderConfig) -> Result<()> {
        let Some(cancel) = self.discovery.token(generation) else {
            return Ok(());
        };
        let result = async {
            let vault = self.vault()?;
            self.accounts
                .prepare_provider_credential(&config, vault.clone(), &cancel)
                .await?;
            xylo_services::providers::discover_provider_models(&config, &vault, &cancel).await
        }
        .await;
        // Superseded requests must not emit either a catalog or an error that
        // could replace the newer provider's state, including after auth changes.
        if cancel.is_cancelled() || self.discovery.generation() != generation {
            return Ok(());
        }
        let catalog = match result {
            Ok(catalog) => catalog,
            Err(error) => {
                tokio::select! {
                    _ = cancel.cancelled() => {},
                    result = self.events.send(HostEvent::ModelsFailed { generation, message: provider_control::discovery_error(&config, error), provider: config }) => { result?; },
                }
                return Ok(());
            }
        };
        let context_limits = catalog
            .iter()
            .filter_map(|m| m.context_tokens.map(|n| (m.id.clone(), n)))
            .collect();
        let reasoning_options = catalog
            .iter()
            .filter_map(|m| {
                m.reasoning_options
                    .clone()
                    .map(|options| (m.id.clone(), options))
            })
            .collect();
        let default_reasoning_efforts = catalog
            .iter()
            .filter_map(|m| {
                m.default_reasoning_effort
                    .clone()
                    .map(|effort| (m.id.clone(), effort))
            })
            .collect();
        let models = catalog.into_iter().map(|m| m.id).collect();
        tokio::select! {
            _ = cancel.cancelled() => {},
            result = self.events.send(HostEvent::Models { generation, provider: config, models, context_limits, reasoning_options, default_reasoning_efforts }) => { result?; },
        }
        Ok(())
    }

    async fn shutdown(&self) {
        // Cancel resource waiters before waiting for an installation writer.
        // Never cancel the installer once it owns its exclusive replacement gate.
        for (_, cancel) in self.queued.lock().values() {
            cancel.cancel();
        }
        for cancel in self.search_requests.lock().await.values() {
            cancel.cancel();
        }
        let installation = *self.install_request.lock();
        for (id, cancel) in self.capability_requests.lock().await.iter() {
            if Some(*id) != installation {
                cancel.cancel();
            }
        }
        self.stopping.store(true, Ordering::Release);
        let _operations = self.operations.write().await;
        self.workspace.shutdown();
        for token in self.search_requests.lock().await.values() {
            token.cancel();
        }
        self.lsp.shutdown().await;
        for (_, cancel) in self.queued.lock().values() {
            cancel.cancel();
        }
        for cancel in self.capability_requests.lock().await.values() {
            cancel.cancel();
        }
        let workers: Vec<_> = self.workers.lock().await.drain().collect();
        futures_util::future::join_all(workers.into_iter().map(
            |(session, mut worker)| async move {
                let _ = tokio::time::timeout(
                    Duration::from_secs(1),
                    worker.commands.send(Envelope::new(
                        Uuid::new_v4(),
                        Some(session),
                        0,
                        WorkerCommand::Shutdown,
                    )),
                )
                .await;
                if !*worker.exited.borrow()
                    && tokio::time::timeout(Duration::from_secs(6), worker.exited.changed())
                        .await
                        .is_err()
                {
                    worker.stop.cancel();
                    let _ =
                        tokio::time::timeout(Duration::from_secs(2), worker.exited.changed()).await;
                }
            },
        ))
        .await;
        let helpers: Vec<_> = self
            .helpers
            .lock()
            .await
            .drain()
            .map(|(_, helper)| helper)
            .collect();
        futures_util::future::join_all(helpers.iter().map(HelperClient::shutdown)).await;
    }

    async fn complete_schedule(&self, session: Uuid) -> Result<()> {
        acknowledge_schedule(&self.default_scheduler()?, &self.scheduled, session).await
    }

    fn scheduler(&self, workspace: PathBuf) -> Result<crate::scheduler::Scheduler> {
        crate::scheduler::Scheduler::new(self.paths.artifacts.join("schedules.json"), workspace)
    }

    fn default_scheduler(&self) -> Result<crate::scheduler::Scheduler> {
        self.scheduler(self.paths.data.clone())
    }

    async fn schedule_mutated(&self, request_id: Uuid, result: Result<()>) -> Result<()> {
        let success = result.is_ok();
        self.events
            .send(HostEvent::ScheduleMutation {
                request_id,
                result: result.map_err(|error| error.to_string()),
            })
            .await?;
        // The durable acknowledgement is independent of a subsequent list refresh.
        if success {
            self.send_schedules().await?;
        }
        Ok(())
    }
    async fn send_schedules(&self) -> Result<()> {
        self.events
            .send(HostEvent::Schedules(
                self.default_scheduler()?.list().await?,
            ))
            .await?;
        Ok(())
    }

    async fn schedule_loop(self: Arc<Self>) {
        let scheduler = match self.default_scheduler() {
            Ok(scheduler) => scheduler,
            Err(error) => {
                tracing::error!(%error,"Initialize schedules");
                return;
            }
        };
        let mut reported_interruptions = std::collections::HashSet::new();
        loop {
            let operation = self.operations.read().await;
            if self.installing.load(Ordering::Acquire) {
                drop(operation);
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
            self.lsp.release_idle().await;
            if self.stopping.load(Ordering::Acquire) {
                break;
            }
            retire_idle_workers(
                &mut *self.workers.lock().await,
                None,
                std::time::Instant::now(),
            );
            self.report_embedding_failure().await;
            let scheduled = self.scheduled.lock().await.clone();
            for (session, id) in scheduled {
                let queued = self.queued.lock().contains_key(&session);
                if queued
                    || self
                        .workers
                        .lock()
                        .await
                        .get(&session)
                        .is_some_and(|worker| worker.active.load(Ordering::Acquire))
                {
                    if let Err(error) = scheduler.renew(id).await {
                        tracing::warn!(%error,"Renew active schedule");
                    }
                } else {
                    let store = self.store.clone();
                    let completed =
                        tokio::task::spawn_blocking(move || store.completed_turn(session, session))
                            .await;
                    let result = match completed {
                        Ok(Ok(Some(_))) => self.complete_schedule(session).await,
                        Ok(Ok(None)) => scheduler.pause(id).await,
                        Ok(Err(error)) => Err(error),
                        Err(error) => Err(error.into()),
                    };
                    if result.is_ok() {
                        self.scheduled.lock().await.remove(&session);
                    } else {
                        tracing::warn!(?result, "Retain scheduled occurrence for recovery");
                    }
                }
            }
            self.report_interrupted_schedules(&scheduler, &mut reported_interruptions)
                .await;
            match scheduler.claim_due().await {
                Ok(tasks) => {
                    for task in tasks {
                        let Some(occurrence) = task.claim.map(|claim| claim.occurrence) else {
                            continue;
                        };
                        if self
                            .scheduled
                            .lock()
                            .await
                            .values()
                            .any(|id| *id == task.id)
                        {
                            continue;
                        }
                        self.scheduled.lock().await.insert(occurrence, task.id);
                        self.queued
                            .lock()
                            .insert(occurrence, (occurrence, CancellationToken::new()));
                        let (provider, permissions) = {
                            let settings = self.settings.lock().await;
                            (settings.provider.clone(), settings.permissions)
                        };
                        // Admission fills the current mode and standing approvals.
                        let turn = TurnRequest {
                            session_id: occurrence,
                            turn_id: occurrence,
                            prompt: task.prompt,
                            workspace: task.workspace,
                            provider,
                            permissions,
                            attachments: Vec::new(),
                            approved_rules: Vec::new(),
                        };
                        let state = self.clone();
                        let scheduler = scheduler.clone();
                        tokio::spawn(async move {
                            let _operation = state.operations.read().await;
                            if let Err(error) = state
                                .worker_command(Some(occurrence), WorkerCommand::Run { turn })
                                .await
                            {
                                tracing::warn!(%error,"Scheduled task could not start");
                                notice_control::emit(
                                    &state.notices,
                                    &state.foreground,
                                    occurrence,
                                    occurrence,
                                    &EngineEvent::Error {
                                        message: String::new(),
                                        retryable: true,
                                        kind: ErrorKind::Internal,
                                    },
                                    true,
                                );
                                if scheduler.pause(task.id).await.is_ok() {
                                    state.scheduled.lock().await.remove(&occurrence);
                                }
                                let _ = state
                                    .events
                                    .send(HostEvent::Status(
                                        "A scheduled task needs attention in Settings".into(),
                                    ))
                                    .await;
                            }
                        });
                    }
                }
                Err(error) => tracing::warn!(%error,"Read due schedules"),
            }
            drop(operation);
            tokio::time::sleep(Duration::from_secs(15)).await;
        }
    }
}

impl HostState {
    /// Tells the user once per occurrence that a scheduled run was cut off
    /// (its host stopped renewing the claim) and was not repeated.
    async fn report_interrupted_schedules(
        &self,
        scheduler: &crate::scheduler::Scheduler,
        reported: &mut std::collections::HashSet<Uuid>,
    ) {
        let Ok(tasks) = scheduler.list().await else {
            return;
        };
        for task in tasks {
            if let Some(occurrence) = task.interrupted
                && reported.insert(occurrence)
            {
                let name: String = task.prompt.chars().take(60).collect();
                let _ = self
                    .events
                    .send(HostEvent::Status(format!(
                        "A scheduled run of \"{name}\" was interrupted and was not repeated"
                    )))
                    .await;
            }
        }
    }
}

async fn acknowledge_schedule(
    scheduler: &crate::scheduler::Scheduler,
    scheduled: &Mutex<HashMap<Uuid, Uuid>>,
    session: Uuid,
) -> Result<()> {
    let mut scheduled = scheduled.lock().await;
    if let Some(id) = scheduled.get(&session).copied() {
        scheduler.complete(id).await?;
        scheduled.remove(&session);
    }
    Ok(())
}

fn activity_file(path: &std::path::Path) -> Result<std::fs::File> {
    Ok(std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?)
}

fn model_activity_busy(path: &std::path::Path) -> Result<bool> {
    let file = activity_file(path)?;
    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => {
            FileExt::unlock(&file)?;
            Ok(false)
        }
        Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
            Ok(true)
        }
        Err(error) => Err(error.into()),
    }
}

async fn acquire_model_activity(
    path: PathBuf,
    cancel: CancellationToken,
) -> Result<ModelActivityLease> {
    let file = activity_file(&path)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        ensure!(!cancel.is_cancelled(), "Task cancelled");
        match FileExt::try_lock_shared(&file) {
            Ok(()) => return Ok(ModelActivityLease(file)),
            Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {}
            Err(error) => return Err(error.into()),
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "Local model setup is busy; try again shortly"
        );
        tokio::select! {_ = cancel.cancelled()=>anyhow::bail!("Task cancelled"),_ = tokio::time::sleep(Duration::from_millis(20))=>{}}
    }
}

/// Resolve only an application-owned sibling executable. Test executables live
/// one directory below the normal Cargo binaries; installed bundles use siblings.
fn native_executable(name: &str) -> Result<PathBuf> {
    let executable = std::env::current_exe()?;
    let directory = executable
        .parent()
        .context("Application executable has no directory")?;
    let filename = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    };
    let sibling = directory.join(&filename);
    if sibling.is_file() {
        return Ok(sibling);
    }
    if directory.file_name().is_some_and(|name| name == "deps")
        && let Some(parent) = directory.parent()
    {
        let candidate = parent.join(filename);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    anyhow::bail!(
        "A required application component is missing. Reinstall the complete application."
    )
}

#[cfg(test)]
mod tests;
