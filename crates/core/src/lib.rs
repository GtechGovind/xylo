//! Application-owned contracts. Community engine types must not cross this boundary.
mod branding;
pub use branding::{APPLICATION_ID, APPLICATION_NAME, CREDENTIAL_SERVICE};
mod documents;
pub use documents::*;
mod plugins;
pub use plugins::*;
mod environments;
pub use environments::*;
mod attachments;
pub use attachments::*;
mod image_generation;
pub use image_generation::*;
mod steering;
pub use steering::*;

use anyhow::{Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub const PROTOCOL_VERSION: u16 = 2;
pub const MAX_PROTOCOL_BYTES: usize = 8 * 1024 * 1024;
pub const EVENT_QUEUE_CAPACITY: usize = 256;

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApiFamily {
    OpenAiChat,
    OpenAiResponses,
    Anthropic,
    Gemini,
    Azure,
    Bedrock,
    Vertex,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    pub family: ApiFamily,
    pub endpoint: String,
    pub model: String,
    pub credential_ref: Option<String>,
    pub context_tokens: u32,
    pub output_tokens: u32,
    pub timeout_seconds: u64,
    /// Explicit model reasoning choice. None leaves the provider default intact.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Catalog-advertised choices for this exact model. Some(empty) means unsupported.
    #[serde(default)]
    pub reasoning_options: Option<Vec<String>>,
    #[serde(default)]
    pub default_reasoning_effort: Option<String>,
    /// Explicit catalog destination; receives this connection's auth and headers.
    #[serde(default)]
    pub discovery_url: Option<String>,
    /// Restart the application-owned local server before a turn. External
    /// endpoints retain their own lifecycle and concurrency policy.
    #[serde(default)]
    pub managed_local: bool,
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            name: "Local model".into(),
            family: ApiFamily::OpenAiChat,
            endpoint: "http://127.0.0.1:8080/v1".into(),
            model: String::new(),
            credential_ref: None,
            context_tokens: 8192,
            output_tokens: 2048,
            timeout_seconds: 120,
            reasoning_effort: None,
            reasoning_options: None,
            default_reasoning_effort: None,
            discovery_url: None,
            managed_local: false,
            headers: Default::default(),
        }
    }
}

/// Catalog effort identifiers are bounded protocol tokens, never arbitrary request data.
pub fn valid_reasoning_option(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value.as_bytes()[0].is_ascii_lowercase()
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

impl ProviderConfig {
    /// Conservative, protocol-aware capabilities. Unrecognized model names and
    /// custom deployment aliases do not imply support for a reasoning control.
    /// Source: OpenAI reasoning guide and GPT-5.2 model documentation.
    pub fn supported_reasoning_efforts(&self) -> Vec<&str> {
        if !matches!(
            self.family,
            ApiFamily::OpenAiChat | ApiFamily::OpenAiResponses | ApiFamily::Azure
        ) {
            return Vec::new();
        }
        if let Some(options) = &self.reasoning_options {
            let mut supported = Vec::new();
            for option in options.iter().take(16) {
                if valid_reasoning_option(option) && !supported.contains(&option.as_str()) {
                    supported.push(option.as_str());
                }
            }
            return supported;
        }
        let model = self.model.to_ascii_lowercase();
        let is = |name: &str| {
            model == name
                || model
                    .strip_prefix(name)
                    .is_some_and(|suffix| suffix.starts_with("-20"))
        };
        let known: &[&str] = if is("gpt-5.2-pro") {
            &["medium", "high", "xhigh"]
        } else if is("gpt-5.2-codex") {
            &["low", "medium", "high", "xhigh"]
        } else if is("gpt-5.2") {
            &["none", "low", "medium", "high", "xhigh"]
        } else if is("gpt-5.1") {
            &["none", "low", "medium", "high"]
        } else if is("gpt-5") || is("gpt-5-mini") || is("gpt-5-nano") {
            &["minimal", "low", "medium", "high"]
        } else if is("o3") || is("o3-mini") || is("o4-mini") {
            &["low", "medium", "high"]
        } else {
            &[]
        };
        known.to_vec()
    }

    /// Unsupported selections are never sent to a model after a model switch.
    pub fn effective_reasoning_effort(&self) -> Option<&str> {
        self.reasoning_effort
            .as_deref()
            .filter(|effort| self.supported_reasoning_efforts().contains(effort))
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.model.trim().is_empty(),
            "Choose a model in Providers before starting a task"
        );
        self.validate_connection()
    }

    /// Validate provider setup before a model has been discovered or selected.
    pub fn validate_connection(&self) -> Result<()> {
        ensure!(
            self.context_tokens >= 1024 && self.output_tokens < self.context_tokens,
            "Output tokens must fit inside the context budget (minimum context: 1024)"
        );
        ensure!(
            self.timeout_seconds > 0 && self.timeout_seconds <= 3600,
            "Invalid provider timeout"
        );
        ensure!(
            self.endpoint.starts_with("https://")
                || self.endpoint.starts_with("http://127.0.0.1:")
                || self.endpoint.starts_with("http://localhost:"),
            "Use HTTPS or a loopback HTTP endpoint"
        );
        Ok(())
    }
}

