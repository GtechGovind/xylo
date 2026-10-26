//! Event-driven application host. UI methods enqueue bounded commands and return
//! immediately; database, credentials, process I/O and networking stay off the UI.

use crate::{
    AppPaths,
    resources::{ResourceLease, ResourceManager, TaskDemand},
};
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
use workbench_core::*;
use workbench_helper_client::HelperClient;
use workbench_services::{ConnectorBroker, CredentialVault, SqliteMemory, Store};
#[path = "host/action_control.rs"]
mod action_control;
#[path = "host/document_control.rs"]
mod document_control;
#[path = "host/fact_control.rs"]
mod fact_control;
#[path = "host/image_control.rs"]
mod image_control;
#[path = "host/notice_control.rs"]
mod notice_control;
#[path = "host/steering_control.rs"]
mod steering_control;
#[path = "host/update_control.rs"]
mod update_control;

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
    HistoryIndexReady,
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
    Conversations(Vec<Conversation>),
    ConversationPage(ConversationPage),
    History {
        session_id: Uuid,
        messages: Vec<ChatMessage>,
    },
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
        provider: ProviderConfig,
        models: Vec<String>,
        context_limits: std::collections::BTreeMap<String, u32>,
    },
    /// May contain a returning account hint. Open in the system browser; never log.
    AuthorizationUrl(String),
    Error(String),
    Status(String),
    File {
        path: PathBuf,
        content: String,
    },
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
    Diff(String),
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
    Connectors(Vec<ConnectorConfig>),
    ConnectorTools {
        connector_id: Uuid,
        tools: Vec<ToolDefinition>,
    },
    Schedules(Vec<crate::scheduler::ScheduledTask>),
    TeamTasks(Vec<TeamTask>),
    Capability {
        request_id: Uuid,
        name: String,
        result: std::result::Result<Value, String>,
    },
    Stopped,
}

enum HostCommand {
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
    Load(Uuid),
    HistoryPage(Uuid, Option<i64>, usize),
    Lsp(crate::lsp::Command),
    Conversations,
    ConversationPage(Option<ConversationCursor>, usize),
    SaveSettings(AppSettings),
    Credential(String, String),
    Models(ProviderConfig),
    SaveProvider(AppSettings, Option<String>),
    Workspace(PathBuf),
    Read(PathBuf),
    OpenFile(Uuid, PathBuf),
    Save(PathBuf, String),
    Diff(PathBuf),
    MemorySearch(Uuid, String, String, Option<crate::MemoryCursor>),
    Remember(Uuid, MemoryRecord),
    Forget(Uuid, String, Uuid),
    ExportMemory(Uuid, String, PathBuf),
    Connectors,
    SaveConnector(ConnectorConfig),
    ConnectorTools(Uuid),
    Schedules,
    SaveSchedule(crate::scheduler::ScheduledTask),
    CreateSchedule(String, String, PathBuf),
    RemoveSchedule(Uuid),
    TeamTasks,
    Capability(Uuid, String, Value),
    CancelCapability(Uuid),
    Shutdown,
}

