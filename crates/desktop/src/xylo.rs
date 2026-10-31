#[path = "xylo/environments.rs"]
mod environments;
#[path = "xylo/extensions.rs"]
mod extensions;
#[path = "xylo/management.rs"]
mod management;
#[path = "xylo/notifications.rs"]
mod notifications;
#[path = "xylo/plugins.rs"]
mod plugins;
#[path = "xylo/recovery.rs"]
mod recovery;
use notifications::NotificationUi;
#[path = "xylo/drafts.rs"]
mod drafts;
use drafts::{DraftFlushAction, DraftUi};
#[path = "xylo/turn_queue.rs"]
mod turn_queue;
use turn_queue::TurnQueueUi;
#[path = "xylo/attachments.rs"]
mod attachments;
use attachments::AttachmentUi;
#[path = "xylo/image_generation.rs"]
mod image_generation;
use image_generation::ImageGenerationUi;
#[path = "xylo/accounts.rs"]
mod accounts;
use accounts::{AccountUi, CHATGPT_REF};
#[path = "xylo/speech.rs"]
mod speech;
use speech::SpeechUi;
#[path = "xylo/file_browser.rs"]
mod file_browser;
use file_browser::FileBrowserUi;
#[path = "xylo/project_search.rs"]
mod project_search;
use project_search::ProjectSearchUi;
#[path = "xylo/learning.rs"]
mod learning;
use learning::LearningUi;
#[path = "xylo/updates.rs"]
mod updates;
use updates::UpdateUi;
#[path = "xylo/history_search.rs"]
mod history_search;
use history_search::HistorySearchUi;
#[path = "xylo/shortcuts.rs"]
mod shortcuts;
use shortcuts::*;
#[path = "xylo/assets.rs"]
mod assets;
use self::assets::AssetsUi;
#[path = "xylo/appearance.rs"]
mod appearance;
use appearance::{Panel, ResizePanel};
#[path = "xylo/editor_assist.rs"]
mod editor_assist;
use editor_assist::EditorAssist;
#[path = "xylo/app_status.rs"]
mod app_status;
#[path = "xylo/chat.rs"]
mod chat;
use app_status::{HostHealth, StatusLine, product_error};
#[path = "xylo/context_panel.rs"]
mod context_panel;
#[path = "xylo/memory.rs"]
mod memory;
#[path = "xylo/memory_form.rs"]
mod memory_form;
#[path = "xylo/memory_versions.rs"]
mod memory_versions;
#[path = "xylo/model_picker.rs"]
mod model_picker;
#[path = "xylo/native_tools.rs"]
mod native_tools;
#[path = "xylo/permission_control.rs"]
mod permission_control;
#[path = "xylo/quit.rs"]
mod quit;
#[path = "xylo/settings.rs"]
mod settings;
#[path = "xylo/settings_ui.rs"]
mod settings_ui;
#[path = "xylo/work_panel.rs"]
mod work_panel;
use native_tools::NativeUi;
#[path = "xylo/documents.rs"]
mod documents;
#[path = "xylo/providers.rs"]
mod providers;
#[path = "xylo/sidebar.rs"]
mod sidebar;
#[path = "xylo/tasks.rs"]
mod tasks;
#[path = "xylo/workspace_pages.rs"]
mod workspace_pages;
use crate::{
    icons::{self, AppIcon},
    state::{TurnState, message},
    terminal_workspace::{TerminalWorkspace, TerminalWorkspaceEvent},
    ui::*,
};
use gpui_kit::base::text::TextView;
use gpui_kit::component::input::{
    Editor, EditorState, Input, InputEvent, InputState, Textarea, TextareaState,
};
use gpui_kit::{prelude::*, *};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
use uuid::Uuid;
use xylo_core::*;
use xylo_runtime::{
    host::{HostClient, HostEvent},
    terminal::TerminalSession,
};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    Chat,
    Permissions,
    Skills,
    Files,
    Search,
    Drafts,
    Voice,
    Images,
    Notifications,
    Changes,
    Artifacts,
    Memory,
    Connectors,
    Plugins,
    Environments,
    Providers,
    Schedules,
    Appearance,
    Assets,
}
impl Page {
    /// Settings pages replace the sidebar with settings navigation.
    fn is_settings(self) -> bool {
        matches!(
            self,
            Self::Providers
                | Self::Permissions
                | Self::Skills
                | Self::Memory
                | Self::Connectors
                | Self::Appearance
                | Self::Assets
                | Self::Voice
                | Self::Images
                | Self::Notifications
                | Self::Plugins
                | Self::Environments
        )
    }
    fn key(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Files => "files",
            Self::Search => "search",
            Self::Drafts => "drafts",
            Self::Voice => "voice",
            Self::Images => "images",
            Self::Notifications => "notifications",
            Self::Changes => "changes",
            Self::Artifacts => "artifacts",
            Self::Memory => "memory",
            Self::Connectors => "connectors",
            Self::Plugins => "plugins",
            Self::Environments => "environments",
            Self::Providers => "providers",
            Self::Schedules => "schedules",
            Self::Assets => "downloads",
            Self::Appearance => "appearance",
            Self::Permissions => "permissions",
            Self::Skills => "skills",
        }
    }
    /// The page restored at launch. Transient pages (drafts, voice, images,
    /// updates, notifications, artifacts, schedules, history search) reopen as
    /// the chat.
    fn from_key(key: &str) -> Self {
        match key {
            "permissions" => Self::Permissions,
            "skills" => Self::Skills,
            "files" => Self::Files,
            "search" => Self::Search,
            "changes" => Self::Changes,
            "memory" => Self::Memory,
            "connectors" => Self::Connectors,
            "plugins" => Self::Plugins,
            "environments" => Self::Environments,
            "providers" => Self::Providers,
            "downloads" => Self::Assets,
            "appearance" => Self::Appearance,
            _ => Self::Chat,
        }
    }
}

/// How long typing in the Memory search must pause before it searches.
const MEMORY_SEARCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(200);