/// How much the agent may do without asking. The presets mirror Codex.
///
/// The mode stored in settings is the source of truth: the host forwards every
/// change to running workers and the engine reads the current mode at each tool
/// call, so a change applies to the running task immediately.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Sandboxed project commands (no network), memory writes, enabled plugin
    /// hooks and ordinary file edits inside the project run without asking.
    /// Edits to files that run code or hold secrets (auto-run config, CI,
    /// hooks, `.env`), anything outside the project, network access,
    /// connectors, plugin components, desktop and browser control, image
    /// generation, remote environments and schedules ask first.
    #[default]
    AskForApproval,
    /// Ask only for actions the engine assesses as potentially unsafe.
    ApproveForMe,
    /// No approvals and no sandbox: unrestricted file and network access.
    FullAccess,
    /// Read and search only; any write is refused.
    ReadOnly,
}

impl PermissionMode {
    /// Presentation order for menus and settings.
    pub const ALL: [PermissionMode; 4] = [
        PermissionMode::AskForApproval,
        PermissionMode::ApproveForMe,
        PermissionMode::FullAccess,
        PermissionMode::ReadOnly,
    ];

    /// The one product label for this mode.
    pub fn label(self) -> &'static str {
        match self {
            PermissionMode::AskForApproval => "Ask for approval",
            PermissionMode::ApproveForMe => "Approve for me",
            PermissionMode::FullAccess => "Full access",
            PermissionMode::ReadOnly => "Read only",
        }
    }

    /// One-line explanation shown beside the label.
    pub fn description(self) -> &'static str {
        match self {
            PermissionMode::AskForApproval => {
                "Always ask to edit files outside the project and use the internet"
            }
            PermissionMode::ApproveForMe => "Only ask for actions detected as potentially unsafe",
            PermissionMode::FullAccess => {
                "Unrestricted access to the internet and any file on your computer"
            }
            PermissionMode::ReadOnly => "Read and search only; nothing is changed",
        }
    }

    /// Whether the agent may change anything at all.
    pub fn allows_writes(self) -> bool {
        self != PermissionMode::ReadOnly
    }

    /// Whether commands run inside the OS sandbox.
    pub fn sandboxed(self) -> bool {
        self != PermissionMode::FullAccess
    }
}

/// Where Xylo keeps provider keys, tokens and connector secrets.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SecretStorage {
    /// An encrypted vault file whose key is an owner-only file in the
    /// application data directory. No operating-system prompts.
    #[default]
    LocalFile,
    /// The same encrypted vault, with its key held as a single item in the
    /// operating system's credential store.
    OsKeychain,
}