#[derive(Clone)]
pub struct HostClient {
    commands: Sender<HostCommand>,
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
        let (commands, incoming) = async_channel::bounded(128);
        let (events, receiver) = async_channel::bounded(EVENT_QUEUE_CAPACITY);
        std::thread::Builder::new().name("workbench-host".into()).spawn(move || {
            let _profile_lock = lock;
            let result = (|| -> Result<()> {
                let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
                let store = Store::open(&paths.database)?;
                let recovered = store.recover_interrupted()?;
                store.recover_steering()?;
                let settings = store.settings()?;
                let vault = CredentialVault::new(workbench_core::CREDENTIAL_SERVICE)?;
                let accounts=workbench_services::providers::ChatGptSignInService::new(&paths.data)?;
                let notifications=crate::notifications::Service::new(paths.data.clone(),crate::notifications::AppIdentity{id:APPLICATION_ID.into(),name:APPLICATION_NAME.into()})?;
                let (notices,notice_incoming)=async_channel::bounded(128);
                let updates=crate::updates::Service::new(paths.data.clone(),std::env::current_exe()?)?;
                let (memory,memory_setup_needed) = match SqliteMemory::configured(store.clone(), &paths.data.join("embedding.json")) {
                    Ok(memory)=>(memory,false),
                    Err(error)=>{
                        tracing::warn!(%error,"Semantic memory assets need repair");
                        (SqliteMemory::new(store.clone()),true)
                    }
                };
                let (editor_events,editor_incoming)=async_channel::bounded(128);
                let (workspace_events,workspace_incoming)=async_channel::bounded(16);
                let workspace=crate::workspace::WorkspaceService::with_cache(workspace_events,paths.data.join("workspace-index"));
                let context=runtime.enter();
                let lsp=crate::lsp::Service::new(paths.data.clone(),editor_events);
                drop(context);
                let state = Arc::new(HostState { paths, memory,accounts,updates,notifications,notices,foreground:Arc::new(AtomicBool::new(false)),operations:tokio::sync::RwLock::new(()),installing:Arc::new(AtomicBool::new(false)),install_request:Arc::new(parking_lot::Mutex::new(None)),workspace,search_requests:Mutex::new(HashMap::new()),image_import_gate:Mutex::new(()),
                    connectors:ConnectorBroker::new(store.clone(),vault.clone()),vault,store,
                    settings:Mutex::new(settings.clone()),workers:Mutex::new(HashMap::new()),events:events.clone(),
                    resources:ResourceManager::new(),queued:Arc::new(parking_lot::Mutex::new(HashMap::new())),
                    scheduled:Arc::new(Mutex::new(HashMap::new())),stopping:Arc::new(AtomicBool::new(false)),
                    helpers:Mutex::new(HashMap::new()),capability_requests:Mutex::new(HashMap::new()),lsp });
                runtime.block_on(async move {
                    let maintenance_stop=CancellationToken::new();
                    let _maintenance_guard=maintenance_stop.clone().drop_guard();
                    let notification_tasks=state.start_notifications(notice_incoming,maintenance_stop.clone());
                    let learning=workbench_services::LearningService::new(state.store.clone());
                    let learning_stop=maintenance_stop.clone();
                    let learning_task=tokio::spawn(async move {
                        if let Err(error)=learning.run(learning_stop).await {tracing::warn!(%error,"Background procedure evaluation stopped");}
                    });
                    let history_store=state.store.clone();
                    let history_stop=maintenance_stop.clone();
                    let history_events=state.events.clone();
                    let history_indexer=tokio::spawn(async move{
                        loop {
                            if history_stop.is_cancelled(){break;}
                            let store=history_store.clone();
                            match tokio::task::spawn_blocking(move||store.index_history_chunk(500)).await {
                                Ok(Ok(true))=>{let _=history_events.send(HostEvent::HistoryIndexReady).await;break;},
                                Ok(Ok(false))=>{},
                                result=>{tracing::warn!(?result,"Conversation indexing stopped");break;}
                            }
                            tokio::select!{_=history_stop.cancelled()=>break,_=tokio::time::sleep(Duration::from_millis(20))=>{}}
                        }
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
                    if memory_setup_needed {state.events.send(HostEvent::Status("Semantic search needs setup in Downloads. Your saved memories remain searchable.".into())).await?;}
                    if recovered > 0 {state.events.send(HostEvent::Status(format!("Recovered {recovered} interrupted actions; uncertain writes require review"))).await?;}
                    state.send_conversations().await?;
                    let scheduling_state=state.clone();
                    let scheduling=tokio::spawn(async move{scheduling_state.schedule_loop().await;});
                    while let Ok(command) = incoming.recv().await {
                        if matches!(command,HostCommand::Shutdown) {maintenance_stop.cancel();state.shutdown().await;scheduling.abort();editor_forwarder.abort();project_forwarder.abort();let _=learning_task.await;let _=history_indexer.await;for task in notification_tasks {let _=task.await;}state.events.send(HostEvent::Stopped).await?;break;}
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
                                if let Err(error)=state.handle(HostCommand::Workspace(path)).await{state.events.send(HostEvent::Error(error.to_string())).await?;}
                                continue;
                            },
                            HostCommand::Lsp(mut command)=>{
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
                        if !state.prepare_command(&command).await? {continue;}
                        let state = state.clone();
                        tokio::spawn(async move {
                            if let Err(error) = state.handle(command).await {
                                let _ = state.events.send(HostEvent::Error(format!("{error:#}"))).await;
                            }
                        });
                    }
                    Ok::<_,anyhow::Error>(())
                })
            })();
            if let Err(error) = result {let _ = events.send_blocking(HostEvent::Error(format!("Host stopped: {error:#}")));}
        })?;
        Ok((Self { commands }, receiver))
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
    pub fn load_session(&self, id: Uuid) -> Result<()> {
        self.enqueue(HostCommand::Load(id))
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
    pub fn set_credential(&self, reference: String, secret: String) -> Result<()> {
        self.enqueue(HostCommand::Credential(reference, secret))
    }
    /// Persist credentials before settings and discovery, within one host operation.
    pub fn save_provider(&self, settings: AppSettings, secret: Option<String>) -> Result<()> {
        self.enqueue(HostCommand::SaveProvider(settings, secret))
    }
    pub fn discover_models(&self, config: ProviderConfig) -> Result<()> {
        self.enqueue(HostCommand::Models(config))
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
    pub fn read_file(&self, path: PathBuf) -> Result<()> {
        self.enqueue(HostCommand::Read(path))
    }
    pub fn open_file(&self, request_id: Uuid, path: PathBuf) -> Result<()> {
        self.enqueue(HostCommand::OpenFile(request_id, path))
    }
    pub fn save_file(&self, path: PathBuf, content: String) -> Result<()> {
        self.enqueue(HostCommand::Save(path, content))
    }
    pub fn git_diff(&self, path: PathBuf) -> Result<()> {
        self.enqueue(HostCommand::Diff(path))
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
    pub fn memory_forget(&self, request_id: Uuid, scope: String, id: Uuid) -> Result<()> {
        self.enqueue(HostCommand::Forget(request_id, scope, id))
    }
    pub fn memory_export(
        &self,
        request_id: Uuid,
        scope: String,
        destination: PathBuf,
    ) -> Result<()> {
        self.enqueue(HostCommand::ExportMemory(request_id, scope, destination))
    }
    pub fn connectors(&self) -> Result<()> {
        self.enqueue(HostCommand::Connectors)
    }
    pub fn save_connector(&self, config: ConnectorConfig) -> Result<()> {
        self.enqueue(HostCommand::SaveConnector(config))
    }
    pub fn connector_tools(&self, id: Uuid) -> Result<()> {
        self.enqueue(HostCommand::ConnectorTools(id))
    }
    pub fn list_schedules(&self) -> Result<()> {
        self.enqueue(HostCommand::Schedules)
    }
    pub fn save_schedule(&self, task: crate::scheduler::ScheduledTask) -> Result<()> {
        self.enqueue(HostCommand::SaveSchedule(task))
    }
    pub fn create_schedule(&self, prompt: String, cron: String, workspace: PathBuf) -> Result<()> {
        self.enqueue(HostCommand::CreateSchedule(prompt, cron, workspace))
    }
    pub fn remove_schedule(&self, id: Uuid) -> Result<()> {
        self.enqueue(HostCommand::RemoveSchedule(id))
    }
    pub fn team_tasks(&self) -> Result<()> {
        self.enqueue(HostCommand::TeamTasks)
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

struct WorkerHandle {
    commands: mpsc::Sender<Envelope<WorkerCommand>>,
    active: Arc<AtomicBool>,
    lease: Arc<Mutex<Option<ResourceLease>>>,
    stop: CancellationToken,
    exited: tokio::sync::watch::Receiver<bool>,
    run_request: Arc<parking_lot::Mutex<Option<Uuid>>>,
    run_turn: Arc<parking_lot::Mutex<Option<Uuid>>>,
    model_activity: Arc<Mutex<Option<ModelActivityLease>>>,
}

/// Keep a retiring process supervised until its reader has killed/waited it and
/// reconciled the journal. The stop token also prevents a new turn being queued
/// behind Shutdown on the old channel. Idle workers have no active turn to lose.
fn retire_idle_workers(workers: &mut HashMap<Uuid, WorkerHandle>, current: Uuid) {
    workers.retain(|session, worker| *session == current || !*worker.exited.borrow());
    for (session, worker) in workers.iter() {
        if *session != current && !worker.active.load(Ordering::Acquire) {
            let _ = worker.commands.try_send(Envelope::new(
                Uuid::new_v4(),
                Some(*session),
                0,
                WorkerCommand::Shutdown,
            ));
            worker.stop.cancel();
        }
    }
}

struct ModelActivityLease(std::fs::File);
impl Drop for ModelActivityLease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

type PendingRuns = Arc<parking_lot::Mutex<HashMap<Uuid, (Uuid, CancellationToken)>>>;

/// Retain a session's starting state until it is either running or fully failed.
/// Cancelling a queued turn leaves the guard in place until admission unwinds.
struct PendingRun {
    session: Uuid,
    pending: PendingRuns,
    id: Uuid,
    cancel: CancellationToken,
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
struct HostState {
    workspace: crate::workspace::WorkspaceService,
    search_requests: Mutex<HashMap<Uuid, CancellationToken>>,
    image_import_gate: Mutex<()>,
    paths: AppPaths,
    store: Store,
    memory: SqliteMemory,
    accounts: workbench_services::providers::ChatGptSignInService,
    vault: CredentialVault,
    connectors: ConnectorBroker,
    settings: Mutex<AppSettings>,
    workers: Mutex<HashMap<Uuid, WorkerHandle>>,
    events: Sender<HostEvent>,
    resources: ResourceManager,
    queued: PendingRuns,
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
                HostCommand::MemorySearch(id, _, _, _)
                | HostCommand::Remember(id, _)
                | HostCommand::Forget(id, _, _)
                | HostCommand::ExportMemory(id, _, _) => {
                    let message = "Wait for the update installation to finish".to_owned();
                    let event = match command {
                        HostCommand::MemorySearch(_, _, _, _) => HostEvent::MemoryPage {
                            request_id: *id,
                            result: Err(message),
                        },
                        HostCommand::ExportMemory(_, _, _) => HostEvent::MemoryExport {
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
                    .worker_command_with_draft(
                        Some(turn.session_id),
                        WorkerCommand::Run { turn },
                        Some(submission),
                    )
                    .await
                    .and_then(|receipt| receipt.context("The draft was not admitted"))
                    .map_err(|error| error.to_string());
                self.events
                    .send(HostEvent::DraftAdmitted { request_id, result })
                    .await?;
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
                    workbench_services::attachments::AttachmentStore::new(artifacts)?
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
            HostCommand::Worker(session, command) => self.worker_command(session, command).await?,
            HostCommand::Load(id) => {
                let store = self.store.clone();
                let rows = tokio::task::spawn_blocking(move || store.messages(id, 1000)).await??;
                self.events
                    .send(HostEvent::History {
                        session_id: id,
                        messages: rows,
                    })
                    .await?;
            }
            HostCommand::HistoryPage(id, before, limit) => {
                let store = self.store.clone();
                let page =
                    tokio::task::spawn_blocking(move || store.messages_page(id, before, limit))
                        .await??;
                self.events.send(HostEvent::HistoryPage(page)).await?;
            }
            HostCommand::Conversations => self.send_conversations().await?,
            HostCommand::ConversationPage(before, limit) => {
                let store = self.store.clone();
                let page =
                    tokio::task::spawn_blocking(move || store.conversations_page(before, limit))
                        .await??;
                self.events.send(HostEvent::ConversationPage(page)).await?;
            }
            HostCommand::SaveSettings(settings) => {
                let value = settings.clone();
                let store = self.store.clone();
                tokio::task::spawn_blocking(move || store.save_settings(&value)).await??;
                *self.settings.lock().await = settings.clone();
                self.events.send(HostEvent::Settings(settings)).await?;
            }
            HostCommand::Credential(reference, secret) => {
                let vault = self.vault.clone();
                tokio::task::spawn_blocking(move || vault.set(&reference, &secret)).await??;
                self.events
                    .send(HostEvent::Status(
                        "Credential saved in the OS credential store".into(),
                    ))
                    .await?;
            }
            HostCommand::SaveProvider(settings, secret) => {
                let config = settings.provider.clone();
                config.validate_connection()?;
                workbench_services::providers::normalize_provider(&config)?;
                if let Some(secret) = secret {
                    let reference = config
                        .credential_ref
                        .clone()
                        .context("Missing credential reference")?;
                    let vault = self.vault.clone();
                    tokio::task::spawn_blocking(move || vault.set(&reference, &secret)).await??;
                }
                let value = settings.clone();
                let store = self.store.clone();
                tokio::task::spawn_blocking(move || store.save_settings(&value)).await??;
                *self.settings.lock().await = settings.clone();
                self.events.send(HostEvent::Settings(settings)).await?;
                if !matches!(
                    config.family,
                    workbench_core::ApiFamily::Azure | workbench_core::ApiFamily::Vertex
                ) {
                    self.discover_models(config).await?;
                }
            }
            HostCommand::Models(config) => self.discover_models(config).await?,
            HostCommand::Workspace(path) => {
                let requested = path;
                let path = tokio::fs::canonicalize(&requested).await?;
                ensure!(path.is_dir(), "Choose a directory");
                let mut settings = self.settings.lock().await;
                settings.last_workspace = Some(path.clone());
                let store = self.store.clone();
                let value = settings.clone();
                drop(settings);
                tokio::task::spawn_blocking(move || store.save_settings(&value)).await??;
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
            HostCommand::Read(path) => {
                let path = self.authorized_file(path, false).await?;
                ensure!(
                    tokio::fs::metadata(&path).await?.len() <= 8 * 1024 * 1024,
                    "File exceeds editor preview limit"
                );
                let content = tokio::fs::read_to_string(&path).await?;
                self.events.send(HostEvent::File { path, content }).await?;
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
            HostCommand::Diff(path) => {
                let result = crate::process::run(
                    "git",
                    &[
                        "diff".into(),
                        "--no-ext-diff".into(),
                        "--no-textconv".into(),
                    ],
                    &path,
                    Duration::from_secs(30),
                    Default::default(),
                )
                .await?;
                let result = crate::process::require_success(result)?;
                self.events.send(HostEvent::Diff(result.stdout)).await?;
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
            }
            HostCommand::Forget(request_id, scope, id) => {
                let memory = self.memory.clone();
                let result = tokio::task::spawn_blocking(move || {
                    memory.forget_scoped(&scope, id)?;
                    Ok::<_, anyhow::Error>(crate::MemoryMutation {
                        scope,
                        id,
                        deleted: true,
                    })
                })
                .await?
                .map_err(|error| error.to_string());
                self.events
                    .send(HostEvent::MemoryMutation { request_id, result })
                    .await?;
            }
            HostCommand::ExportMemory(request_id, scope, destination) => {
                let database = self.paths.database.clone();
                let result = tokio::task::spawn_blocking(move || {
                    workbench_services::export_memories(
                        &database,
                        &scope,
                        &destination,
                        &CancellationToken::new(),
                    )
                })
                .await?
                .map_err(|error| error.to_string());
                self.events
                    .send(HostEvent::MemoryExport { request_id, result })
                    .await?;
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
            HostCommand::ConnectorTools(id) => {
                let store = self.store.clone();
                let configs = tokio::task::spawn_blocking(move || store.connectors()).await??;
                let config = configs
                    .into_iter()
                    .find(|c| c.id == id)
                    .context("Connector no longer exists")?;
                let tools = self.connectors.tools(&config, &Default::default()).await?;
                self.events
                    .send(HostEvent::ConnectorTools {
                        connector_id: id,
                        tools,
                    })
                    .await?;
            }
            HostCommand::Schedules => self.send_schedules().await?,
            HostCommand::SaveSchedule(task) => {
                self.scheduler(task.workspace.clone())?.save(task).await?;
                self.send_schedules().await?;
            }
            HostCommand::CreateSchedule(prompt, cron, workspace) => {
                self.scheduler(workspace)?.create(prompt, cron).await?;
                self.send_schedules().await?;
            }
            HostCommand::RemoveSchedule(id) => {
                self.default_scheduler()?.remove(id).await?;
                self.send_schedules().await?;
            }
            HostCommand::TeamTasks => {
                let store = self.store.clone();
                let actions = tokio::task::spawn_blocking(move || {
                    store.actions_by_name("agent_delegate", 200)
                })
                .await??;
                let tasks = actions
                    .into_iter()
                    .map(|action| TeamTask {
                        id: action.intent.id,
                        title: action
                            .intent
                            .arguments
                            .get("task")
                            .and_then(Value::as_str)
                            .unwrap_or("Delegated task")
                            .chars()
                            .take(120)
                            .collect(),
                        status: match action.state.as_str() {
                            "succeeded" => "Completed",
                            "running" => "Working",
                            "pending" => "Waiting",
                            "denied" => "Cancelled",
                            _ => "Needs attention",
                        }
                        .into(),
                        summary: action
                            .result
                            .as_ref()
                            .and_then(|result| result.get("summary"))
                            .map(|summary| {
                                summary
                                    .as_str()
                                    .map(str::to_owned)
                                    .unwrap_or_else(|| summary.to_string())
                            })
                            .unwrap_or_default(),
                    })
                    .collect();
                self.events.send(HostEvent::TeamTasks(tasks)).await?;
            }
            HostCommand::Capability(request_id, name, args) => {
                let cancel = self
                    .capability_requests
                    .lock()
                    .await
                    .get(&request_id)
                    .cloned()
                    .context("Action is no longer pending")?;
                let result = self.run_capability(&name, &args, cancel).await;
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
        name: &str,
        args: &Value,
        cancel: CancellationToken,
    ) -> Result<Value> {
        ensure!(
            !self.stopping.load(Ordering::Acquire),
            "Application is closing"
        );
        ensure!(!cancel.is_cancelled(), "Action cancelled");
        match name {
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
                            self.vault.clone(),
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
                        self.accounts.account(self.vault.clone(), reference).await?,
                    )?);
                }
                "account_chatgpt_sign_out" => {
                    let warning = self
                        .accounts
                        .sign_out(self.vault.clone(), reference, &cancel)
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
        let _native_lease = if let Some(demand) = crate::resources::capability_demand(name) {
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
                    native_executable("workbench-helper")?,
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
        let temp = path.with_file_name(format!(".workbench-{}.tmp", Uuid::new_v4()));
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
            let images = workbench_services::attachments::AttachmentStore::new(artifacts)?;
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
        self.worker_command_with_draft(target, command, None)
            .await
            .map(|_| ())
    }

    async fn worker_command_with_draft(
        self: &Arc<Self>,
        target: Option<Uuid>,
        mut command: WorkerCommand,
        submission: Option<crate::drafts::DraftSubmission>,
    ) -> Result<Option<crate::drafts::DraftAdmission>> {
        let session = match &command {
            WorkerCommand::Run { turn } => turn.session_id,
            _ => match target {
                Some(id) => id,
                None => {
                    let workers = self.workers.lock().await;
                    let active: Vec<_> = workers
                        .iter()
                        .filter(|(_, w)| w.active.load(Ordering::Acquire))
                        .map(|(id, _)| *id)
                        .collect();
                    ensure!(
                        active.len() == 1,
                        "Select a single active task for this control action"
                    );
                    active[0]
                }
            },
        };
        if matches!(command, WorkerCommand::Cancel) {
            let queued = self
                .queued
                .lock()
                .get(&session)
                .map(|(_, cancel)| cancel.clone());
            if let Some(cancel) = queued {
                cancel.cancel();
                self.events
                    .send(HostEvent::Engine(Envelope::new(
                        Uuid::new_v4(),
                        Some(session),
                        0,
                        EngineEvent::Cancelled,
                    )))
                    .await?;
                return Ok(None);
            }
        }
        let mut pending_run = None;
        let mut admitted = None;
        let mut model_activity = None;
        if let WorkerCommand::Run { turn } = &mut command {
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
            pending_run = Some(PendingRun {
                session,
                id: turn.turn_id,
                cancel: cancellation.clone(),
                pending: self.queued.clone(),
            });
            if cancellation.is_cancelled() {
                return Ok(None);
            }
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
            let store = self.store.clone();
            let turn_id = turn.turn_id;
            if let Some(completed) =
                tokio::task::spawn_blocking(move || store.completed_turn(session, turn_id))
                    .await??
            {
                self.complete_schedule(session).await?;
                self.events
                    .send(HostEvent::Engine(Envelope::new(
                        turn_id,
                        Some(session),
                        0,
                        EngineEvent::Completed {
                            text: completed.text,
                            usage: completed.usage,
                        },
                    )))
                    .await?;
                let receipt = if let Some(submission) = submission {
                    Some(
                        crate::drafts::admit_turn(self.store.clone(), turn.clone(), submission)
                            .await?,
                    )
                } else {
                    None
                };
                return Ok(receipt);
            }
            turn.provider.validate()?;
            self.accounts
                .prepare_provider_credential(&turn.provider, self.vault.clone(), &cancellation)
                .await?;
            {
                let mut workers = self.workers.lock().await;
                ensure!(
                    !workers
                        .get(&session)
                        .is_some_and(|w| w.active.load(Ordering::Acquire)),
                    "This task is already working"
                );
                retire_idle_workers(&mut workers, session);
            }
            admitted = Some(
                self.resources
                    .acquire(
                        TaskDemand::interactive(turn.provider.context_tokens),
                        cancellation.clone(),
                    )
                    .await?,
            );
            if cancellation.is_cancelled() {
                return Ok(None);
            }
            if turn.provider.managed_local {
                let helper = self
                    .helper_for_workspace(turn.workspace.clone(), &cancellation)
                    .await?;
                model_activity = Some(
                    acquire_model_activity(
                        self.paths.data.join("local-inference-activity.lock"),
                        cancellation.clone(),
                    )
                    .await?,
                );
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
            }
            if cancellation.is_cancelled() {
                return Ok(None);
            }
        }
        let mut workers = self.workers.lock().await;
        ensure!(
            !self.stopping.load(Ordering::Acquire),
            "Application is closing"
        );
        if pending_run
            .as_ref()
            .is_some_and(|pending| pending.cancel.is_cancelled())
        {
            return Ok(None);
        }
        let dead = workers.get(&session).is_some_and(|worker| {
            worker.stop.is_cancelled() || *worker.exited.borrow() || worker.commands.is_closed()
        });
        if dead && let Some(worker) = workers.get_mut(&session) {
            worker.stop.cancel();
            if !*worker.exited.borrow() {
                tokio::time::timeout(Duration::from_secs(8), worker.exited.changed())
                    .await
                    .context("The previous task is still stopping")??;
            }
            workers.remove(&session);
        }
        if let std::collections::hash_map::Entry::Vacant(entry) = workers.entry(session) {
            ensure!(
                matches!(command, WorkerCommand::Run { .. }),
                "This task is no longer running"
            );
            entry.insert(self.spawn_worker(session).await?);
        }
        let worker = workers.get(&session).context("Task could not start")?;
        if let Some(pending) = pending_run.as_ref() {
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
                !pending.cancel.is_cancelled() && !self.stopping.load(Ordering::Acquire),
                "Task cancelled before admission"
            );
            let WorkerCommand::Run { turn } = &command else {
                unreachable!()
            };
            let attachments = turn.attachments.clone();
            let artifacts = self.paths.artifacts.clone();
            tokio::task::spawn_blocking(move || {
                workbench_services::attachments::AttachmentStore::new(artifacts)?
                    .validate(session, &attachments)
            })
            .await??;
            ensure!(
                !pending.cancel.is_cancelled(),
                "Task cancelled before admission"
            );
            let receipt = if let Some(submission) = submission {
                Some(crate::drafts::admit_turn(self.store.clone(), turn.clone(), submission).await?)
            } else {
                let store = self.store.clone();
                let turn = turn.clone();
                tokio::task::spawn_blocking(move || store.register_user_turn(&turn)).await??;
                None
            };
            // The durable user turn and conditional draft clear have committed.
            // Always dispatch the reserved Run, even if cancellation arrived
            // during SQLite commit; a following Cancel stops that exact run.
            let request_id = Uuid::new_v4();
            *worker.run_request.lock() = Some(request_id);
            if let WorkerCommand::Run { turn } = &command {
                *worker.run_turn.lock() = Some(turn.turn_id);
            }
            worker.active.store(true, Ordering::Release);
            *worker.lease.lock().await = admitted;
            *worker.model_activity.lock().await = model_activity;
            permit.send(Envelope::new(request_id, Some(session), 0, command));
            let cancelled = {
                let mut starting = self.queued.lock();
                starting.remove(&session);
                pending.cancel.is_cancelled() || self.stopping.load(Ordering::Acquire)
            };
            let control = worker.commands.clone();
            let stop = worker.stop.clone();
            drop(workers);
            if cancelled {
                // Never drop an admitted cancellation because Run filled the
                // channel. If a stalled writer cannot accept it promptly,
                // supervise a process stop and preserve journal recovery.
                if !matches!(
                    tokio::time::timeout(
                        Duration::from_millis(100),
                        control.send(Envelope::new(
                            Uuid::new_v4(),
                            Some(session),
                            0,
                            WorkerCommand::Cancel
                        ))
                    )
                    .await,
                    Ok(Ok(()))
                ) {
                    stop.cancel();
                }
            }
            if let Err(error) = self.send_conversations().await {
                tracing::debug!(%error, "Refresh admitted conversation list");
            }
            return Ok(receipt);
        }
        worker
            .commands
            .send(Envelope::new(Uuid::new_v4(), Some(session), 0, command))
            .await?;
        Ok(None)
    }

    async fn discover_models(&self, config: ProviderConfig) -> Result<()> {
        let cancel = CancellationToken::new();
        self.accounts
            .prepare_provider_credential(&config, self.vault.clone(), &cancel)
            .await?;
        let catalog =
            workbench_services::providers::discover_provider_models(&config, &self.vault, &cancel)
                .await?;
        let context_limits = catalog
            .iter()
            .filter_map(|m| m.context_tokens.map(|n| (m.id.clone(), n)))
            .collect();
        let models = catalog.into_iter().map(|m| m.id).collect();
        self.events
            .send(HostEvent::Models {
                provider: config,
                models,
                context_limits,
            })
            .await?;
        Ok(())
    }

    async fn spawn_worker(&self, session: Uuid) -> Result<WorkerHandle> {
        let executable = native_executable("workbench-worker")?;
        let mut command = tokio::process::Command::new(executable);
        #[cfg(windows)]
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: worker uses private pipes.
        let mut child = command
            .arg("--data-dir")
            .arg(&self.paths.data)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let input = child.stdin.take().context("Missing worker stdin")?;
        let output = child.stdout.take().context("Missing worker stdout")?;
        let errors = child.stderr.take().context("Missing worker stderr")?;
        let (tx, mut rx) = mpsc::channel::<Envelope<WorkerCommand>>(64);
        let active = Arc::new(AtomicBool::new(false));
        let active_reader = active.clone();
        let run_request = Arc::new(parking_lot::Mutex::new(None::<Uuid>));
        let reader_request = run_request.clone();
        let run_turn = Arc::new(parking_lot::Mutex::new(None::<Uuid>));
        let reader_turn = run_turn.clone();
        let notice_sender = self.notices.clone();
        let foreground = self.foreground.clone();
        let model_activity = Arc::new(Mutex::new(None::<ModelActivityLease>));
        let reader_activity = model_activity.clone();
        let recovery_store = self.store.clone();
        let steering_store = self.store.clone();
        let events = self.events.clone();
        let scheduling = self.default_scheduler()?;
        let scheduling_state = self.scheduled.clone();
        let lease = Arc::new(Mutex::new(None::<ResourceLease>));
        let reader_lease = lease.clone();
        let stop = CancellationToken::new();
        let reader_stop = stop.clone();
        let writer_stop = stop.clone();
        let (exited_tx, exited) = tokio::sync::watch::channel(false);
        tokio::spawn(async move {
            let mut input = BufWriter::new(input);
            loop {
                let command =
                    tokio::select! {_ = writer_stop.cancelled()=>break,command=rx.recv()=>command};
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
        });
        tokio::spawn(async move {
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
                        tracing::debug!(worker=%session,message=%String::from_utf8_lossy(&line),"native worker")
                    }
                }
            }
        });
        tokio::spawn(async move {
            let mut output = BufReader::new(output);
            let mut line = Vec::new();
            loop {
                line.clear();
                let mut limited = (&mut output).take((MAX_PROTOCOL_BYTES + 1) as u64);
                let read = tokio::select! {
                    _ = reader_stop.cancelled() => break,
                    read = limited.read_until(b'\n', &mut line) => read,
                };
                match read {
                    Ok(0) | Err(_) => break,
                    _ => {}
                }
                if line.len() > MAX_PROTOCOL_BYTES {
                    let _ = events
                        .send(HostEvent::Error(
                            "Worker exceeded protocol payload limit".into(),
                        ))
                        .await;
                    break;
                }
                match serde_json::from_slice::<Envelope<EngineEvent>>(&line) {
                    Ok(mut envelope) if envelope.validate().is_ok() => {
                        envelope.session_id = Some(session);
                        if let EngineEvent::Steering { receipt } = &envelope.payload {
                            if let Err(error) = steering_control::emit_receipt(
                                steering_store.clone(),
                                &events,
                                session,
                                receipt.clone(),
                            )
                            .await
                            {
                                tracing::warn!(%error, "Read durable guidance receipt");
                            }
                            continue;
                        }
                        let terminal = matches!(
                            envelope.payload,
                            EngineEvent::Completed { .. }
                                | EngineEvent::Cancelled
                                | EngineEvent::Error { .. }
                        );
                        let matched = *reader_request.lock() == Some(envelope.request_id);
                        if terminal && !matched {
                            if let EngineEvent::Error { message, .. } = envelope.payload {
                                let _ = events.send(HostEvent::Error(message)).await;
                            }
                            continue;
                        }
                        let scheduled_task =
                            terminal && scheduling_state.lock().await.contains_key(&session);
                        if matches!(envelope.payload, EngineEvent::Completed { .. })
                            && let Err(error) =
                                acknowledge_schedule(&scheduling, &scheduling_state, session).await
                        {
                            tracing::error!(%error,"Acknowledge completed schedule");
                        }
                        let current_turn = *reader_turn.lock();
                        if matched && let Some(turn_id) = current_turn {
                            notice_control::emit(
                                &notice_sender,
                                &foreground,
                                session,
                                turn_id,
                                &envelope.payload,
                                scheduled_task,
                            );
                        }
                        if terminal {
                            if let Err(error) = steering_control::finish_turn(
                                steering_store.clone(),
                                &events,
                                session,
                                current_turn,
                            )
                            .await
                            {
                                tracing::warn!(%error, "Settle ended task guidance");
                            }
                            reader_turn.lock().take();
                            reader_request.lock().take();
                            active_reader.store(false, Ordering::Release);
                            reader_lease.lock().await.take();
                            reader_activity.lock().await.take();
                        }
                        if events.send(HostEvent::Engine(envelope)).await.is_err() {
                            break;
                        }
                    }
                    _ => {
                        let _ = events
                            .send(HostEvent::Error("Invalid native worker message".into()))
                            .await;
                        break;
                    }
                }
            }
            let _ = child.kill().await;
            let _ = child.wait().await;
            let ended_turn = *reader_turn.lock();
            if let Err(error) =
                steering_control::finish_turn(steering_store, &events, session, ended_turn).await
            {
                tracing::warn!(%error, "Recover reaped task guidance");
            }
            let recovered = tokio::task::spawn_blocking(move || {
                recovery_store.recover_session_interrupted(session)
            })
            .await;
            if !matches!(recovered, Ok(Ok(_))) {
                tracing::error!(?recovered,%session,"Recover reaped task journal");
            }
            reader_stop.cancel();
            let crashed_turn = reader_turn.lock().take();
            reader_request.lock().take();
            if active_reader.swap(false, Ordering::AcqRel) {
                if let Some(turn) = crashed_turn {
                    notice_control::emit(
                        &notice_sender,
                        &foreground,
                        session,
                        turn,
                        &EngineEvent::Error {
                            message: String::new(),
                            retryable: true,
                        },
                        scheduling_state.lock().await.contains_key(&session),
                    );
                }
                let _ = events
                    .send(HostEvent::Engine(Envelope::new(
                        Uuid::new_v4(),
                        Some(session),
                        0,
                        EngineEvent::Error {
                            message: "This task stopped unexpectedly. Resume the chat to continue."
                                .into(),
                            retryable: true,
                        },
                    )))
                    .await;
            }
            reader_lease.lock().await.take();
            reader_activity.lock().await.take();
            let _ = exited_tx.send(true);
        });
        Ok(WorkerHandle {
            commands: tx,
            active,
            lease,
            stop,
            exited,
            run_request,
            run_turn,
            model_activity,
        })
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
            match scheduler.claim_due().await {
                Ok(tasks) => {
                    for task in tasks {
                        let occurrence =
                            Uuid::new_v5(&task.id, task.next_run.to_rfc3339().as_bytes());
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
                        let provider = self.settings.lock().await.provider.clone();
                        let turn = TurnRequest {
                            session_id: occurrence,
                            turn_id: occurrence,
                            prompt: task.prompt,
                            workspace: task.workspace,
                            provider,
                            permissions: PermissionMode::AskForWrites,
                            attachments: Vec::new(),
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

#[cfg(test)]
#[path = "host/draft_tests.rs"]
mod draft_tests;
