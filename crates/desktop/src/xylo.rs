#[path = "xylo/environments.rs"]
mod environments;
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
#[path = "xylo/chat.rs"]
mod chat;
#[path = "xylo/connectors.rs"]
mod connectors;
#[path = "xylo/diagnostics.rs"]
mod diagnostics;
#[path = "xylo/memory.rs"]
mod memory;
#[path = "xylo/memory_form.rs"]
mod memory_form;
#[path = "xylo/memory_versions.rs"]
mod memory_versions;
#[path = "xylo/native_tools.rs"]
mod native_tools;
#[path = "xylo/settings.rs"]
mod settings;
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
    Agent,
    Permissions,
    Skills,
    Context,
    Files,
    Search,
    HistorySearch,
    Drafts,
    Voice,
    Images,
    Updates,
    Notifications,
    Changes,
    Artifacts,
    Memory,
    Connectors,
    Plugins,
    Environments,
    Providers,
    Diagnostics,
    Schedules,
    Teams,
    Appearance,
    Assets,
}
impl Page {
    fn label(self) -> &'static str {
        match self {
            Self::Assets => "Downloads",
            Self::Appearance => "General",
            Self::Agent => "Agent",
            Self::Permissions => "Permissions",
            Self::Skills => "Skills",
            Self::Context => "Context",
            Self::Chat => "Conversations",
            Self::Files => "Files",
            Self::Search => "Project search",
            Self::HistorySearch => "Search conversations",
            Self::Drafts => "Saved drafts",
            Self::Voice => "Voice",
            Self::Images => "Image generation",
            Self::Updates => "Updates",
            Self::Notifications => "Notifications",
            Self::Changes => "Changes",
            Self::Artifacts => "Artifacts",
            Self::Memory => "Memory",
            Self::Connectors => "Connectors",
            Self::Plugins => "Plugins",
            Self::Environments => "Environments",
            Self::Providers => "Providers",
            Self::Diagnostics => "Activity",
            Self::Schedules => "Scheduled Tasks",
            Self::Teams => "Teams",
        }
    }
}
impl Page {
    fn key(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Files => "files",
            Self::Search => "search",
            Self::HistorySearch => "history-search",
            Self::Drafts => "drafts",
            Self::Voice => "voice",
            Self::Images => "images",
            Self::Updates => "updates",
            Self::Notifications => "notifications",
            Self::Changes => "changes",
            Self::Artifacts => "artifacts",
            Self::Memory => "memory",
            Self::Connectors => "connectors",
            Self::Plugins => "plugins",
            Self::Environments => "environments",
            Self::Providers => "providers",
            Self::Diagnostics => "activity",
            Self::Schedules => "schedules",
            Self::Teams => "teams",
            Self::Assets => "downloads",
            Self::Appearance => "appearance",
            Self::Agent => "agent",
            Self::Permissions => "permissions",
            Self::Skills => "skills",
            Self::Context => "context",
        }
    }
    fn from_key(key: &str) -> Self {
        match key {
            "agent" => Self::Agent,
            "permissions" => Self::Permissions,
            "skills" => Self::Skills,
            "context" => Self::Context,
            "files" => Self::Files,
            "search" => Self::Search,
            "history-search" => Self::HistorySearch,
            "drafts" => Self::Chat,
            "voice" => Self::Chat,
            "images" => Self::Chat,
            "updates" => Self::Chat,
            "notifications" => Self::Chat,
            "changes" => Self::Changes,
            "artifacts" => Self::Chat,
            "memory" => Self::Memory,
            "connectors" => Self::Connectors,
            "plugins" => Self::Plugins,
            "environments" => Self::Environments,
            "providers" => Self::Providers,
            "activity" => Self::Agent,
            "schedules" => Self::Chat,
            "teams" => Self::Chat,
            "downloads" => Self::Assets,
            "appearance" => Self::Appearance,
            _ => Self::Chat,
        }
    }
}

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
    diff: String,
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
    provider_editor_kind: String,
    endpoint: Entity<InputState>,
    model: Entity<InputState>,
    secret: Entity<InputState>,
    context: Entity<InputState>,
    output: Entity<InputState>,
    models: Vec<String>,
    model_context_limits: std::collections::BTreeMap<String, u32>,
    memory_query: Entity<InputState>,
    memory_content: Entity<TextareaState>,
    memories: Vec<MemoryRecord>,
    facts: memory::MemoryUi,
    connector_name: Entity<InputState>,
    connector_endpoint: Entity<InputState>,
    connector_command: Entity<InputState>,
    connector_args: Entity<InputState>,
    connector_transport: String,
    connectors: Vec<ConnectorConfig>,
    connector_tools: Vec<ToolDefinition>,
    connector_inspected: Option<Uuid>,
    connector_loading: bool,
    connector_error: Option<String>,
    status: String,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
    histories: BTreeMap<Uuid, ChatHistory>,
    file_scroll: UniformListScrollHandle,
    diff_scroll: UniformListScrollHandle,
    model_scroll: UniformListScrollHandle,
    conversation_scroll: UniformListScrollHandle,
    diff_lines: Vec<String>,
    settings_open: bool,
    settings_loaded: bool,
    provider_form_open: bool,
    search_open: bool,
    conversation_query: Entity<InputState>,
    selected_assistant: usize,
    expanded_tools: BTreeSet<Uuid>,
    raw_tool_details: BTreeSet<Uuid>,
    schedules: Vec<xylo_runtime::scheduler::ScheduledTask>,
    team_tasks: Vec<TeamTask>,
    schedule_prompt: Entity<TextareaState>,
    schedule_time: Entity<InputState>,
    schedule_frequency: usize,
    quitting: bool,
    quit_prompt_open: bool,
    quit_after_save: Option<PathBuf>,
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
    attachments: AttachmentUi,
    image_generation: ImageGenerationUi,
    notifications: NotificationUi,
    plugins: plugins::PluginUi,
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
                .placeholder("Describe a change…")
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
        let conversation_query = input("Search conversations", window, cx);
        let query_subscription =
            cx.subscribe(&conversation_query, |view, _, event: &InputEvent, cx| {
                view.sidebar_state.dirty = true;
                match event {
                    InputEvent::Change => view.history_search_changed(cx),
                    InputEvent::PressEnter { .. } => view.history_search_submit(cx),
                    _ => {}
                }
                cx.notify();
            });
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
        subscriptions.push(cx.observe_window_activation(window, |view, window, _| {
            let _ = view
                .host
                .as_ref()
                .map(|host| host.foreground(window.is_window_active()));
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
                })
                .detach();
                (Some(client), None)
            }
            Err(error) => (None, Some(error.to_string())),
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
            diff: String::new(),
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
            provider_editor_kind: String::new(),
            endpoint: input("https://provider.example/v1", window, cx),
            model: input("Model ID", window, cx),
            secret,
            context: input("Context tokens", window, cx),
            output: input("Output tokens", window, cx),
            models: vec![],
            model_context_limits: Default::default(),
            memory_query: input("Search project memory", window, cx),
            memory_content,
            memories: vec![],
            facts: memory::MemoryUi::default(),
            connector_name: input("Connector name", window, cx),
            connector_endpoint: input("https://…/mcp", window, cx),
            connector_command: input("Executable path", window, cx),
            connector_args: input("Arguments (one per line)", window, cx),
            connector_transport: "stdio".into(),
            connectors: vec![],
            connector_tools: vec![],
            connector_inspected: None,
            connector_loading: false,
            connector_error: None,
            status: "Connecting…".into(),
            error,
            _subscriptions: subscriptions,
            histories: BTreeMap::new(),
            file_scroll: UniformListScrollHandle::new(),
            diff_scroll: UniformListScrollHandle::new(),
            model_scroll: UniformListScrollHandle::new(),
            conversation_scroll: UniformListScrollHandle::new(),
            diff_lines: vec![],
            settings_open: false,
            settings_loaded: false,
            provider_form_open: false,
            search_open: false,
            conversation_query,
            selected_assistant: 0,
            expanded_tools: BTreeSet::new(),
            raw_tool_details: BTreeSet::new(),
            schedules: vec![],
            team_tasks: vec![],
            schedule_prompt: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .placeholder("What should your assistant do?")
                    .rows(3)
            }),
            schedule_time: input("09:00", window, cx),
            schedule_frequency: 0,
            quitting: false,
            quit_prompt_open: false,
            quit_after_save: None,
            native: NativeUi::new(window, cx),
            documents: documents::DocumentUi::new(window, cx),
            assets: AssetsUi::default(),
            account: AccountUi::default(),
            speech: SpeechUi::new(window, cx),
            project_search: ProjectSearchUi::new(window, cx),
            learning: LearningUi::new(window, cx),
            updates: UpdateUi::default(),
            history_search: HistorySearchUi::new(window, cx),
            drafts: DraftUi::new(window),
            attachments: AttachmentUi::new(),
            image_generation: ImageGenerationUi::new(window, cx),
            notifications: NotificationUi::default(),
            plugins: plugins::PluginUi::new(window, cx),
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
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn prepare_render_preview(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        cx.notify();
    }
    pub(crate) fn request_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.updates.installing {
            return;
        }
        if self.quitting || self.quit_prompt_open || self.quit_after_save.is_some() {
            return;
        }
        if self.recovery.mutation_busy() {
            self.error = Some("Wait for the observed outcome request, or stop waiting and check its result, before closing.".into());
            cx.notify();
            return;
        }
        if !self.management_check_close(DraftFlushAction::Quit, window, cx) {
            return;
        }
        if !self.editor_dirty {
            self.begin_shutdown(cx);
            return;
        }
        window.activate_window();
        cx.activate(true);
        self.quit_prompt_open = true;
        let filename = self
            .editor_path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "your file".into());
        let detail = format!("Your changes to {filename} haven't been saved.");
        let answer = window.prompt(
            PromptLevel::Warning,
            "Save changes before quitting?",
            Some(&detail),
            &[
                PromptButton::ok("Save"),
                PromptButton::new("Discard"),
                PromptButton::cancel("Cancel"),
            ],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let answer = answer.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.quit_prompt_open = false;
                match answer {
                    Ok(0) => this.request_editor_save(true, window, cx),
                    Ok(1) => this.begin_shutdown(cx),
                    _ => cx.notify(),
                }
            });
        })
        .detach();
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
    fn begin_shutdown(&mut self, cx: &mut Context<Self>) {
        if self.management_busy() || self.management_dirty() {
            self.error = Some(
                "Finish or discard your plugin and environment setup before closing Xylo.".into(),
            );
            cx.notify();
            return;
        }
        if !self.flush_drafts(DraftFlushAction::Quit, cx)
            || !self.memory_forms_flush(DraftFlushAction::Quit, cx)
            || !self.document_drafts_flush(DraftFlushAction::Quit, cx)
            || !self.recovery_flush(DraftFlushAction::Quit, cx)
        {
            return;
        }
        for (_, (terminal, _)) in std::mem::take(&mut self.terminal_cache) {
            terminal.update(cx, |terminal, cx| terminal.close_all(cx));
        }
        self.stop_speech(cx);
        self.cancel_project_search(cx);
        self.stop_asset_poll();
        self.detach_editor_assist();
        if self.quitting {
            return;
        }
        self.quitting = true;
        if self.host.as_ref().is_none_or(|h| h.shutdown().is_err()) {
            cx.quit();
            return;
        }
        self.status = "Finishing your tasks…".into();
        let deadline = cx
            .background_executor()
            .timer(std::time::Duration::from_secs(30));
        cx.spawn(async move |_, cx| {
            deadline.await;
            cx.update(|cx| cx.quit());
        })
        .detach();
        cx.notify();
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
    fn provider_from_form(&self, cx: &App) -> anyhow::Result<ProviderConfig> {
        let mut provider = self.provider_editor_source.clone().unwrap_or_default();
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
            HostEvent::HistoryIndexReady => self.history_index_ready(cx),
            HostEvent::WorkspaceOpened { requested, root } => {
                if self.workspace.as_ref() == Some(&requested) {
                    if !self.draft_before_switch(cx) {
                        return;
                    }
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
                }
            }
            HostEvent::WorkspaceIndexed(index) => self.workspace_indexed(index, cx),
            HostEvent::WorkspaceFiles { request_id, result } => {
                self.file_page_result(request_id, result, cx)
            }
            HostEvent::WorkspaceUnavailable { root, message } => {
                if self.workspace.as_ref() == Some(&root) {
                    self.error = Some(message);
                }
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
            HostEvent::TeamTasks(tasks) => self.team_tasks = tasks,
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
                    self.page = Page::from_key(&self.settings.ui.active_page);
                    self.settings_open = matches!(
                        self.page,
                        Page::Providers
                            | Page::Agent
                            | Page::Permissions
                            | Page::Skills
                            | Page::Context
                            | Page::Memory
                            | Page::Connectors
                            | Page::Plugins
                            | Page::Environments
                            | Page::Diagnostics
                            | Page::Appearance
                            | Page::Assets
                            | Page::Voice
                            | Page::Images
                            | Page::Updates
                            | Page::Notifications
                    );
                    if self.page == Page::Memory {
                        self.search_memory(cx);
                    }
                    if self.page == Page::Images {
                        self.load_image_generation_provider(cx);
                    }
                    if self.page == Page::Updates {
                        self.refresh_updates(cx);
                    }
                    if self.page == Page::Notifications {
                        self.refresh_notifications(cx);
                    }
                    if self.page == Page::Plugins {
                        self.refresh_plugins(cx);
                    }
                    if self.page == Page::Environments {
                        self.refresh_environments(cx);
                    }
                    if self.settings.ui.terminal_visible && !self.terminal_visible {
                        self.toggle_terminal(window, cx);
                    }
                }
                if account_corrected {
                    let corrected = self.settings.clone();
                    self.host_call(cx, |host| host.save_settings(corrected));
                }
            }
            HostEvent::ConversationPage(page) => self.conversation_page(page, cx),
            HostEvent::Conversations(conversations) => {
                self.conversations = conversations;
                self.sidebar_state.dirty = true;
            }
            HostEvent::History {
                session_id,
                messages,
            } => {
                if session_id == self.session {
                    self.messages = messages;
                }
            }
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
                self.native.select_local_model=false;
                if let Err(error)=self.register_discovered_models(&provider,&models,&context_limits,&reasoning_options,&default_reasoning_efforts,window,cx) {
                    self.error=Some(error.to_string());
                }
                self.model_context_limits = context_limits;
                self.status = format!("Discovered {} models", models.len());
                self.models = models;
            }
            HostEvent::File { path, content } => {
                if self.editor_dirty {
                    self.error = Some("Save your changes before opening another file.".into());
                    cx.notify();
                    return;
                }
                let language = language(&path);
                self.editor = cx.new(|cx| {
                    EditorState::new(window, cx)
                        .language(language)
                        .default_value(content)
                });
                self.editor_subscription =
                    Some(cx.subscribe(&self.editor, |this, _, event, cx| {
                        if matches!(event, InputEvent::Change) {
                            this.editor_changed(cx);
                        }
                    }));
                self.attach_editor_assist(path.clone(), cx);
                self.editor_path = Some(path);
                self.editor_dirty = false;
                self.page = Page::Files;
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
                self.status = format!("Saved {}", path.display());
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
            HostEvent::Diff(diff) => {
                self.diff_lines = diff.lines().map(str::to_owned).collect();
                self.diff = diff;
            }
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
            HostEvent::ConnectorTools {
                connector_id,
                tools,
            } => {
                if self.connector_inspected == Some(connector_id) {
                    self.connector_loading = false;
                    self.connector_error = None;
                    self.connector_tools = tools;
                }
            }
            HostEvent::ConnectorToolsFailed {
                connector_id,
                message,
            } => {
                if self.connector_inspected == Some(connector_id) {
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
                self.status = status;
            }
            HostEvent::Engine(envelope) => {
                let session = envelope.session_id.unwrap_or(self.session);
                let event = envelope.payload;
                match &event {
                    EngineEvent::Ready { .. } => {
                        self.status = "Ready".into();
                    }
                    EngineEvent::ToolFinished { result, .. } if session == self.session => {
                        if let Ok(value) = serde_json::from_str::<serde_json::Value>(result)
                            && let Some(paths) = value.get("artifacts").and_then(|v| v.as_array())
                        {
                            for path in paths.iter().filter_map(|v| v.as_str()) {
                                let path = PathBuf::from(path);
                                self.record_artifact(path);
                            }
                        }
                    }
                    EngineEvent::Completed { text, .. } => {
                        if session != self.session
                            && let Some(history) = self.histories.get_mut(&session)
                        {
                            history.stale = true;
                        }
                        if session == self.session {
                            self.messages
                                .push(message(session, "assistant", text.clone()));
                        }
                        let _ = self.host.as_ref().map(|h| h.list_conversations());
                    }
                    EngineEvent::Error { message, .. } if session == self.session => {
                        self.error = Some(message.clone());
                    }
                    _ => {}
                }
                self.log(match &event {
                    EngineEvent::Delta { .. } => String::new(),
                    EngineEvent::ToolStarted { name, .. } => format!("Tool started: {name}"),
                    EngineEvent::ToolFinished { name, success, .. } => {
                        format!("Tool finished: {name}, success={success}")
                    }
                    EngineEvent::ApprovalRequired { name, .. } => {
                        format!("Approval required: {name}")
                    }
                    other => format!("{other:?}"),
                });
                let review_changed = matches!(
                    &event,
                    EngineEvent::Started
                        | EngineEvent::Completed { .. }
                        | EngineEvent::Cancelled
                        | EngineEvent::Error { .. }
                        | EngineEvent::ToolFinished { .. }
                );
                self.turns.entry(session).or_default().apply(event);
                if session == self.session && review_changed {
                    self.recovery_activate(window, cx);
                    self.refresh_action_reviews(cx);
                }
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
        if self.attachments.importing()
            || self.draft_flush_pending()
            || self.memory_forms_flush_pending()
        {
            return;
        }
        if self.running() {
            self.submit_persisted_steering(window, cx);
            return;
        }
        self.submit_persisted_draft(window, cx);
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
        }
        self.result(result, cx);
    }
    fn conversation_page(&mut self, page: ConversationPage, cx: &mut Context<Self>) {
        let advance = self.conversations_loading_more || self.conversations.is_empty();
        let mut merged: BTreeMap<_, _> = self
            .conversations
            .drain(..)
            .map(|conversation| (conversation.id, conversation))
            .collect();
        for conversation in page.conversations {
            merged.insert(conversation.id, conversation);
        }
        self.conversations = merged.into_values().collect();
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
        self.sidebar_state.dirty = true;
        self.settings.ui.work_panel_open = false;
        self.page = Page::Chat;
        self.settings_open = false;
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
        if !self.management_before_switch(cx) {
            return;
        }
        if self.editor_dirty {
            self.error = Some("Save your editor changes before switching projects.".into());
            cx.notify();
            return;
        }
        if !self.draft_before_switch(cx)
            || !self.memory_before_switch(cx)
            || !self.document_before_workspace_switch(cx)
        {
            return;
        }
        let previous_root = self.workspace.clone();
        let restoring =
            previous_root.is_none() && self.settings.last_workspace.as_ref() == Some(&path);
        self.remember_work_panel();
        let panel_state = self
            .settings
            .ui
            .project_panels
            .get(&path)
            .cloned()
            .unwrap_or_else(|| {
                if restoring {
                    WorkspacePanelPreferences {
                        open: self.settings.ui.work_panel_open,
                        panel: self.settings.ui.work_panel.clone(),
                        width: self.settings.ui.work_panel_width,
                    }
                } else {
                    WorkspacePanelPreferences::default()
                }
            });
        self.new_chat(cx);
        self.settings.ui.work_panel_open = panel_state.open;
        self.settings.ui.work_panel = panel_state.panel;
        self.settings.ui.work_panel_width = panel_state.width;
        self.stop_speech(cx);
        self.cancel_project_search(cx);
        self.project_search.reset();
        self.history_search.reset();
        self.learning.clear();
        self.management_scope_changed(cx);
        self.workspace = Some(path.clone());
        self.workspace_ready = false;
        self.memory_after_switch(window, cx);
        self.document_after_workspace_switch(window, cx);
        self.draft_switch_deferred(cx);
        self.sidebar_state.dirty = true;
        self.settings.last_workspace = Some(path.clone());
        self.reset_file_browser();
        self.diff.clear();
        self.diff_lines.clear();
        self.git_panel = Default::default();
        self.memories.clear();
        self.detach_editor_assist();
        self.editor_path = None;
        self.editor_dirty = false;
        let subscription = self.terminal_subscription.take();
        if let Some(terminal) = self.terminal.take()
            && let Some(root) = previous_root
        {
            self.terminal_cache.insert(root, (terminal, subscription));
        }
        if let Some((terminal, subscription)) = self.terminal_cache.remove(&path) {
            self.terminal = Some(terminal);
            self.terminal_subscription = subscription;
        }
        self.terminal_visible = self.settings.ui.work_panel_open
            && self.settings.ui.work_panel == "terminal"
            && self.terminal.is_some();
        self.terminal_starting = false;
        self.host_call(cx, |h| h.open_workspace(path.clone()));
    }
    fn refresh_files(&mut self, cx: &mut Context<Self>) {
        if let Some(host) = &self.host {
            let result = host.refresh_workspace();
            self.result(result, cx);
            self.load_file_page(None, cx);
        }
    }
    fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.terminal_visible = !self.terminal_visible;
        if self.terminal_visible {
            self.settings.ui.work_panel_open = true;
            self.settings.ui.work_panel = "terminal".into();
            self.settings_open = false;
            self.page = Page::Chat;
        } else if self.settings.ui.work_panel == "terminal" {
            self.settings.ui.work_panel_open = false;
        }
        self.persist_ui(cx);
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
                                            this.terminal_visible = false;
                                            this.persist_ui(cx);
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
        if matches!(
            page,
            Page::Providers
                | Page::Agent
                | Page::Permissions
                | Page::Skills
                | Page::Context
                | Page::Memory
                | Page::Connectors
                | Page::Appearance
                | Page::Assets
                | Page::Voice
                | Page::Images
                | Page::Updates
                | Page::Notifications
                | Page::Plugins
                | Page::Environments
        ) {
            self.settings_open = true;
        }
        self.persist_ui(cx);
        match page {
            Page::Updates => self.refresh_updates(cx),
            Page::Notifications => self.refresh_notifications(cx),
            Page::Plugins => self.refresh_plugins(cx),
            Page::Environments => self.refresh_environments(cx),
            Page::Images => self.load_image_generation_provider(cx),
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
            Page::Connectors | Page::Context => self.host_call(cx, |h| h.connectors()),
            Page::Skills => self.refresh_learning(cx),
            _ => {}
        }
        cx.notify();
    }
    fn scope(&self) -> String {
        self.workspace
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "global".into())
    }
    fn save_provider(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let result = (|| -> anyhow::Result<()> {
            let mut provider = self.provider_from_form(cx)?;
            if let Some(limit) = self.model_context_limits.get(&provider.model) {
                provider.context_tokens = *limit;
                provider.output_tokens = provider.output_tokens.min(*limit / 4);
            }
            provider.validate_connection()?;
            let key = self.secret.read(cx).value().to_string();
            if matches!(
                self.provider_editor_kind.as_str(),
                "OpenAI" | "Anthropic" | "Gemini"
            ) && provider.credential_ref.is_none()
                && key.trim().is_empty()
            {
                anyhow::bail!("Enter an API key for this provider");
            }
            if !key.is_empty() {
                let reference = provider
                    .credential_ref
                    .clone()
                    .unwrap_or_else(|| format!("provider:{}", Uuid::new_v4()));
                provider.credential_ref = Some(reference);
            }
            let mut settings = self.settings.clone();
            if let Some(existing)=settings.providers.iter_mut().find(|saved|providers::same_connection(saved,&provider) && saved.model==provider.model) { *existing=provider.clone(); } else {settings.providers.push(provider.clone());}
            self.dispatch_provider_save(settings.clone(), provider.clone(), (!key.is_empty()).then_some(key))?;
            self.settings = settings;
            let kind = self.provider_editor_kind.clone();
            self.provider_form(&provider, window, cx);
            self.provider_editor_kind = kind;
            Ok(())
        })();
        if result.is_ok() {
            Self::set_input(&self.secret, "", window, cx);
            self.models.clear();
            self.status = "Saving provider and discovering models…".into();
        }
        self.result(result, cx);
    }
    fn preset(
        &mut self,
        name: &str,
        endpoint: &str,
        family: ApiFamily,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let p = ProviderConfig {
            name: name.into(),
            endpoint: endpoint.into(),
            family,
            ..Default::default()
        };
        self.provider_form(&p, window, cx);
        self.models.clear();
        self.model_context_limits.clear();
        cx.notify();
    }
    fn save_connector(&mut self, cx: &mut Context<Self>) {
        let name = self.connector_name.read(cx).value().trim().to_string();
        if name.is_empty() {
            self.error = Some("A connector needs a name.".into());
            cx.notify();
            return;
        }
        let args = self.connector_args.read(cx).value().to_string();
        let args: Result<Vec<String>, std::convert::Infallible> = Ok(args
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect());
        match args {
            Ok(args) => {
                let command = self.connector_command.read(cx).value().trim().to_string();
                let config = ConnectorConfig {
                    id: Uuid::new_v4(),
                    name,
                    transport: self.connector_transport.clone(),
                    endpoint: self.connector_endpoint.read(cx).value().trim().into(),
                    command: (!command.is_empty()).then(|| PathBuf::from(command)),
                    args,
                    credential_ref: None,
                };
                self.host_call(cx, |h| h.save_connector(config));
            }
            Err(error) => {
                self.error = Some(error.to_string());
                cx.notify();
            }
        }
    }
}
impl Render for Xylo {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.updates.installing {
            return div()
                .size_full()
                .bg(rgb(bg()))
                .text_color(rgb(text()))
                .p_8()
                .child(heading(
                    "Installing update…",
                    "Keep Xylo open until installation finishes.",
                ))
                .into_any_element();
        }
        let body = match self.page {
            Page::Assets => self.assets_page(cx),
            Page::Appearance => self.general_settings_page(cx),
            Page::Agent => self.agent_settings_page(cx),
            Page::Permissions => self.permission_settings_page(cx),
            Page::Skills => self.skills_settings_page(cx),
            Page::Context => self.context_settings_page(cx),
            Page::Chat | Page::Files | Page::Changes => self.native_workspace(window, cx),
            Page::Search => self.project_search_page(cx),
            Page::HistorySearch => self.history_search_page(cx),
            Page::Drafts => self.drafts_page(cx),
            Page::Updates => self.updates_page(cx),
            Page::Notifications => self.notifications_page(cx),
            Page::Images => self.image_provider_settings_page(cx),
            Page::Voice => div()
                .id("voice-page")
                .size_full()
                .overflow_y_scroll()
                .p_6()
                .child(self.speech_card(cx))
                .into_any_element(),
            Page::Artifacts => self.artifacts_page(cx),
            Page::Providers => self.providers_page(cx),
            Page::Memory => self.memory_page(cx),
            Page::Connectors => self.connections_settings_page(cx),
            Page::Plugins => self.plugins_page(cx),
            Page::Environments => self.environments_page(cx),
            Page::Diagnostics => self.diagnostics_page(cx),
            Page::Schedules => self.schedules_page(cx),
            Page::Teams => self.teams_page(cx),
        };
        let title = if self.settings_open {
            self.page.label().to_owned()
        } else {
            self.conversations
                .iter()
                .find(|chat| chat.id == self.session)
                .map(|chat| chat.title.clone())
                .unwrap_or_else(|| "New chat".into())
        };
        let mut main = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(rgb(panel()))
            .flex()
            .flex_col();
        let mut toolbar = div()
            .h(px(49.))
            .flex_shrink_0()
            .px(px(17.))
            .flex()
            .items_center()
            .gap_2()
            .border_b_1()
            .border_color(rgb(border()));
        if !self.settings_open {
            toolbar = toolbar.child(
                icon_button(
                    "toggle-sidebar",
                    "Toggle sidebar",
                    gpui_kit::component::IconName::PanelLeft,
                )
                .on_click(cx.listener(|view, _, _, cx| {
                    view.settings.ui.sidebar_visible = !view.settings.ui.sidebar_visible;
                    view.persist_ui(cx);
                    cx.notify();
                })),
            );
        }
        toolbar = toolbar.child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(12.))
                .font_weight(FontWeight::MEDIUM)
                .child(title),
        );
        if !self.settings_open {
            toolbar = toolbar.child(self.conversation_actions(cx));
            toolbar = toolbar.child(
                icon_button(
                    "toggle-work-panel",
                    "Toggle work panel",
                    gpui_kit::component::IconName::PanelRight,
                )
                .on_click(cx.listener(|view, _, window, cx| {
                    let was_open = view.settings.ui.work_panel_open
                        || matches!(view.page, Page::Files | Page::Changes);
                    if view.page == Page::Files {
                        view.settings.ui.work_panel = "files".into();
                    }
                    if view.page == Page::Changes {
                        view.settings.ui.work_panel = "changes".into();
                    }
                    view.page = Page::Chat;
                    view.settings.ui.work_panel_open = !was_open;
                    if view.settings.ui.work_panel_open {
                        view.activate_work_panel(window, cx);
                    }
                    view.persist_ui(cx);
                    cx.notify();
                })),
            );
        }
        main = main.child(toolbar);
        if let Some(error) = &self.error {
            main = main.child(
                div()
                    .px_5()
                    .py_2()
                    .bg(rgb(panel()))
                    .border_b_1()
                    .border_color(rgb(border()))
                    .text_color(rgb(muted()))
                    .text_sm()
                    .flex()
                    .justify_between()
                    .gap_3()
                    .child(div().flex_1().min_w_0().child(product_error(error)))
                    .child(button("dismiss-error", "Dismiss").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.error = None;
                            cx.notify();
                        },
                    ))),
            );
        }
        if self.draft_flush_pending()
            || self.memory_forms_flush_pending()
            || self.document_drafts_flush_pending()
            || self.recovery_flush_pending()
        {
            main =
                main.child(
                    div()
                        .px_5()
                        .py_2()
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(subtitle("Saving your work before closing…"))
                        .child(
                            button("pending-save-keep-working", "Keep working").on_click(
                                cx.listener(|view, _, _, cx| {
                                    view.cancel_draft_flush(cx);
                                    view.cancel_memory_forms_flush(cx);
                                    view.cancel_document_drafts_flush(cx);
                                    view.cancel_recovery_flush(cx);
                                }),
                            ),
                        )
                        .child(button("pending-save-memory", "Review Memory").on_click(
                            cx.listener(|view, _, _, cx| view.navigate(Page::Memory, cx)),
                        ))
                        .child(
                            button("pending-save-documents", "Review documents").on_click(
                                cx.listener(|view, _, _, cx| view.navigate(Page::Artifacts, cx)),
                            ),
                        ),
                );
        }
        main = main.child(div().flex_1().min_h_0().child(body));
        if self.terminal_visible
            && !matches!(self.page, Page::Chat | Page::Files | Page::Changes)
            && !self.settings_open
        {
            main = main.child(
                div()
                    .h(px(self.settings.ui.terminal_height))
                    .flex_shrink_0()
                    .relative()
                    .child(self.resize_edge(Panel::Terminal))
                    .border_t_1()
                    .border_color(rgb(border()))
                    .child(if let Some(terminal) = &self.terminal {
                        terminal.clone().into_any_element()
                    } else {
                        div()
                            .p_4()
                            .child(subtitle(if self.terminal_starting {
                                "Starting your shell…"
                            } else {
                                "Terminal unavailable. See the error above."
                            }))
                            .into_any_element()
                    }),
            );
        }
        let _ = window;
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
                view.search_open = true;
                view.page = Page::HistorySearch;
                view.settings_open = false;
                view.conversation_query
                    .update(cx, |input, cx| input.focus(window, cx));
                cx.notify();
            }))
            .on_action(cx.listener(|view, _: &FindProject, window, cx| {
                view.open_project_search(window, cx);
            }))
            .on_action(cx.listener(|view, _: &AllowPendingAction, _, cx| view.allow_pending(cx)))
            .on_action(cx.listener(|view, _: &DenyOrStop, _, cx| view.deny_or_stop(cx)))
            .on_action(cx.listener(|view, _: &FocusComposer, window, cx| {
                view.page = Page::Chat;
                view.composer
                    .update(cx, |input, cx| input.focus(window, cx));
                cx.notify();
            }))
            .flex()
            .overflow_hidden()
            .bg(rgb(bg()))
            .text_color(rgb(text()))
            .text_size(px(13.))
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
                            let side = if view.settings.ui.sidebar_visible {
                                view.settings.ui.sidebar_width
                            } else {
                                0.
                            };
                            let work_width = view
                                .settings
                                .ui
                                .work_panel_width
                                .min((total - side).max(0.) * 0.62)
                                .max(320.);
                            let left = total - work_width;
                            view.settings.ui.files_width =
                                (f32::from(event.event.position.x - event.bounds.origin.x) - left)
                                    .clamp(130., (work_width * 0.6).max(130.).min(480.));
                        }
                        Panel::Work => {
                            view.settings.ui.work_panel_width =
                                (f32::from(event.bounds.size.width)
                                    - f32::from(event.event.position.x - event.bounds.origin.x))
                                .clamp(320., 1200.);
                        }
                        Panel::Terminal => {
                            view.settings.ui.terminal_height =
                                f32::from(event.bounds.bottom() - event.event.position.y)
                                    .clamp(150., 520.)
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
            .into_any_element()
    }
}
/// Keep setup and task errors actionable; detailed service errors stay in logs.
fn product_error(error: &str) -> String {
    let lower = error.to_lowercase();
    if lower.contains("http 401") {
        "The provider rejected your credentials. Check the access token and any additional authentication required by your provider, then retry.".into()
    } else if lower.contains("context budget exceeded") {
        "This request still exceeds the model's context window after compaction. Reduce large attachments or choose a model with a larger context window.".into()
    } else if lower.contains("sha256") || lower.contains("checksum") {
        "The download could not be verified. Please retry.".into()
    } else if lower.contains("clang") || lower.contains("cmake") || lower.contains("cargo build") {
        "This feature could not be prepared on your computer. Try another available package.".into()
    } else if lower.contains("already running") {
        "Xylo is already open. Switch to the existing window.".into()
    } else if lower.contains("worker")
        || lower.contains("host stopped")
        || lower.contains("protocol")
        || lower.contains("hermes")
    {
        "The task service could not start. Restart Xylo and try again.".into()
    } else if lower.contains("helper") {
        "This feature could not start. Check its setup and try again.".into()
    } else if lower.contains("application service unavailable")
        || lower.contains("queue is full")
        || lower.contains("disconnected")
    {
        "Xylo is busy or reconnecting. Please try again.".into()
    } else {
        error
            .lines()
            .next()
            .unwrap_or("Something went wrong. Please try again.")
            .to_string()
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
#[cfg(test)]
mod tests {
    use super::{language, truncate};
    use std::path::Path;
    #[test]
    fn truncation_is_unicode_safe() {
        assert_eq!(truncate("界界界", 2), "界界\n… (truncated for display)");
    }
    #[test]
    fn language_uses_extension() {
        assert_eq!(language(Path::new("src/lib.rs")), "rust");
    }
}
#[path = "xylo/history.rs"]
mod history;
use history::ChatHistory;