/// A user's standing approval for one kind of action in one project.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApprovalRule {
    /// Stable rule key emitted by the engine (`ApprovalRequired.rule_key`).
    pub key: String,
    /// What the rule allows, in product words.
    pub label: String,
    pub workspace: PathBuf,
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppSettings {
    pub provider: ProviderConfig,
    pub providers: Vec<ProviderConfig>,
    pub permissions: PermissionMode,
    pub theme: String,
    pub last_workspace: Option<PathBuf>,
    pub local_context: u32,
    pub telemetry_export: bool,
    #[serde(default)]
    pub ui: UiPreferences,
    #[serde(default)]
    pub secret_storage: SecretStorage,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspacePanelPreferences {
    pub open: bool,
    pub panel: String,
    pub width: f32,
}
impl Default for WorkspacePanelPreferences {
    fn default() -> Self {
        Self {
            open: false,
            panel: "files".into(),
            width: 640.,
        }
    }
}

/// Product layout only; execution and hardware policy are not UI preferences.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct UiPreferences {
    pub project_panels: std::collections::BTreeMap<PathBuf, WorkspacePanelPreferences>,
    pub collapsed_projects: Vec<PathBuf>,
    pub open_projects: Vec<PathBuf>,
    pub sidebar_visible: bool,
    pub work_panel_width: f32,
    pub work_panel_open: bool,
    pub work_panel: String,
    pub sidebar_width: f32,
    pub files_width: f32,
    pub active_page: String,
    /// Most recently chosen models, newest first, for the model picker.
    pub recent_models: Vec<RecentModel>,
}

/// A model the user picked recently, identified by its connection endpoint.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecentModel {
    pub endpoint: String,
    pub model: String,
}

impl Default for UiPreferences {
    fn default() -> Self {
        Self {
            project_panels: Default::default(),
            collapsed_projects: Vec::new(),
            open_projects: Vec::new(),
            sidebar_visible: true,
            work_panel_width: 640.,
            work_panel_open: false,
            work_panel: "files".into(),
            sidebar_width: 222.0,
            files_width: 240.0,
            active_page: "chat".into(),
            recent_models: Vec::new(),
        }
    }
}

impl AppSettings {
    /// Keep distinct model choices for the same endpoint without duplicates.
    pub fn remember_model(&mut self, provider: ProviderConfig) {
        if provider.model.is_empty() {
            return;
        }
        if let Some(existing) = self.providers.iter_mut().find(|p| {
            p.endpoint == provider.endpoint
                && p.family == provider.family
                && p.model == provider.model
                && p.credential_ref == provider.credential_ref
                && p.headers == provider.headers
                && p.discovery_url == provider.discovery_url
        }) {
            *existing = provider;
        } else {
            self.providers.push(provider);
        }
    }
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            provider: ProviderConfig::default(),
            providers: Vec::new(),
            permissions: PermissionMode::default(),
            theme: "light".into(),
            last_workspace: None,
            local_context: 8192,
            telemetry_export: false,
            ui: UiPreferences::default(),
            secret_storage: SecretStorage::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TurnRequest {
    pub session_id: Uuid,
    pub turn_id: Uuid,
    pub prompt: String,
    pub workspace: PathBuf,
    pub provider: ProviderConfig,
    pub permissions: PermissionMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<ImageAttachment>,
    /// Keys of the project's standing approvals ("always allow" rules).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approved_rules: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub version: u16,
    pub request_id: Uuid,
    pub session_id: Option<Uuid>,
    pub sequence: u64,
    pub payload: T,
}

impl<T> Envelope<T> {
    pub fn new(request_id: Uuid, session_id: Option<Uuid>, sequence: u64, payload: T) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id,
            session_id,
            sequence,
            payload,
        }
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == PROTOCOL_VERSION,
            "Unsupported worker protocol version {}",
            self.version
        );
        Ok(())
    }
}

/// Transient worker-only authentication. Never place this in settings, turns,
/// checkpoints, event payloads, command-line arguments, or environment variables.
#[derive(Clone, Serialize, Deserialize)]
pub struct WorkerCredential(pub Option<String>);
impl std::fmt::Debug for WorkerCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WorkerCredential([REDACTED])")
    }
}

/// Private host-to-worker credential response; never persisted in turn history.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderCredentialUpdate {
    pub generation: u64,
    pub credential: WorkerCredential,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
