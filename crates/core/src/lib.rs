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

pub const PROTOCOL_VERSION: u16 = 1;
pub const HERMES_REVISION: &str = "c002f8c4efe475c89c9f14d3995d44e6a5df5b27";
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

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Ask before every change, command and external write.
    #[default]
    AskForWrites,
    /// Inspect only; any writing tool is refused.
    ReadOnly,
    /// Apply file edits inside the project without asking; ask for commands
    /// and external writes.
    AcceptEdits,
    /// Apply project file edits and run local commands without asking; ask for
    /// connectors, plugins, remote environments and other external writes.
    Auto,
}

impl PermissionMode {
    pub const ALL: [PermissionMode; 4] = [
        PermissionMode::AskForWrites,
        PermissionMode::AcceptEdits,
        PermissionMode::Auto,
        PermissionMode::ReadOnly,
    ];
    pub fn label(self) -> &'static str {
        match self {
            PermissionMode::AskForWrites => "Ask before changes",
            PermissionMode::AcceptEdits => "Auto-accept edits",
            PermissionMode::Auto => "Edits and commands",
            PermissionMode::ReadOnly => "Read only",
        }
    }
    pub fn description(self) -> &'static str {
        match self {
            PermissionMode::AskForWrites => "Approve each file change and command",
            PermissionMode::AcceptEdits => {
                "Project file edits apply automatically; commands still ask"
            }
            PermissionMode::Auto => {
                "Project edits and local commands run without asking; external actions still ask"
            }
            PermissionMode::ReadOnly => "Read and search only; nothing is changed",
        }
    }
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
    pub terminal_height: f32,
    pub terminal_visible: bool,
    pub active_page: String,
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
            terminal_height: 260.0,
            terminal_visible: false,
            active_page: "chat".into(),
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
    Cancel,
    Steer {
        message: String,
    },
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
    Shutdown,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub elapsed_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EngineEvent {
    Ready {
        engine: String,
        revision: String,
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
    },
    ApprovalRequired {
        action_id: Uuid,
        name: String,
        arguments: Value,
        /// What "always allow" would cover for the rest of this chat, when the
        /// action can be remembered (for example "`cargo test` commands").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rule: Option<String>,
    },
    ToolFinished {
        action_id: Uuid,
        name: String,
        result: String,
        success: bool,
    },
    Completed {
        text: String,
        usage: Usage,
    },
    Cancelled,
    Error {
        message: String,
        retryable: bool,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Conversation {
    pub id: Uuid,
    pub title: String,
    pub workspace: PathBuf,
    pub updated_at: i64,
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
    fn steer(&self, message: String);
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
mod tests {
    use super::*;
    #[test]
    fn worker_credential_debug_is_redacted() {
        let credential = WorkerCredential(Some("fixture-secret".into()));
        assert!(!format!("{credential:?}").contains("fixture-secret"));
        let decoded: WorkerCredential =
            serde_json::from_str(&serde_json::to_string(&credential).unwrap()).unwrap();
        assert_eq!(decoded.0, credential.0);
    }
    #[test]
    fn incompatible_protocol_is_rejected() {
        let mut message = Envelope::new(Uuid::new_v4(), None, 0, WorkerCommand::Hello);
        message.version = 2;
        assert!(message.validate().is_err());
    }
    #[test]
    fn context_and_endpoint_are_validated() {
        let mut config = ProviderConfig::default();
        assert!(config.validate().is_err());
        config.model = "user-selected-model".into();
        assert!(config.validate().is_ok());
        config.output_tokens = 9000;
        assert!(config.validate().is_err());
    }
}

#[cfg(test)]
mod reasoning_tests {
    use super::*;

    #[test]
    fn remembering_a_model_preserves_distinct_custom_connections() {
        let base = ProviderConfig {
            model: "shared-model".into(),
            ..Default::default()
        };
        for different_catalog in [false, true] {
            let mut first = base.clone();
            let mut second = base.clone();
            if different_catalog {
                first.discovery_url = Some("https://example.com/catalog-a".into());
                second.discovery_url = Some("https://example.com/catalog-b".into());
            } else {
                first.headers.insert("X-Tenant".into(), "a".into());
                second.headers.insert("X-Tenant".into(), "b".into());
            }
            let mut settings = AppSettings::default();
            settings.remember_model(first.clone());
            settings.remember_model(second.clone());
            assert_eq!(settings.providers.len(), 2);
            second.reasoning_effort = Some("high".into());
            settings.remember_model(second.clone());
            assert_eq!(settings.providers.len(), 2);
            assert_eq!(settings.providers[0].headers, first.headers);
            assert_eq!(settings.providers[0].discovery_url, first.discovery_url);
            assert_eq!(settings.providers[0].reasoning_effort, None);
            assert_eq!(settings.providers[1].reasoning_effort, second.reasoning_effort);
        }
    }

    #[test]
    fn legacy_provider_configuration_keeps_provider_default_reasoning() {
        let mut value = serde_json::to_value(ProviderConfig::default()).unwrap();
        value.as_object_mut().unwrap().remove("reasoning_effort");
        value.as_object_mut().unwrap().remove("reasoning_options");
        value
            .as_object_mut()
            .unwrap()
            .remove("default_reasoning_effort");
        value.as_object_mut().unwrap().remove("discovery_url");
        let config: ProviderConfig = serde_json::from_value(value).unwrap();
        assert_eq!(config.reasoning_effort, None);
        assert_eq!(config.reasoning_options, None);
        assert_eq!(config.default_reasoning_effort, None);
        assert_eq!(config.discovery_url, None);
    }

    #[test]
    fn advertised_reasoning_survives_persistence_and_explicit_no_support_wins() {
        let mut config = ProviderConfig {
            model: "future-account-model".into(),
            reasoning_options: Some(vec![
                "high".into(),
                "low".into(),
                "high".into(),
                "invalid value".into(),
            ]),
            default_reasoning_effort: Some("low".into()),
            reasoning_effort: Some("high".into()),
            ..Default::default()
        };
        let restored: ProviderConfig =
            serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
        assert_eq!(restored.supported_reasoning_efforts(), vec!["high", "low"]);
        assert_eq!(restored.effective_reasoning_effort(), Some("high"));
        assert_eq!(restored.default_reasoning_effort.as_deref(), Some("low"));
        config.model = "gpt-5.2".into();
        config.reasoning_options = Some(vec![]);
        assert!(config.supported_reasoning_efforts().is_empty());
        assert_eq!(config.effective_reasoning_effort(), None);
        config.reasoning_options = Some(vec!["high".into()]);
        config.family = ApiFamily::Anthropic;
        assert!(config.supported_reasoning_efforts().is_empty());
    }

    #[test]
    fn reasoning_capability_follows_protocol_model_and_saved_effort() {
        let mut config = ProviderConfig {
            model: "gpt-5.2".into(),
            reasoning_effort: Some("xhigh".into()),
            ..Default::default()
        };
        assert_eq!(config.effective_reasoning_effort(), Some("xhigh"));
        config.model = "gpt-5.2-pro".into();
        config.reasoning_effort = Some("low".into());
        assert_eq!(config.effective_reasoning_effort(), None);
        config.model = "gpt-4.1".into();
        assert!(config.supported_reasoning_efforts().is_empty());
        config.model = "gpt-5.2-chat-latest".into();
        assert!(config.supported_reasoning_efforts().is_empty());
        config.model = "gpt-5.2".into();
        config.family = ApiFamily::Anthropic;
        assert!(config.supported_reasoning_efforts().is_empty());
    }
}

#[cfg(test)]
mod native_workspace_preferences_tests {
    use super::UiPreferences;
    #[test]
    fn existing_ui_preferences_load_with_new_workspace_defaults() {
        let prefs: UiPreferences = serde_json::from_value(serde_json::json!({"sidebar_width":248.0,"files_width":240.0,"terminal_height":260.0,"terminal_visible":false,"active_page":"chat"})).unwrap();
        assert!(prefs.sidebar_visible);
        assert!(!prefs.work_panel_open);
        assert!(prefs.open_projects.is_empty());
        assert_eq!(prefs.work_panel, "files");
        let saved = serde_json::to_value(&prefs).unwrap();
        let restored: UiPreferences = serde_json::from_value(saved).unwrap();
        assert_eq!(restored.active_page, "chat");
        assert_eq!(restored.sidebar_width, 248.0);
    }
}
