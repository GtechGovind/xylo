//! Versioned, replayable trajectories.
//!
//! A [`Trace`] holds everything one trial produced: every model request with
//! the reply or error it got (as seen beneath the engine's context guard, so
//! exactly what would reach the wire), every engine event, the journal
//! records of the actions the turn started and its final checkpoint.
//!
//! Invariants:
//! - A trace names its format with [`TRACE_VERSION`]; [`Trace::load`] rejects
//!   any other version instead of guessing.
//! - Replies are stored in the scripted provider's own [`ModelReply`] form, so
//!   a trace replays without translation.
//! - Usage is kept as raw JSON so provider fields the engine adds later (cache
//!   reads, reasoning tokens) survive a round trip and stay measurable.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::Path;
use xylo_core::{EngineEvent, ProviderConfig};
use xylo_engine::{FinishReason, Message, ModelError, ModelRequest, Reasoning, ToolCall};

/// The trace format this crate writes and reads.
pub const TRACE_VERSION: u32 = 2;

/// One trial's complete, replayable record.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Trace {
    pub version: u32,
    /// The task the trial ran; replay recreates its fixtures.
    pub task: String,
    pub trial: u32,
    /// Which model produced the replies.
    pub model: ModelLabel,
    /// The provider configuration the trial ran with, without its credential
    /// reference and headers. Replay runs with it, so window and output
    /// budgets match the recording.
    pub provider: ProviderConfig,
    /// The scripted server's window, when the trial had one; replay applies
    /// it again instead of replaying the rejections it caused.
    pub served_window: Option<u64>,
    /// The user prompt as sent, with fixture placeholders resolved.
    pub prompt: String,
    pub exchanges: Vec<Exchange>,
    pub events: Vec<EngineEvent>,
    /// Journal records of every action the turn started, in event order.
    pub actions: Vec<RecordedAction>,
    /// The conversation checkpoint after the turn ended.
    pub checkpoint: Option<Value>,
}

impl Trace {
    /// Writes the trace as pretty JSON.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Create {}", parent.display()))?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("Write trace {}", path.display()))
    }

    /// Reads a trace, refusing other format versions.
    pub fn load(path: &Path) -> Result<Self> {
        let bytes =
            std::fs::read(path).with_context(|| format!("Read trace {}", path.display()))?;
        let value: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("Parse trace {}", path.display()))?;
        let version = value.get("version").and_then(Value::as_u64);
        ensure!(
            version == Some(u64::from(TRACE_VERSION)),
            "Trace {} has format version {version:?}; this build reads version {TRACE_VERSION}",
            path.display()
        );
        serde_json::from_value(value).with_context(|| format!("Decode trace {}", path.display()))
    }

    /// The exchanges the root agent loop made (not children, summaries or
    /// titles).
    pub fn agent_exchanges(&self) -> impl Iterator<Item = &Exchange> {
        self.exchanges
            .iter()
            .filter(|exchange| exchange.kind == RequestKind::Agent)
    }

    /// The exchanges of delegated child agents, grouped by child (its task
    /// message) in order of first appearance. Children may run in parallel,
    /// so only the order within a child is meaningful.
    pub fn child_conversations(&self) -> Vec<(String, Vec<&Exchange>)> {
        let mut children: Vec<(String, Vec<&Exchange>)> = Vec::new();
        for exchange in &self.exchanges {
            if exchange.kind != RequestKind::Child {
                continue;
            }
            let task = exchange.request.child_task().unwrap_or_default().to_owned();
            match children.iter_mut().find(|(known, _)| *known == task) {
                Some((_, exchanges)) => exchanges.push(exchange),
                None => children.push((task, vec![exchange])),
            }
        }
        children
    }
}

/// `config` without anything secret: no credential reference and no headers
/// (which may carry authorization).
pub fn sanitized(config: &ProviderConfig) -> ProviderConfig {
    ProviderConfig {
        credential_ref: None,
        headers: Default::default(),
        ..config.clone()
    }
}

/// The model a trace was recorded against.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelLabel {
    /// `scripted` for offline runs, otherwise the matrix entry's label.
    pub label: String,
    pub model: String,
}

/// What a model request was for, decided from its shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    /// A step of the root agent loop (it offers tools).
    Agent,
    /// A step of a delegated child agent (its system prompt is the helper
    /// agent prompt, `prompts/subagent.md`).
    Child,
    /// A context summary (no tools, configured reasoning).
    Summary,
    /// A conversation title (lowest reasoning effort).
    Title,
}

impl RequestKind {
    /// Classifies a request the way the engine builds them: only agent
    /// loops offer tools, a child's system prompt opens with the helper
    /// prompt, and only the title job asks for the lowest effort.
    pub fn of(request: &ModelRequest) -> Self {
        if request.reasoning == Reasoning::Lowest {
            Self::Title
        } else if request.tools.is_empty() {
            Self::Summary
        } else if request.messages.first().is_some_and(|message| {
            let opening = child_opening();
            message.role == xylo_engine::Role::System
                && !opening.is_empty()
                && message.content.starts_with(opening)
        }) {
            Self::Child
        } else {
            Self::Agent
        }
    }
}

/// The fixed opening of the helper-agent system prompt: the template's text
/// before its first placeholder.
pub(crate) fn child_opening() -> &'static str {
    const TEMPLATE: &str = include_str!("../../engine/prompts/subagent.md");
    TEMPLATE
        .split("{{")
        .next()
        .unwrap_or_default()
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
}

/// The text of the first user message.
pub(crate) fn first_user(messages: &[Message]) -> Option<&str> {
    messages
        .iter()
        .find(|message| message.role == xylo_engine::Role::User)
        .map(|message| message.content.as_str())
}