#[allow(
    clippy::large_enum_variant,
    reason = "A bounded command queue keeps the small turn payload inline and avoids a separate heap allocation"
)]
pub enum WorkerCommand {
    Hello,
    Run {
        turn: TurnRequest,
    },
    RunAuthorized {
        turn: TurnRequest,
        credential: WorkerCredential,
    },
    ProviderCredentialUpdated {
        request_id: Uuid,
        result: Result<ProviderCredentialUpdate, String>,
    },
    Cancel,
    SteerDurable {
        guidance: SteeringRequest,
    },
    Approve {
        action_id: Uuid,
        allow: bool,
        /// Also allow matching actions for the rest of this chat session.
        #[serde(default)]
        remember: bool,
    },
    /// Apply a permission change to the running and all later tool calls.
    SetPermissions {
        mode: PermissionMode,
    },
    /// The secret-vault key for this worker. Sent once, before any `Run`.
    Unlock {
        key: WorkerCredential,
    },
    Shutdown,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Prompt tokens summed over every model call of the turn.
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub elapsed_ms: u64,
    /// Prompt tokens of the turn's last model call: the context actually in use.
    #[serde(default)]
    pub context_tokens: u64,
}

/// What a running turn is doing right now, for the activity line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnPhase {
    /// Admission: resources, credentials, local model start.
    Preparing,
    /// A model request is in flight and nothing has streamed yet.
    Thinking,
    /// The model is streaming text.
    Responding,
    /// Tools from the last response are executing.
    RunningTools,
    /// An action is waiting for the user's decision.
    WaitingForApproval,
    /// Earlier context is being condensed to fit the window.
    Compacting,
    /// Waiting before retrying a failed or rate-limited model request.
    Retrying { attempt: u32, delay_ms: u64 },
    /// A helper agent is working on a delegated task.
    Delegating,
}

/// Token accounting of the context sent with the latest model request.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextUsage {
    /// The configured context window.
    pub capacity: u64,
    /// Tokens reserved for the model's output.
    pub output_reserve: u64,
    /// System prompt, project instructions and retained facts.
    pub system: u64,
    /// Schemas of the tools offered directly.
    pub tools: u64,
    pub tool_count: u32,
    /// Tools discoverable through search; not counted in `tools`.
    pub deferred_tool_count: u32,
    /// Conversation messages sent with the request.
    pub messages: u64,
    /// Condensed summary of earlier work, when compaction happened.
    pub summary: u64,
    pub compacted: bool,
    /// Usage above which earlier context is condensed.
    pub autocompact_threshold: u64,
    /// Prompt tokens the provider reported for the last request, when known.
    #[serde(default)]
    pub provider_input: Option<u64>,
}

impl ContextUsage {
    /// Tokens of the request as estimated before sending.
    pub fn used(&self) -> u64 {
        self.system + self.tools + self.messages + self.summary
    }
}

