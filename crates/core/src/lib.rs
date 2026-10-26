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
            managed_local: false,
            headers: Default::default(),
        }
    }
}

impl ProviderConfig {
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
    #[default]
    AskForWrites,
    ReadOnly,
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

/// Product layout only; execution and hardware policy are not UI preferences.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct UiPreferences {
    pub sidebar_width: f32,
    pub files_width: f32,
    pub terminal_height: f32,
    pub terminal_visible: bool,
    pub active_page: String,
}

impl Default for UiPreferences {
    fn default() -> Self {
        Self {
            sidebar_width: 248.0,
            files_width: 240.0,
            terminal_height: 260.0,
            terminal_visible: false,
            active_page: "chat".into(),
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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
#[allow(
    clippy::large_enum_variant,
    reason = "A bounded command queue keeps the small turn payload inline and avoids a separate heap allocation"
)]
pub enum WorkerCommand {
    Hello,
    Run { turn: TurnRequest },
    Cancel,
    Steer { message: String },
    SteerDurable { guidance: SteeringRequest },
    Approve { action_id: Uuid, allow: bool },
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