/// One model request and what came back.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Exchange {
    pub kind: RequestKind,
    pub request: RecordedRequest,
    pub outcome: Outcome,
}

/// A model request as sent.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordedRequest {
    pub messages: Vec<Message>,
    /// Names of the tools offered, in order.
    pub tools: Vec<String>,
    /// SHA-256 of the full tool definitions; equal digests mean a
    /// byte-identical, cache-friendly tool block.
    pub tools_digest: String,
    pub max_tokens: u32,
}

impl RecordedRequest {
    /// Records `request`.
    pub fn of(request: &ModelRequest) -> Self {
        let definitions = serde_json::to_vec(&request.tools).unwrap_or_default();
        Self {
            messages: request.messages.clone(),
            tools: request.tools.iter().map(|tool| tool.name.clone()).collect(),
            tools_digest: hex::encode(Sha256::digest(&definitions)),
            max_tokens: request.max_tokens,
        }
    }

    /// A child request's task: its first user message.
    pub fn child_task(&self) -> Option<&str> {
        first_user(&self.messages)
    }

    /// The system prompt, when the request starts with one.
    pub fn system(&self) -> Option<&str> {
        self.messages
            .first()
            .filter(|message| message.role == xylo_engine::Role::System)
            .map(|message| message.content.as_str())
    }
}

/// How a request ended.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Reply(ModelReply),
    Error(RecordedError),
    /// The consumer stopped reading before the stream ended (cancellation).
    Abandoned,
}

/// A complete model reply, in the form the scripted provider plays back.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelReply {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default = "stop")]
    pub finish: FinishReason,
    /// Provider usage as reported; `None` lets the scripted provider count
    /// tokens deterministically from the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Value>,
}

fn stop() -> FinishReason {
    FinishReason::Stop
}

impl Default for ModelReply {
    fn default() -> Self {
        Self {
            text: String::new(),
            reasoning: None,
            tool_calls: Vec::new(),
            finish: FinishReason::Stop,
            usage: None,
        }
    }
}

impl ModelReply {
    /// A plain answer.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Self::default()
        }
    }

    /// Tool calls, numbered `call-<step>-<n>` so ids are unique in a script.
    pub fn calls(step: usize, calls: impl IntoIterator<Item = (&'static str, Value)>) -> Self {
        Self {
            tool_calls: calls
                .into_iter()
                .enumerate()
                .map(|(index, (name, arguments))| {
                    ToolCall::new(format!("call-{step}-{index}"), name, arguments.to_string())
                })
                .collect(),
            finish: FinishReason::ToolCalls,
            ..Self::default()
        }
    }

    /// This reply cut off by the output limit.
    pub fn truncated(mut self) -> Self {
        self.finish = FinishReason::Length;
        self
    }
}

/// A provider failure in serializable form.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Authentication,
    RateLimited,
    Transient,
    Permanent,
    InvalidRequest,
    ContextOverflow,
    Timeout,
    Interrupted,
    Io,
    /// The harness stopped a model that kept repeating itself.
    Stalled,
    /// The model returned no usable reply, even when asked again.
    EmptyResponse,
}

/// A recorded provider failure.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedError {
    pub kind: ErrorKind,
    pub message: String,
}

impl RecordedError {
    /// A context-window rejection, as a server whose real window is smaller
    /// than configured would send.
    pub fn context_overflow(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::ContextOverflow,
            message: message.into(),
        }
    }

    /// The equivalent engine error.
    pub fn to_model_error(&self) -> ModelError {
        let message = self.message.clone();
        match self.kind {
            ErrorKind::Authentication => ModelError::Authentication(message),
            ErrorKind::RateLimited => ModelError::RateLimited {
                retry_after: None,
                message,
            },
            ErrorKind::Transient => ModelError::transient(message),
            ErrorKind::Permanent => ModelError::permanent(message),
            ErrorKind::InvalidRequest => ModelError::InvalidRequest(message),
            ErrorKind::ContextOverflow => ModelError::ContextOverflow(message),
            ErrorKind::Timeout => ModelError::Timeout(message),
            ErrorKind::Interrupted => ModelError::Interrupted,
            ErrorKind::Io => ModelError::Io(message),
            ErrorKind::Stalled => ModelError::Stalled(message),
            ErrorKind::EmptyResponse => ModelError::EmptyResponse(message),
        }
    }
}

impl From<&ModelError> for RecordedError {
    fn from(error: &ModelError) -> Self {
        let kind = match error {
            ModelError::Authentication(_) => ErrorKind::Authentication,
            ModelError::RateLimited { .. } => ErrorKind::RateLimited,
            ModelError::Provider { retryable, .. } if *retryable => ErrorKind::Transient,
            ModelError::Provider { .. } => ErrorKind::Permanent,
            ModelError::InvalidRequest(_) => ErrorKind::InvalidRequest,
            ModelError::ContextOverflow(_) => ErrorKind::ContextOverflow,
            ModelError::Timeout(_) => ErrorKind::Timeout,
            ModelError::Interrupted => ErrorKind::Interrupted,
            ModelError::Io(_) => ErrorKind::Io,
            ModelError::Stalled(_) => ErrorKind::Stalled,
            ModelError::EmptyResponse(_) => ErrorKind::EmptyResponse,
        };
        Self {
            kind,
            message: error.to_string(),
        }
    }
}

/// A journal record of one action the turn started.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordedAction {
    pub id: uuid::Uuid,
    pub name: String,
    pub arguments: Value,
    /// Journal state (`succeeded`, `failed`, `denied`, ...), or `missing`
    /// when the event named an action the journal never recorded.
    pub state: String,
    pub result: Option<Value>,
    pub error: Option<String>,
}

#[cfg(test)]
mod tests;