pub struct Xylo {
    host: Option<HostClient>,
    /// Bounded test-only observation of the UI→worker dispatch boundary.
    #[cfg(test)]
    validation_worker_commands: Option<std::sync::mpsc::SyncSender<(Uuid, WorkerCommand)>>,
    settings: AppSettings,
    page: Page,
    workspace: Option<PathBuf>,
    workspace_ready: bool,
    session: Uuid,
    conversations: Vec<Conversation>,
    conversations_before: Option<ConversationCursor>,
    conversations_has_more: bool,
    conversations_loading_more: bool,
    /// The host has answered the first conversation list; until then the
    /// sidebar shows placeholder rows.
    conversations_loaded: bool,
    sidebar_state: sidebar::SidebarUi,
    messages: Vec<ChatMessage>,
    turns: BTreeMap<Uuid, TurnState>,
    composer: Entity<TextareaState>,
    editor: Entity<EditorState>,
    editor_path: Option<PathBuf>,
    editor_assist: Option<EditorAssist>,
    editor_clock: std::sync::Arc<std::sync::atomic::AtomicI32>,
    editor_assist_task: Option<Task<()>>,
    editor_dirty: bool,
    pending_saves: BTreeMap<PathBuf, String>,
    editor_subscription: Option<Subscription>,
    file_browser: FileBrowserUi,
    artifacts: Vec<PathBuf>,
    git_panel: work_panel::GitPanel,
    terminal: Option<Entity<TerminalWorkspace>>,
    terminal_subscription: Option<Subscription>,
    terminal_cache: BTreeMap<PathBuf, (Entity<TerminalWorkspace>, Option<Subscription>)>,
    terminal_visible: bool,
    terminal_starting: bool,
    provider_name: Entity<InputState>,
    provider_headers: Entity<TextareaState>,
    provider_discovery_url: Entity<InputState>,
    provider_editor_source: Option<ProviderConfig>,
    provider_discovery: providers::ProviderDiscoveryUi,
    permissions: settings::PermissionsUi,
    provider_editor_kind: String,
    endpoint: Entity<InputState>,
    model: Entity<InputState>,
    secret: Entity<InputState>,
    context: Entity<InputState>,
    output: Entity<InputState>,
    models: Vec<String>,
    model_context_limits: std::collections::BTreeMap<String, u32>,
    memory_query: Entity<InputState>,
    /// The search that runs once typing in `memory_query` pauses; replacing
    /// it drops (cancels) the previous one.
    memory_search_debounce: Option<Task<()>>,
    memory_content: Entity<TextareaState>,
    memories: Vec<MemoryRecord>,
    facts: memory::MemoryUi,
    connector_name: Entity<InputState>,
    connector_endpoint: Entity<InputState>,
    connector_command: Entity<InputState>,
    connector_args: Entity<TextareaState>,
    connector_pending: Option<Uuid>,
    connector_pending_save: bool,
    connector_editing: Option<Uuid>,
    connector_credential: Entity<InputState>,
    connector_transport: String,
    /// The Connections page shows its add/edit form only after Add or Edit.
    connector_form_open: bool,
    connectors: Vec<ConnectorConfig>,
    connector_tools: Vec<ToolDefinition>,
    connector_inspected: Option<Uuid>,
    connector_inspection_request: Option<Uuid>,
    connector_loading: bool,
    connector_error: Option<String>,
    /// Latest host or task notice, shown briefly as a toast.
    status: StatusLine,
    status_expiry: Option<(std::time::Instant, Task<()>)>,
    host_health: HostHealth,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
    histories: BTreeMap<Uuid, ChatHistory>,
    file_scroll: UniformListScrollHandle,
    diff_scroll: UniformListScrollHandle,
    model_scroll: UniformListScrollHandle,
    conversation_scroll: UniformListScrollHandle,
    settings_open: bool,
    settings_loaded: bool,
    provider_form_open: bool,
    /// Query of the chat search palette (Cmd-K).
    conversation_query: Entity<InputState>,
    expanded_tools: BTreeSet<Uuid>,
    raw_tool_details: BTreeSet<Uuid>,
    /// The answer being read aloud, by content hash, so only its action row
    /// offers "Stop reading".
    reading_aloud: Option<u64>,
    schedules: Vec<xylo_runtime::scheduler::ScheduledTask>,
    schedule_prompt: Entity<TextareaState>,
    schedule_time: Entity<InputState>,
    schedule_frequency: usize,
    schedule_pending: Option<(Uuid, Option<String>)>,
    /// The new-task form is open on the Scheduled page.
    schedule_form_open: bool,
    quitting: bool,
    quit_prompt_open: bool,
    quit_after_save: Option<PathBuf>,
    /// The user chose to quit although tasks were running.
    quit_tasks_confirmed: bool,
    /// Quitting waits for pending saves; the banner offers Quit anyway.
    quit_waiting_for_saves: bool,
    model_picker: model_picker::ModelPickerUi,
    /// When context accounting last arrived, per chat.
    context_updated_at: BTreeMap<Uuid, i64>,
    /// Focus target of the chat transcript; bare Escape acts on the task only
    /// while the composer or the transcript has focus.
    transcript_focus: FocusHandle,
    native: NativeUi,
    documents: documents::DocumentUi,
    assets: AssetsUi,
    account: AccountUi,
    speech: SpeechUi,
    project_search: ProjectSearchUi,
    learning: LearningUi,
    updates: UpdateUi,
    history_search: HistorySearchUi,
    drafts: DraftUi,
    turn_queue: TurnQueueUi,
    attachments: AttachmentUi,
    image_generation: ImageGenerationUi,
    notifications: NotificationUi,
    plugins: plugins::PluginUi,
    extensions: extensions::ExtensionUi,
    environments: environments::EnvironmentUi,
    management_confirming: bool,
    recovery: recovery::RecoveryUi,
}
impl Xylo {
    pub fn new(
        host: anyhow::Result<(HostClient, async_channel::Receiver<HostEvent>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        shortcuts::bind(cx);
        let input = |placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder.to_string()))
        };
        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("How can I help you today?")
                .auto_grow(1, 8)
        });
        let editor = cx.new(|cx| EditorState::new(window, cx).language("rust"));
        let secret = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("API key")
                .masked(true)
        });
        let subscriptions =
            vec![
                cx.subscribe_in(&composer, window, |this, _, event, window, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.draft_changed(cx);
                    }
                    if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                        this.submit(window, cx);
                    }
                }),
            ];
        let editor_subscription = cx.subscribe(&editor, |this, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                this.editor_changed(cx);
            }
        });
        let conversation_query = input("Search chats", window, cx);
        let query_subscription = cx.subscribe_in(
            &conversation_query,
            window,
            |view, _, event: &InputEvent, window, cx| {
                match event {
                    InputEvent::Change => view.history_search_changed(cx),
                    InputEvent::PressEnter { .. } => view.palette_activate(window, cx),
                    _ => {}
                }
                cx.notify();
            },
        );
        let memory_content = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("A durable decision or preference to remember")
                .rows(3)
        });
        let mut subscriptions = subscriptions;
        subscriptions.push(
            cx.subscribe(&memory_content, |view, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    view.memory_form_changed(cx);
                }
            }),
        );
        subscriptions.push(query_subscription);
        let memory_query = input("Search project memory", window, cx);
        subscriptions.push(
            cx.subscribe(&memory_query, |view, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    view.memory_query_changed(cx);
                }
            }),
        );
        subscriptions.push(cx.observe_window_activation(window, |view, window, cx| {
            let active = window.is_window_active();
            if active {
                // macOS posts no "Reduce motion" change GPUI can follow, so a
                // change made while Xylo was in the background lands here.
                crate::motion::refresh_motion_preference(cx);
                // The person may have changed notification permission in
                // System Settings while Xylo was in the background.
                view.recheck_notification_permission(cx);
            }
            let _ = view.host.as_ref().map(|host| host.foreground(active));
        }));
        let (host, error) = match host {
            Ok((client, receiver)) => {
                cx.spawn_in(window, async move |this, cx| {
                    // Keep foreground work bounded while preserving every
                    // ordered event. Adjacent text deltas can share one append
                    // and repaint; tool/approval/status boundaries stay intact.
                    let mut pending = std::collections::VecDeque::new();
                    loop {
                        if pending.is_empty() {
                            let Ok(first) = receiver.recv().await else { break; };
                            pending.push_back(first);
                            for _ in 1..64 {
                                let Ok(event) = receiver.try_recv() else { break; };
                                if let (Some(HostEvent::Engine(previous)), HostEvent::Engine(next)) = (pending.back_mut(), &event)
                                    && previous.request_id == next.request_id
                                    && previous.session_id == next.session_id
                                    && let (EngineEvent::Delta { text: left }, EngineEvent::Delta { text: right }) = (&mut previous.payload, &next.payload)
                                    && left.len().saturating_add(right.len()) <= 64 * 1024
                                {
                                    left.push_str(right);
                                    previous.sequence = next.sequence;
                                } else {
                                    pending.push_back(event);
                                }
                            }
                        }
                        let applied = this.update_in(cx, |this, window, cx| {
                            let started = std::time::Instant::now();
                            while let Some(event) = pending.pop_front() {
                                this.event(event, window, cx);
                                if started.elapsed() >= std::time::Duration::from_millis(2) {
                                    break;
                                }
                            }
                            pending
                        });
                        let Ok(remainder) = applied else { break; };
                        pending = remainder;
                        // An empty queue already yields through recv(). Only
                        // pause while more foreground work is ready to run.
                        if !pending.is_empty() || !receiver.is_empty() {
                            cx.background_executor().timer(std::time::Duration::from_millis(1)).await;
                        }
                    }
                    let _ = this.update(cx, |this, cx| this.host_disconnected(cx));
                })
                .detach();
                (Some(client), None)
            }
            Err(error) => (None, Some(error.to_string())),
        };
        let host_health = if host.is_some() {
            HostHealth::Running
        } else {
            HostHealth::Unavailable
        };
        let mut view = Self {
            host,
            #[cfg(test)]
            validation_worker_commands: None,
            settings: AppSettings::default(),
            page: Page::Chat,
            workspace: None,
            workspace_ready: false,
            session: Uuid::new_v4(),
            conversations: vec![],
            conversations_before: None,
            conversations_has_more: false,
            conversations_loading_more: false,
            conversations_loaded: false,
            sidebar_state: Default::default(),
            messages: vec![],
            turns: BTreeMap::new(),
            composer,
            editor,
            editor_path: None,
            editor_assist: None,
            editor_clock: Default::default(),
            editor_assist_task: None,
            editor_dirty: false,
            pending_saves: BTreeMap::new(),
            editor_subscription: Some(editor_subscription),
            file_browser: Default::default(),
            artifacts: vec![],
            git_panel: Default::default(),
            terminal: None,
            terminal_subscription: None,
            terminal_cache: Default::default(),
            terminal_visible: false,
            terminal_starting: false,
            provider_name: input("Provider name", window, cx),
            provider_headers: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .placeholder(r#"{"X-Header":"value"}"#)
                    .rows(3)
            }),
            provider_discovery_url: input("Optional absolute model discovery URL", window, cx),
            provider_editor_source: None,
            provider_discovery: Default::default(),
            permissions: Default::default(),
            provider_editor_kind: String::new(),
            endpoint: input("https://provider.example/v1", window, cx),
            model: input("Model ID", window, cx),
            secret,
            context: input("Context tokens", window, cx),
            output: input("Output tokens", window, cx),
            models: vec![],
            model_context_limits: Default::default(),
            memory_query,
            memory_search_debounce: None,
            memory_content,
            memories: vec![],
            facts: memory::MemoryUi::default(),
            connector_name: input("Connector name", window, cx),
            connector_endpoint: input("https://…/mcp", window, cx),
            connector_command: input("Executable path", window, cx),
            connector_args: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .rows(3)
                    .placeholder("Arguments (one per line)")
            }),
            connector_pending: None,
            connector_pending_save: false,
            connector_editing: None,
            connector_credential: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("Access token (optional)")
                    .masked(true)
            }),
            connector_transport: "stdio".into(),
            connector_form_open: false,
            connectors: vec![],
            connector_tools: vec![],
            connector_inspected: None,
            connector_inspection_request: None,
            connector_loading: false,
            connector_error: None,
            status: "Connecting…".into(),
            status_expiry: None,
            host_health,
            error,
            _subscriptions: subscriptions,
            histories: BTreeMap::new(),
            file_scroll: UniformListScrollHandle::new(),
            diff_scroll: UniformListScrollHandle::new(),
            model_scroll: UniformListScrollHandle::new(),
            conversation_scroll: UniformListScrollHandle::new(),
            settings_open: false,
            settings_loaded: false,
            provider_form_open: false,
            conversation_query,
            expanded_tools: BTreeSet::new(),
            raw_tool_details: BTreeSet::new(),
            reading_aloud: None,
            schedules: vec![],
            schedule_prompt: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .placeholder("What should your assistant do?")
                    .rows(3)
            }),
            schedule_time: input("09:00", window, cx),
            schedule_frequency: 0,
            schedule_pending: None,
            schedule_form_open: false,
            quitting: false,
            quit_prompt_open: false,
            quit_after_save: None,
            quit_tasks_confirmed: false,
            quit_waiting_for_saves: false,
            model_picker: model_picker::ModelPickerUi::new(window, cx),
            context_updated_at: BTreeMap::new(),
            transcript_focus: cx.focus_handle(),
            native: NativeUi::new(),
            documents: documents::DocumentUi::new(window, cx),
            assets: AssetsUi::default(),
            account: AccountUi::default(),
            speech: SpeechUi::new(window, cx),
            project_search: ProjectSearchUi::new(window, cx),
            learning: LearningUi::new(window, cx),
            updates: UpdateUi::default(),
            history_search: HistorySearchUi::new(window, cx),
            drafts: DraftUi::new(window),
            turn_queue: TurnQueueUi::default(),
            attachments: AttachmentUi::new(),
            image_generation: ImageGenerationUi::new(window, cx),
            notifications: NotificationUi::default(),
            plugins: plugins::PluginUi::new(window, cx),
            extensions: extensions::ExtensionUi::new(window, cx),
            environments: environments::EnvironmentUi::new(window, cx),
            management_confirming: false,
            recovery: recovery::RecoveryUi::new(window, cx),
        };
        view.subscribe_documents(window, cx);
        view.subscribe_environments(cx);
        view.subscribe_recovery(cx);
        if let Some(host) = &view.host {
            let _ = host.foreground(window.is_window_active());
            let _ = host.list_conversations();
            let _ = host.connectors();
        }
        view.draft_switch_deferred(cx);
        view
    }
    /// Memory search follows typing. Each keystroke replaces the pending
    /// search, so only a pause of [`MEMORY_SEARCH_DEBOUNCE`] sends one, and a
    /// newer search supersedes an older one still in flight: its answer no
    /// longer matches the current query and is dropped on arrival.
    fn memory_query_changed(&mut self, cx: &mut Context<Self>) {
        self.memory_search_debounce = Some(cx.spawn(async move |view, cx| {
            cx.background_executor().timer(MEMORY_SEARCH_DEBOUNCE).await;
            let _ = view.update(cx, |view, cx| view.search_memory(cx));
        }));
    }
    fn save_editor(&mut self, quit_after: bool, cx: &mut Context<Self>) {
        let Some(path) = self.editor_path.clone() else {
            self.updates.after_save = None;
            self.error = Some("Choose a file before saving.".into());
            cx.notify();
            return;
        };
        if self.pending_saves.contains_key(&path) {
            if quit_after {
                self.quit_after_save = Some(path);
            }
            cx.notify();
            return;
        }
        let content = self.editor.read(cx).value().to_string();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|h| h.save_file(path.clone(), content.clone()));
        if result.is_ok() {
            self.pending_saves.insert(path.clone(), content);
            if quit_after {
                self.quit_after_save = Some(path);
            }
        }
        if result.is_err() {
            self.updates.after_save = None;
        }
        self.result(result, cx);
    }
    fn result(&mut self, result: anyhow::Result<()>, cx: &mut Context<Self>) {
        if let Err(error) = result {
            self.error = Some(error.to_string());
        }
        cx.notify();
    }
    fn host_call(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&HostClient) -> anyhow::Result<()>,
    ) {
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "The application service is unavailable. Restart Xylo to reconnect."
                )
            })
            .and_then(f);
        self.result(result, cx);
    }
    fn engine_command(&mut self, command: WorkerCommand, cx: &mut Context<Self>) {
        let session = self.session;
        #[cfg(test)]
        if let Some(probe) = &self.validation_worker_commands {
            let result = probe
                .try_send((session, command))
                .map_err(|error| anyhow::anyhow!("Test worker dispatch: {error}"));
            self.result(result, cx);
            return;
        }
        self.host_call(cx, |h| h.send_to(session, command));
    }
    fn set_input(
        input: &Entity<InputState>,
        value: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let value: SharedString = value.into();
        input.update(cx, |s, cx| s.set_value(value, window, cx));
    }
    fn provider_form(&mut self, p: &ProviderConfig, window: &mut Window, cx: &mut Context<Self>) {
        self.models.clear();
        self.model_context_limits.clear();
        self.provider_editor_source = Some(p.clone());
        self.provider_editor_kind = providers::provider_kind(p).into();
        Self::set_input(&self.secret, "", window, cx);
        Self::set_input(&self.provider_name, p.name.clone(), window, cx);
        Self::set_input(&self.endpoint, p.endpoint.clone(), window, cx);
        Self::set_input(&self.model, p.model.clone(), window, cx);
        Self::set_input(&self.context, p.context_tokens.to_string(), window, cx);
        Self::set_input(&self.output, p.output_tokens.to_string(), window, cx);
        Self::set_input(
            &self.provider_discovery_url,
            p.discovery_url.clone().unwrap_or_default(),
            window,
            cx,
        );
        self.provider_headers.update(cx, |input, cx| {
            input.set_value(
                serde_json::to_string_pretty(&p.headers).unwrap_or_else(|_| "{}".into()),
                window,
                cx,
            )
        });
    }
    /// The connection the provider form describes. While a save awaits the
    /// host, that is the connection being saved (with its new credential
    /// reference), so the host's answer matches the form.
    fn provider_from_form(&self, cx: &App) -> anyhow::Result<ProviderConfig> {
        let mut provider = self
            .provider_discovery
            .saving()
            .or(self.provider_editor_source.as_ref())
            .cloned()
            .unwrap_or_default();
        let original_endpoint = provider.endpoint.trim_end_matches('/').to_owned();
        let original_family = provider.family.clone();
        provider.name = self.provider_name.read(cx).value().trim().into();
        provider.endpoint = self
            .endpoint
            .read(cx)
            .value()
            .trim()
            .trim_end_matches('/')
            .into();
        provider.model = self.model.read(cx).value().trim().into();
        provider.context_tokens = self.context.read(cx).value().parse()?;
        provider.output_tokens = self.output.read(cx).value().parse()?;
        let headers = self.provider_headers.read(cx).value();
        provider.headers = if headers.trim().is_empty() {
            Default::default()
        } else {
            serde_json::from_str(headers.as_ref()).map_err(|_| {
                anyhow::anyhow!("Extra headers must be a JSON object with text values")
            })?
        };
        let discovery = self.provider_discovery_url.read(cx).value();
        provider.discovery_url =
            (!discovery.trim().is_empty()).then(|| discovery.trim().to_owned());
        if provider.endpoint != original_endpoint || provider.family != original_family {
            if self
                .provider_editor_source
                .as_ref()
                .is_some_and(|source| source.headers == provider.headers)
            {
                provider.headers.clear();
            }
            provider.credential_ref = None;
            provider.managed_local = false;
        }
        provider.reasoning_effort = provider.effective_reasoning_effort().map(str::to_owned);
        Ok(provider)
    }
    fn event(&mut self, event: HostEvent, window: &mut Window, cx: &mut Context<Self>) {
        if let HostEvent::Engine(envelope) = &event {
            self.note_context_usage(envelope);
        }
        match event {
            HostEvent::ActionReviews { request_id, result } => {
                self.action_reviews_received(request_id, result, cx)
            }
            HostEvent::ActionReview { request_id, result } => {
                self.action_review_received(request_id, *result, window, cx)
            }
            HostEvent::ActionResolved { request_id, result } => {
                self.action_resolved(request_id, result, window, cx)
            }
            HostEvent::ActionResolution { request_id, result } => {
                self.action_resolution_received(request_id, result, window, cx)
            }
            HostEvent::ActionReviewForm { request_id, result } => {
                self.action_review_form_received(request_id, result, window, cx)
            }
            HostEvent::ActionReviewFormSaved { request_id, result } => {
                self.action_review_form_saved(request_id, result, window, cx)
            }
            HostEvent::DocumentDraftRelinked { request_id, result } => {
                self.document_relinked(request_id, *result, window, cx)
            }
            HostEvent::DocumentDraftRelinkStatus { request_id, result } => {
                self.document_relink_status(request_id, *result, window, cx)
            }
            HostEvent::DocumentDraftLoaded { request_id, result } => {
                self.document_draft_loaded(request_id, result, window, cx)
            }
            HostEvent::DocumentDraftSaved { request_id, result } => {
                self.document_draft_saved(request_id, result, window, cx)
            }
            HostEvent::DocumentDrafts { request_id, result } => {
                self.document_drafts_listed(request_id, result, cx)
            }
            HostEvent::NotificationActivation {
                activation: _,
                conversation,
            } => {
                window.activate_window();
                cx.activate(true);
                if let Some(conversation) = conversation {
                    self.open_conversation(conversation.id, conversation.workspace, window, cx);
                } else {
                    self.navigate(Page::Schedules, cx);
                }
            }
            HostEvent::SteeringResult { request_id, result } => {
                self.steering_result(request_id, result, window, cx)
            }
            HostEvent::DraftLoaded { request_id, result } => {
                self.draft_loaded(request_id, result, window, cx)
            }
            HostEvent::DraftSaved { request_id, result } => {
                self.draft_saved(request_id, result, window, cx)
            }
            HostEvent::Drafts { request_id, result } => self.drafts_listed(request_id, result, cx),
            HostEvent::QueueAdded { request_id, result } => {
                self.queue_added(request_id, result, window, cx)
            }
            HostEvent::TurnQueue {
                request_id,
                session_id,
                result,
            } => self.turn_queue_result(request_id, session_id, result, cx),
            HostEvent::QueuedTurnStarted { session_id, item } => {
                self.queued_turn_started(session_id, item, cx)
            }
            HostEvent::DraftAdmitted { request_id, result } => {
                self.draft_admitted(request_id, result, window, cx)
            }
            HostEvent::ImageGenerationHistory { request_id, result } => {
                self.image_generation_history_received(request_id, result, cx);
            }
            HostEvent::ImageGeneration { request_id, result } => {
                self.image_generation_received(request_id, result, cx);
            }
            HostEvent::ImageGenerationProvider { request_id, result } => {
                self.image_generation_provider_received(request_id, result, window, cx);
            }
            HostEvent::ImageImported {
                request_id,
                session_id,
                result,
            } => {
                self.image_imported(request_id, session_id, result, cx);
            }
            HostEvent::ImagePreview {
                request_id,
                session_id,
                attachment_id,
                result,
            } => {
                self.image_preview(request_id, session_id, attachment_id, result, cx);
            }
            HostEvent::HistorySearch { request_id, result } => {
                self.history_search_result(request_id, result, cx)
            }
            HostEvent::HistoryWindow(page) => self.history_window_result(page, cx),
            HostEvent::WorkspaceOpened { requested, root } => {
                if self.workspace.as_ref() == Some(&requested) {
                    // The switch was checked when it was requested; here the
                    // draft is only saved. A draft problem shows as its own
                    // error and must not drop the open, or the project would
                    // never become ready.
                    let _ = self.draft_before_switch(cx);
                    if requested != root && !self.memory_before_switch(cx) {
                        return;
                    }
                    if requested != root && !self.document_before_workspace_switch(cx) {
                        return;
                    }
                    self.workspace = Some(root.clone());
                    if !self.settings.ui.open_projects.contains(&root) {
                        self.settings.ui.open_projects.push(root.clone());
                    }
                    self.workspace_ready = true;
                    if self.terminal.is_none()
                        && let Some((terminal, subscription)) = self.terminal_cache.remove(&root)
                    {
                        self.terminal = Some(terminal);
                        self.terminal_subscription = subscription;
                    }
                    self.terminal_visible = self.settings.ui.work_panel_open
                        && self.settings.ui.work_panel == "terminal"
                        && self.terminal.is_some();
                    if requested != root {
                        self.memory_after_switch(window, cx);
                        self.document_after_workspace_switch(window, cx);
                        self.management_scope_changed(cx);
                    }
                    self.document_workspace_ready(cx);
                    self.draft_switch_deferred(cx);
                    self.settings.last_workspace = Some(root);
                    self.load_file_page(None, cx);
                    self.persist_ui(cx);
                    self.sidebar_state.dirty = true;
                    self.recovery_activate(window, cx);
                    if self.settings.ui.work_panel_open {
                        self.activate_work_panel(window, cx);
                    }
                    if self.page == Page::Plugins {
                        self.refresh_plugins(cx);
                    }
                    if self.page == Page::Environments {
                        self.refresh_environments(cx);
                    }
                    if self.page == Page::Memory {
                        self.search_memory(cx);
                        self.refresh_learning(cx);
                    }
                    if self.page == Page::Permissions {
                        self.refresh_approval_rules(cx);
                    }
                }
            }
            HostEvent::WorkspaceIndexed(index) => self.workspace_indexed(index, cx),
            HostEvent::WorkspaceFiles { request_id, result } => {
                self.file_page_result(request_id, result, cx)
            }
            HostEvent::WorkspaceUnavailable { root, message } => {
                self.workspace_unavailable(root, message, window, cx)
            }
            HostEvent::ProjectSearch { request_id, result } => {
                self.project_search_result(request_id, result, cx)
            }
            HostEvent::OpenedFile {
                request_id,
                path,
                result,
            } => self.opened_file(request_id, path, result, window, cx),
            HostEvent::AuthorizationUrl(url) => cx.open_url(&url),
            HostEvent::Capability {
                request_id,
                name,
                result,
            } => self.capability_result(request_id, &name, result, window, cx),
            HostEvent::LspResponse { request_id, result } => {
                if let Some(assist) = &self.editor_assist {
                    assist.complete(request_id, result);
                }
            }
            HostEvent::LspDiagnostics {
                path,
                version,
                diagnostics,
            } => self.editor_diagnostics(path, version, diagnostics, cx),
            HostEvent::Schedules(tasks) => self.schedules = tasks,
            HostEvent::ScheduleMutation { request_id, result } => {
                self.schedule_mutated(request_id, result, window, cx)
            }
            HostEvent::Stopped => {
                cx.quit();
                return;
            }
            HostEvent::Settings(mut settings) => {
                let account_corrected = self.account_sanitize_settings(&mut settings);
                let initial = !self.settings_loaded;
                self.settings_loaded = true;
                let workspace = settings.last_workspace.clone();
                let local_ui = (!initial).then(|| self.settings.ui.clone());
                if initial || !self.provider_form_open {
                    self.provider_form(&settings.provider, window, cx);
                }
                self.settings = settings;
                if let Some(mut ui) = local_ui {
                    for root in &self.settings.ui.open_projects {
                        if !ui.open_projects.contains(root) {
                            ui.open_projects.push(root.clone());
                        }
                    }
                    self.settings.ui = ui;
                }
                self.sidebar_state.collapsed = self
                    .settings
                    .ui
                    .collapsed_projects
                    .iter()
                    .cloned()
                    .collect();
                self.sidebar_state.dirty = true;
                self.apply_theme(window, cx);
                if self.workspace.is_none()
                    && let Some(path) = workspace
                {
                    self.set_workspace(path, window, cx);
                }
                if initial {
                    if self.workspace.is_none() {
                        self.memory_form_activate(window, cx);
                    }
                    let page = Page::from_key(&self.settings.ui.active_page);
                    self.page = page;
                    self.settings_open = page.is_settings();
                    self.load_page(page, cx);
                }
                if account_corrected {
                    let corrected = self.settings.clone();
                    self.host_call(cx, |host| host.save_settings(corrected));
                }
            }
            HostEvent::ConversationPage(page) => self.conversation_page(page, cx),
            HostEvent::HistoryPage(page) => self.history_page(page, cx),
            HostEvent::ModelsFailed {
                generation,
                provider,
                message,
            } => {
                if self
                    .host
                    .as_ref()
                    .is_none_or(|host| host.provider_discovery_generation() == generation)
                    && self
                        .provider_from_form(cx)
                        .is_ok_and(|current| providers::same_connection(&current, &provider))
                {
                    self.provider_catalog_failed(&provider, generation, &message);
                }
            }
            HostEvent::Models {
                generation,
                provider,
                models,
                context_limits,
                reasoning_options,
                default_reasoning_efforts,
            } => {
                if self
                    .host
                    .as_ref()
                    .is_some_and(|host| host.provider_discovery_generation() != generation)
                {
                    return;
                }
                let Ok(current) = self.provider_from_form(cx) else {
                    return;
                };
                if !providers::same_connection(&current, &provider) {
                    return;
                }
                if !self.provider_catalog_received(&provider, generation, &models) {
                    return;
                }
                self.native.select_local_model = false;
                if let Err(error) = self.register_discovered_models(
                    &provider,
                    &models,
                    &context_limits,
                    &reasoning_options,
                    &default_reasoning_efforts,
                    window,
                    cx,
                ) {
                    self.error = Some(error.to_string());
                }
                self.model_context_limits = context_limits;
                self.status = format!("Discovered {} models", models.len()).into();
                self.models = models;
            }
            HostEvent::SaveFailed { path, message } => {
                self.pending_saves.remove(&path);
                if self.updates.after_save.as_ref() == Some(&path) {
                    self.updates.after_save = None;
                }
                if self.quit_after_save.as_ref() == Some(&path) {
                    self.quit_after_save = None;
                }
                self.error = Some(message);
            }
            HostEvent::Saved(path) => {
                if let Some(saved) = self.pending_saves.remove(&path)
                    && self.editor_path.as_ref() == Some(&path)
                    && self.editor.read(cx).value().as_ref() == saved
                {
                    self.editor_dirty = false;
                    self.project_search.external_conflict = false;
                }
                self.status = format!("Saved {}", path.display()).into();
                if self.updates.after_save.as_ref() == Some(&path) {
                    self.updates.after_save = None;
                    if !self.editor_dirty {
                        self.begin_update_install(window, cx);
                    } else {
                        self.error=Some("The file changed while saving. Review it before installing the update.".into());
                    }
                }
                if self.quit_after_save.as_ref() == Some(&path) {
                    self.quit_after_save = None;
                    if !self.editor_dirty {
                        self.begin_shutdown(cx);
                    } else {
                        self.error = Some(
                            "The file changed while saving. Review your changes before quitting."
                                .into(),
                        );
                    }
                }
            }
            HostEvent::GitStatus {
                request_id,
                root,
                result,
            } => self.git_status_received(request_id, root, result, cx),
            HostEvent::GitFileDiff {
                request_id,
                root,
                result,
            } => self.git_diff_received(request_id, root, result, cx),
            HostEvent::FactVersions { request_id, result } => {
                self.fact_versions_received(request_id, result, cx)
            }
            HostEvent::FactDeletePreview { request_id, result } => {
                self.fact_delete_previewed(request_id, result, window, cx)
            }
            HostEvent::FactDeleted { request_id, result } => {
                self.fact_deleted(request_id, result, cx)
            }
            HostEvent::MemoryFormLoaded { request_id, result } => {
                self.memory_form_loaded(request_id, result, window, cx)
            }
            HostEvent::MemoryFormSaved { request_id, result } => {
                self.memory_form_saved(request_id, result, window, cx)
            }
            HostEvent::MemoryPage { request_id, result } => {
                self.memory_received(request_id, result, cx)
            }
            HostEvent::MemoryMutation { request_id, result } => {
                self.memory_mutated(request_id, result, window, cx)
            }
            HostEvent::MemoryExport { request_id, result } => {
                self.memory_exported(request_id, result, cx)
            }
            HostEvent::Connectors(connectors) => self.connectors = connectors,
            HostEvent::ConnectorMutation { request_id, result } => {
                self.connector_mutation_result(request_id, result, window, cx)
            }
            HostEvent::ConnectorTools {
                request_id,
                workspace,
                connector_id,
                tools,
            } => {
                if self.connector_inspected == Some(connector_id)
                    && self.connector_inspection_request == Some(request_id)
                    && self.workspace == workspace
                {
                    self.connector_inspection_request = None;
                    self.connector_loading = false;
                    self.connector_error = None;
                    self.connector_tools = tools;
                }
            }
            HostEvent::ConnectorToolsFailed {
                request_id,
                workspace,
                connector_id,
                message,
            } => {
                if self.connector_inspected == Some(connector_id)
                    && self.connector_inspection_request == Some(request_id)
                    && self.workspace == workspace
                {
                    self.connector_inspection_request = None;
                    self.connector_loading = false;
                    self.connector_tools.clear();
                    self.connector_error = Some(message);
                }
            }
            HostEvent::Error(error) => {
                self.quit_after_save = None;
                self.error = Some(error.clone());
                self.log(format!("ERROR {error}"));
            }
            HostEvent::Status(status) => {
                self.status = status.into();
            }
            HostEvent::ApprovalRules { workspace, result } => {
                self.approval_rules_received(workspace, result, cx)
            }
            HostEvent::ConversationRenamed(conversation) => self.conversation_renamed(conversation),
            HostEvent::Engine(envelope) => {
                // Engine events always name their chat; one without a session
                // cannot be attributed to the open chat safely.
                let Some(session) = envelope.session_id else {
                    tracing::debug!(
                        request_id = %envelope.request_id,
                        "Dropped an engine event without a chat"
                    );
                    return;
                };
                self.engine_event(session, envelope.payload, window, cx);
            }
        }
        cx.notify();
    }
    pub(super) fn record_artifact(&mut self, path: PathBuf) {
        if !self.artifacts.contains(&path) {
            if self.artifacts.len() >= 256 {
                self.artifacts.remove(0);
            }
            self.artifacts.push(path);
        }
    }
    fn log(&mut self, event: String) {
        if !event.is_empty() {
            tracing::debug!(%event,"Task activity");
        }
    }
    fn running(&self) -> bool {
        self.turns.get(&self.session).is_some_and(|t| t.running)
    }
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.recovery_blocks_submit() {
            self.recovery.open = true;
            self.error = Some("Review the interrupted action before continuing this chat. Your message stays in the composer.".into());
            cx.notify();
            return;
        }
        let blocked = if self.attachments.importing_for(self.session) {
            Some("Wait for the images to finish importing, then send.")
        } else if self.draft_flush_pending() {
            Some("Drafts are being saved before Xylo closes. Choose Keep working to send.")
        } else if self.memory_forms_flush_pending() {
            Some("Wait for your memory changes to finish saving, then send.")
        } else {
            None
        };
        if let Some(reason) = blocked {
            self.show_composer_hint(reason, cx);
            return;
        }
        self.submit_persisted_draft(window, cx);
        self.refresh_composer_placeholder(cx);
    }
    fn more_conversations(&mut self, cx: &mut Context<Self>) {
        if self.conversations_loading_more || !self.conversations_has_more {
            return;
        }
        self.conversations_loading_more = true;
        self.sidebar_state.dirty = true;
        let before = self.conversations_before.clone();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.list_conversations_page(before, 100));
        if result.is_err() {
            self.conversations_loading_more = false;
            // The list's end loads more as it renders; a failed request must
            // not be retried on every frame.
            self.conversations_has_more = false;
            self.sidebar_state.dirty = true;
        }
        self.result(result, cx);
    }
    fn conversation_page(&mut self, page: ConversationPage, cx: &mut Context<Self>) {
        let advance = self.conversations_loading_more || self.conversations.is_empty();
        let before = self.conversations.clone();
        let mut merged: BTreeMap<_, _> = self
            .conversations
            .drain(..)
            .map(|conversation| (conversation.id, conversation))
            .collect();
        for conversation in page.conversations {
            merged.insert(conversation.id, conversation);
        }
        // Pages leave archived chats out; a merged copy must not keep one.
        self.conversations = merged
            .into_values()
            .filter(|conversation| !conversation.archived)
            .collect();
        self.conversations.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| right.id.cmp(&left.id))
        });
        if advance {
            self.conversations_before = page.before;
            self.conversations_has_more = page.has_more;
        }
        self.conversations_loading_more = false;
        self.note_conversation_changes(&before);
        self.conversations_loaded = true;
        self.sidebar_state.dirty = true;
        cx.notify();
    }
    fn new_chat(&mut self, cx: &mut Context<Self>) {
        if !self.recovery_before_switch(cx) {
            return;
        }
        if !self.draft_before_switch(cx) {
            return;
        }
        self.stash_history();
        self.session = Uuid::new_v4();
        self.recovery_scope_changed(cx);
        self.draft_switch_deferred(cx);
        self.messages.clear();
        self.sidebar_state.note_new_chat(self.session);
        self.sidebar_state.dirty = true;
        self.page = Page::Chat;
        self.settings_open = false;
        self.refresh_composer_placeholder(cx);
        cx.notify();
    }
    fn pick_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            if let Some(folder) = rfd::AsyncFileDialog::new()
                .set_title("Open project folder")
                .pick_folder()
                .await
            {
                let path = folder.path().to_path_buf();
                let _ = this.update_in(cx, |this, window, cx| this.set_workspace(path, window, cx));
            }
        })
        .detach();
    }
    fn set_workspace(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if self.workspace.as_ref() == Some(&path) {
            return;
        }
        if !self.recovery_before_switch(cx) {
            return;
        }
        if self.editor_dirty {
            self.error = Some("Save your editor changes before switching projects.".into());
            cx.notify();
            return;
        }
        if !self.draft_before_switch(cx) || !self.workspace_switch_allowed(cx) {
            return;
        }
        let restoring =
            self.workspace.is_none() && self.settings.last_workspace.as_ref() == Some(&path);
        // Taken before parking, which does not change the current panel.
        let current_panel = WorkspacePanelPreferences {
            open: self.settings.ui.work_panel_open,
            panel: self.settings.ui.work_panel.clone(),
            width: self.settings.ui.work_panel_width,
        };
        self.new_chat(cx);
        self.park_workspace(cx);
        let panel_state = self
            .settings
            .ui
            .project_panels
            .get(&path)
            .cloned()
            .unwrap_or_else(|| {
                if restoring {
                    current_panel
                } else {
                    WorkspacePanelPreferences::default()
                }
            });
        self.settings.ui.work_panel_open = panel_state.open;
        self.settings.ui.work_panel = panel_state.panel;
        self.settings.ui.work_panel_width = panel_state.width;
        self.workspace = Some(path.clone());
        self.settings.last_workspace = Some(path.clone());
        self.after_workspace_change(window, cx);
        if let Some((terminal, subscription)) = self.terminal_cache.remove(&path) {
            self.terminal = Some(terminal);
            self.terminal_subscription = subscription;
        }
        self.terminal_visible = self.settings.ui.work_panel_open
            && self.settings.ui.work_panel == "terminal"
            && self.terminal.is_some();
        self.host_call(cx, |h| h.open_workspace(path.clone()));
    }
    /// Whether project-scoped forms allow leaving the current project.
    fn workspace_switch_allowed(&mut self, cx: &mut Context<Self>) -> bool {
        self.management_before_switch(cx)
            && self.memory_before_switch(cx)
            && self.document_before_workspace_switch(cx)
    }
    /// Leaves the open project for the no-project state: project-scoped forms
    /// are checked first, the project's shells are parked (not stopped) so
    /// returning to the project resumes them. Returns false when a form blocks
    /// the switch.
    pub(super) fn leave_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.workspace.is_some() && !self.workspace_switch_allowed(cx) {
            return false;
        }
        self.park_workspace(cx);
        self.settings.last_workspace = None;
        self.after_workspace_change(window, cx);
        self.persist_ui(cx);
        true
    }
    /// Tears down the open project's state. Its terminal and subscription are
    /// kept in `terminal_cache` under the project root.
    fn park_workspace(&mut self, cx: &mut Context<Self>) {
        self.remember_work_panel();
        self.stop_speech(cx);
        self.cancel_project_search(cx);
        self.project_search.reset();
        self.history_search.reset();
        self.learning.clear();
        self.management_scope_changed(cx);
        let terminal = self
            .terminal
            .take()
            .map(|terminal| (terminal, self.terminal_subscription.take()));
        park_terminal(&mut self.terminal_cache, self.workspace.take(), terminal);
        self.terminal_subscription = None;
        self.terminal_visible = false;
        self.terminal_starting = false;
        self.workspace_ready = false;
        self.reset_file_browser();
        self.git_panel = Default::default();
        self.memories.clear();
        self.detach_editor_assist();
        self.editor_path = None;
        self.editor_dirty = false;
    }
    /// Re-scopes memory, documents and drafts to the current project.
    fn after_workspace_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.memory_after_switch(window, cx);
        self.document_after_workspace_switch(window, cx);
        self.draft_switch_deferred(cx);
        self.sidebar_state.dirty = true;
    }
    /// The project being opened could not be opened. Return to the
    /// no-project state so nothing keeps waiting for it; the project stays in
    /// the sidebar to retry.
    fn workspace_unavailable(
        &mut self,
        root: PathBuf,
        message: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.workspace.as_ref() != Some(&root) {
            return;
        }
        self.error = Some(message);
        self.park_workspace(cx);
        self.settings.last_workspace = None;
        self.after_workspace_change(window, cx);
        self.persist_ui(cx);
    }
    fn refresh_files(&mut self, cx: &mut Context<Self>) {
        if let Some(host) = &self.host {
            let result = host.refresh_workspace();
            self.result(result, cx);
            self.load_file_page(None, cx);
        }
    }
    /// Cmd-J: shows the terminal in the chat's work panel, or hides the
    /// panel when it is already showing the terminal.
    fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let showing = self.page == Page::Chat
            && !self.settings_open
            && self.settings.ui.work_panel_open
            && self.settings.ui.work_panel == "terminal";
        if showing {
            self.close_work_panel(cx);
            return;
        }
        self.settings_open = false;
        self.page = Page::Chat;
        self.settings.ui.work_panel_open = true;
        self.settings.ui.work_panel = "terminal".into();
        self.persist_ui(cx);
        self.show_terminal(window, cx);
    }
    /// Marks the terminal tab as shown and starts the project's shell the
    /// first time it is needed.
    fn show_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.terminal_visible = true;
        if self.terminal_visible && self.terminal.is_none() && !self.terminal_starting {
            let Some(path) = self.workspace.clone() else {
                self.error = Some("Open a project folder to start its shell.".into());
                return;
            };
            self.terminal_starting = true;
            let requested = path.clone();
            let task = cx
                .background_executor()
                .spawn(async move { TerminalSession::spawn(&path) });
            cx.spawn_in(window, async move |this, cx| {
                let result = task.await;
                let _ = this.update_in(cx, |this, window, cx| {
                    if this.workspace.as_ref() != Some(&requested) {
                        return;
                    }
                    this.terminal_starting = false;
                    match result {
                        Ok((session, events)) => {
                            let terminal = cx.new(|cx| {
                                TerminalWorkspace::new(
                                    requested.clone(),
                                    session,
                                    events,
                                    window,
                                    cx,
                                )
                            });
                            this.terminal_subscription = Some(cx.subscribe_in(
                                &terminal,
                                window,
                                |this, source, event, window, cx| {
                                    if this.terminal.as_ref().map(Entity::entity_id)
                                        != Some(source.entity_id())
                                    {
                                        return;
                                    }
                                    match event {
                                        TerminalWorkspaceEvent::Empty => {
                                            this.terminal = None;
                                            this.terminal_subscription = None;
                                            // Closing the last shell closes the
                                            // panel that showed it.
                                            if this.terminal_visible {
                                                this.close_work_panel(cx);
                                            }
                                        }
                                        TerminalWorkspaceEvent::AssistRequested { prompt } => {
                                            this.terminal_assistance_chat(
                                                prompt.clone(),
                                                window,
                                                cx,
                                            );
                                        }
                                    }
                                    cx.notify();
                                },
                            ));
                            terminal.update(cx, |terminal, cx| terminal.focus_active(window, cx));
                            this.terminal = Some(terminal);
                        }
                        Err(error) => this.error = Some(error.to_string()),
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        cx.notify();
    }
    fn navigate(&mut self, page: Page, cx: &mut Context<Self>) {
        if page == Page::Providers {
            self.provider_form_open = false;
        }
        self.page = page;
        self.settings_open = page.is_settings();
        self.load_page(page, cx);
        self.persist_ui(cx);
        cx.notify();
    }
    /// Loads what `page` shows when it opens, at launch or by navigation.
    fn load_page(&mut self, page: Page, cx: &mut Context<Self>) {
        match page {
            Page::Schedules => self.refresh_schedules(cx),
            Page::Assets => self.refresh_assets(cx),
            Page::Appearance => self.refresh_updates(cx),
            Page::Notifications => self.refresh_notifications(cx),
            Page::Plugins => self.refresh_plugins(cx),
            Page::Environments => self.refresh_environments(cx),
            Page::Images => self.load_image_generation_provider(cx),
            Page::Permissions => self.refresh_approval_rules(cx),
            Page::Files => {
                self.settings.ui.work_panel_open = true;
                self.settings.ui.work_panel = "files".into();
                self.refresh_files(cx);
            }
            Page::Artifacts => self.document_workspace_ready(cx),
            Page::Changes => {
                self.settings.ui.work_panel_open = true;
                self.settings.ui.work_panel = "changes".into();
                self.refresh_git(cx);
            }
            Page::Memory => {
                self.search_memory(cx);
                self.refresh_learning(cx);
            }
            Page::Connectors => self.host_call(cx, |h| h.connectors()),
            Page::Skills => self.refresh_learning(cx),
            _ => {}
        }
    }
    fn scope(&self) -> String {
        self.workspace
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "global".into())
    }
    /// Sends the provider form to the host. Nothing changes locally until the
    /// host answers: the typed key stays in the form and the stored settings
    /// keep their previous credential reference, so a failed save can be
    /// retried and never leaves a provider without its secret.
    fn save_provider(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let result = self
            .provider_save_request(cx)
            .and_then(|(settings, provider, secret)| {
                self.dispatch_provider_save(settings, provider, secret)
            });
        if let Err(error) = result {
            self.provider_save_failed(&error);
        }
        cx.notify();
    }
    /// Settings, connection and secret for one provider save. A typed key gets
    /// a credential reference here; it reaches stored settings only after the
    /// host has stored the key under it.
    fn provider_save_request(
        &self,
        cx: &App,
    ) -> anyhow::Result<(AppSettings, ProviderConfig, Option<String>)> {
        let mut provider = self.provider_from_form(cx)?;
        if let Some(limit) = self.model_context_limits.get(&provider.model) {
            provider.context_tokens = *limit;
            provider.output_tokens = provider.output_tokens.min(*limit / 4);
        }
        provider.validate_connection()?;
        let key = self.secret.read(cx).value().trim().to_owned();
        if providers::requires_api_key(&self.provider_editor_kind)
            && provider.credential_ref.is_none()
            && key.is_empty()
        {
            anyhow::bail!("Enter an API key for this provider");
        }
        if !key.is_empty() && provider.credential_ref.is_none() {
            provider.credential_ref = Some(format!("provider:{}", Uuid::new_v4()));
        }
        let mut settings = self.settings.clone();
        if let Some(existing) = settings.providers.iter_mut().find(|saved| {
            providers::same_connection(saved, &provider) && saved.model == provider.model
        }) {
            *existing = provider.clone();
        } else {
            settings.providers.push(provider.clone());
        }
        Ok((settings, provider, (!key.is_empty()).then_some(key)))
    }
}
impl Xylo {
    /// The host is up but has not sent settings yet: the workspace would
    /// render with defaults and then jump, so a calm loading state shows
    /// instead. A host that stopped or reported an error shows the workspace
    /// with its banner, so a failure is never hidden behind the spinner.
    fn starting_up(&self) -> bool {
        self.host.is_some() && self.host_running() && self.error.is_none() && !self.settings_loaded
    }
    fn startup_screen(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let since = crate::motion::shown_since("xylo-startup", window, cx);
        div()
            .id("xylo-startup")
            .test_support()
            .size_full()
            .bg(rgb(bg()))
            .flex()
            .items_center()
            .justify_center()
            .child(crate::motion::fade_in(
                "xylo-startup-enter",
                crate::motion::loading_state("xylo-startup", "Starting Xylo…", since, window, cx),
            ))
            .into_any_element()
    }
}
impl Render for Xylo {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.updates.installing {
            return div()
                .size_full()
                .bg(rgb(bg()))
                .text_color(rgb(text()))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_3()
                .child(crate::motion::spinner(icons::LARGE).color(rgb(muted()).into()))
                .child(empty_state(
                    "Installing update…",
                    "Keep Xylo open until installation finishes.",
                ))
                .into_any_element();
        }
        if self.quitting {
            return self.quitting_screen();
        }
        if self.starting_up() {
            return self.startup_screen(window, cx);
        }
        let body = match self.page {
            Page::Assets => self.assets_page(cx),
            Page::Appearance => self.general_settings_page(cx),
            Page::Permissions => self.permission_settings_page(cx),
            Page::Skills => self.skills_settings_page(cx),
            Page::Chat | Page::Files | Page::Changes => self.native_workspace(window, cx),
            Page::Search => self.project_search_page(window, cx),
            Page::Drafts => self.drafts_page(cx),
            Page::Notifications => self.notifications_page(cx),
            Page::Images => self.image_provider_settings_page(cx),
            Page::Voice => self.voice_settings_page(cx),
            Page::Artifacts => self.artifacts_page(cx),
            Page::Providers => self.providers_page(cx),
            Page::Memory => self.memory_page(cx),
            Page::Connectors => self.connections_settings_page(cx),
            Page::Plugins => self.plugins_page(cx),
            Page::Environments => self.environments_page(cx),
            Page::Schedules => self.schedules_page(cx),
        };
        let mut main = div()
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(rgb(bg()))
            .flex()
            .flex_col();
        let sidebar_hidden = !self.settings.ui.sidebar_visible && !self.settings_open;
        let mut toolbar = div()
            .h(px(52.))
            .flex_shrink_0()
            .pl(px(if sidebar_hidden { 12. } else { 20. }))
            .pr(px(12.))
            .flex()
            .items_center()
            .gap_2();
        if sidebar_hidden {
            toolbar = toolbar
                .child(
                    icons::icon_button("toggle-sidebar", AppIcon::SidebarToggle, "Show sidebar")
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.settings.ui.sidebar_visible = true;
                            view.persist_ui(cx);
                            cx.notify();
                        })),
                )
                .child(brand_mark(20.).ml(px(4.)).mr(px(2.)));
        }
        toolbar = toolbar.child(self.toolbar_title());
        // The chat's menu and work panel belong to the chat page only.
        if matches!(self.page, Page::Chat | Page::Files | Page::Changes) {
            if let Some(actions) = self.conversation_actions(cx) {
                toolbar = toolbar.child(actions);
            }
            let work_open = self.settings.ui.work_panel_open
                || matches!(self.page, Page::Files | Page::Changes);
            toolbar = toolbar.child(
                icons::icon_button(
                    "toggle-work-panel",
                    AppIcon::PanelToggle,
                    if work_open {
                        "Hide work panel"
                    } else {
                        "Show work panel"
                    },
                )
                .when(work_open, |button| {
                    button.text_color(rgb(text())).bg(rgb(selected()))
                })
                .on_click(cx.listener(|view, _, window, cx| {
                    let was_open = view.settings.ui.work_panel_open
                        || matches!(view.page, Page::Files | Page::Changes);
                    if view.page == Page::Files {
                        view.settings.ui.work_panel = "files".into();
                    }
                    if view.page == Page::Changes {
                        view.settings.ui.work_panel = "changes".into();
                    }
                    if was_open {
                        view.close_work_panel(cx);
                        return;
                    }
                    view.page = Page::Chat;
                    view.settings.ui.work_panel_open = true;
                    view.activate_work_panel(window, cx);
                    view.persist_ui(cx);
                    cx.notify();
                })),
            );
        }
        // Settings pages start with their own header; the window keeps its
        // native titlebar, so no toolbar band sits above them.
        if !self.settings_open {
            main = main.child(toolbar);
        }
        if !matches!(
            self.page,
            Page::Chat | Page::Files | Page::Changes | Page::Voice
        ) && self.speech_recording_active()
        {
            main = main.child(self.global_recording_controls(cx));
        }
        if !self.host_running() {
            main = main.child(self.host_stopped_banner(cx));
        } else if let Some(error) = self.error.clone() {
            main = main.child(self.error_notice(&error, cx));
        }
        if let Some(banner) = self.pending_saves_banner(cx) {
            main = main.child(banner);
        }
        main = main.child(div().flex_1().min_h_0().child(body));
        if let Some(toast) = self.status_toast(cx) {
            main = main.child(toast);
        }
        div()
            .size_full()
            .key_context("Xylo")
            .on_action(cx.listener(|view, _: &SaveCurrentFile, window, cx| {
                if view.page == Page::Files
                    || (view.page == Page::Chat
                        && view.settings.ui.work_panel_open
                        && view.settings.ui.work_panel == "files")
                {
                    view.request_editor_save(false, window, cx);
                } else if view.page == Page::Artifacts {
                    view.document_save_copy(cx);
                }
            }))
            .on_action(cx.listener(|view, _: &NewConversation, _, cx| view.new_chat(cx)))
            .on_action(cx.listener(|view, _: &ToggleProjectTerminal, window, cx| {
                view.toggle_terminal(window, cx)
            }))
            .on_action(cx.listener(|view, _: &FindConversation, window, cx| {
                view.open_history_search(None, window, cx);
            }))
            .on_action(cx.listener(|view, _: &FindProject, window, cx| {
                view.open_project_search(window, cx);
            }))
            .on_action(cx.listener(|view, _: &AllowPendingAction, window, cx| {
                view.allow_pending(window, cx)
            }))
            .on_action(cx.listener(|view, _: &DenyOrStop, _, cx| view.deny_or_stop(cx)))
            .on_action(
                cx.listener(|view, _: &EscapePressed, window, cx| view.escape_pressed(window, cx)),
            )
            .on_action(cx.listener(|view, _: &FocusComposer, window, cx| {
                view.navigate(Page::Chat, cx);
                view.composer
                    .update(cx, |input, cx| input.focus(window, cx));
                cx.notify();
            }))
            .flex()
            .overflow_hidden()
            .bg(rgb(bg()))
            .text_color(rgb(text()))
            .text_size(px(14.))
            // Theme resolves the installed platform family once. Replacing it
            // with .SystemUIFont makes every text run repeat failed font lookup.
            .font_family(gpui_kit::component::Theme::global(cx).font_family.clone())
            .on_drag_move(
                cx.listener(|view, event: &DragMoveEvent<ResizePanel>, _, cx| {
                    match event.drag(cx).0 {
                        Panel::Sidebar => {
                            view.settings.ui.sidebar_width =
                                f32::from(event.event.position.x - event.bounds.origin.x)
                                    .clamp(200., 400.)
                        }
                        Panel::Files => {
                            let total = f32::from(event.bounds.size.width);
                            let work_width = view.effective_work_panel_width(total);
                            let left = total - work_width;
                            view.settings.ui.files_width =
                                (f32::from(event.event.position.x - event.bounds.origin.x) - left)
                                    .clamp(130., (work_width * 0.45).clamp(130., 480.));
                        }
                        Panel::Work => {
                            view.settings.ui.work_panel_width =
                                (f32::from(event.bounds.size.width)
                                    - f32::from(event.event.position.x - event.bounds.origin.x))
                                .clamp(320., 1200.);
                        }
                    }
                    cx.notify();
                }),
            )
            .on_drop(cx.listener(|view, _: &ResizePanel, _, cx| view.persist_ui(cx)))
            .when(
                self.settings.ui.sidebar_visible || self.settings_open,
                |layout| layout.child(self.sidebar(cx)),
            )
            .child(main)
            .when(self.history_search.is_open(), |layout| {
                layout
                    .relative()
                    .child(self.history_search_palette(window, cx))
            })
            .into_any_element()
    }
}
/// Keeps a project's terminal for when the project is opened again. A
/// terminal only exists while a project is open, so without a root there is
/// nothing to park.
fn park_terminal<T>(cache: &mut BTreeMap<PathBuf, T>, root: Option<PathBuf>, terminal: Option<T>) {
    if let (Some(root), Some(terminal)) = (root, terminal) {
        cache.insert(root, terminal);
    }
}
fn language(path: &Path) -> &'static str {
    match path.extension().and_then(|x| x.to_str()).unwrap_or("") {
        "rs" => "rust",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" => "cpp",
        "py" => "python",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" => "javascript",
        "md" => "markdown",
        _ => "plain_text",
    }
}
fn truncate(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let result: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        result + "\n… (truncated for display)"
    } else {
        result
    }
}
#[path = "xylo/history.rs"]
mod history;
use history::ChatHistory;

#[cfg(test)]
#[path = "xylo/tests/mod.rs"]
mod tests;