/// Why a turn ended in an error; decides Retry versus Reconnect in the UI.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Credentials are missing, expired or rejected.
    Authentication,
    /// The provider rate-limited or is overloaded.
    RateLimited,
    /// The provider or network failed.
    Provider,
    /// The task no longer fits the model's context window.
    ContextOverflow,
    /// An application fault.
    #[default]
    Internal,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EngineEvent {
    /// Internal request intercepted by the host, never forwarded to the UI.
    ProviderCredentialRequested {
        request_id: Uuid,
        generation: u64,
        force: bool,
    },
    /// The worker is ready for commands; `version` is the worker's build version.
    Ready {
        version: String,
    },
    Started,
    Delta {
        text: String,
    },
    Status {
        text: String,
    },
    Steering {
        receipt: SteeringReceipt,
    },
    ToolStarted {
        action_id: Uuid,
        name: String,
        arguments: Value,
        /// The `agent_delegate` action that started the child agent issuing this call.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_id: Option<Uuid>,
    },
    ApprovalRequired {
        action_id: Uuid,
        name: String,
        arguments: Value,
        /// What "always allow" would cover in this project, in product words
        /// (for example "`cargo test` commands").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rule: Option<String>,
        /// Stable key of that rule, persisted when the user chooses "always allow".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rule_key: Option<String>,
        /// Why the action needs approval under the current mode.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_id: Option<Uuid>,
    },
    ToolFinished {
        action_id: Uuid,
        name: String,
        result: String,
        success: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_id: Option<Uuid>,
    },
    /// The turn's current activity changed.
    Phase {
        phase: TurnPhase,
    },
    /// Context accounting after the latest request was fitted.
    ContextUsage {
        usage: ContextUsage,
    },
    /// A generated title for the conversation.
    TitleUpdated {
        title: String,
    },
    Completed {
        text: String,
        usage: Usage,
    },
    Cancelled,
    Error {
        message: String,
        retryable: bool,
        #[serde(default)]
        kind: ErrorKind,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Conversation {
    pub id: Uuid,
    pub title: String,
    pub workspace: PathBuf,
    pub updated_at: i64,
    #[serde(default)]
    pub title_source: TitleSource,
    /// Pinned chats are listed first in the sidebar.
    #[serde(default)]
    pub pinned: bool,
    /// Archived chats are kept with their history but left out of the chat
    /// list and chat search.
    #[serde(default)]
    pub archived: bool,
}

/// Where a conversation's title came from. Generated titles never replace a
/// title the user chose.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TitleSource {
    /// The first characters of the first prompt.
    #[default]
    Prompt,
    Generated,
    User,
}

/// Whether a stored assistant reply is complete.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageStatus {
    #[default]
    Complete,
    /// The user stopped the turn; the text is what had streamed.
    Interrupted,
    /// The turn failed; the text is what had streamed.
    Failed,
}

/// Stable keyset pagination in most-recently-updated order; IDs break timestamp ties.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConversationCursor {
    pub updated_at: i64,
    pub id: Uuid,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConversationPage {
    pub conversations: Vec<Conversation>,
    pub before: Option<ConversationCursor>,
    pub has_more: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: Uuid,
    pub session_id: Uuid,
    pub role: String,
    pub content: String,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<ImageAttachment>,
    #[serde(default, skip_serializing_if = "is_complete")]
    pub status: MessageStatus,
}

fn is_complete(status: &MessageStatus) -> bool {
    *status == MessageStatus::Complete
}

/// A bounded page of conversation history in durable append order. `before`
/// is an opaque cursor for the next older page, independent of wall-clock skew.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryPage {
    pub session_id: Uuid,
    pub messages: Vec<ChatMessage>,
    pub before: Option<i64>,
    pub has_more: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub id: Uuid,
    pub scope: String,
    pub content: String,
    pub source: String,
    pub created_at: i64,
    pub supersedes: Option<Uuid>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub writes: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolObservation {
    pub status: String,
    pub summary: String,
    pub next_actions: Vec<String>,
    pub artifacts: Vec<PathBuf>,
}

#[async_trait]
pub trait EngineBackend: Send + Sync {
    fn name(&self) -> &str;
    async fn run(
        &self,
        request: TurnRequest,
        events: mpsc::Sender<EngineEvent>,
        cancel: CancellationToken,
    ) -> Result<()>;
}

#[async_trait]
pub trait ProviderAdapter: Send + Sync {
    async fn models(&self, config: &ProviderConfig) -> Result<Vec<String>>;
}

#[async_trait]
pub trait MemoryBackend: Send + Sync {
    async fn remember(&self, record: MemoryRecord) -> Result<()>;
    async fn recall(&self, scope: &str, query: &str, limit: usize) -> Result<Vec<MemoryRecord>>;
    async fn forget(&self, id: Uuid) -> Result<()>;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConnectorConfig {
    pub id: Uuid,
    pub name: String,
    pub transport: String,
    pub endpoint: String,
    pub command: Option<PathBuf>,
    pub args: Vec<String>,
    pub credential_ref: Option<String>,
}

/// Product-facing summary of a delegated task; engine metadata stays in the journal.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TeamTask {
    pub id: Uuid,
    pub title: String,
    pub status: String,
    pub summary: String,
}

#[cfg(test)]
mod tests;
