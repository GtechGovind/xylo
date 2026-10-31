//! The engine's own conversation and model-provider types.
//!
//! [`Message`] is both the durable checkpoint format and the input every
//! provider adapter translates into its wire protocol. Provider-native content
//! (Anthropic thinking signatures, Gemini thought signatures, Responses
//! encrypted reasoning, Bedrock content blocks) rides along in
//! [`Message::native`] so a tool round and a checkpoint replay it byte for
//! byte. Managed image references stay in [`Message::attachments`]; image
//! bytes are only ever hydrated into [`Message::images`] for one request and
//! are never serialized.

use async_trait::async_trait;
use futures_util::{StreamExt, stream::BoxStream};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, fmt, time::Duration};
use tokio_util::sync::CancellationToken;
use xylo_core::{ErrorKind, ImageAttachment};

/// Who wrote a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A tool call requested by the model. `arguments` is the raw JSON text the
/// provider produced; it is validated only when the call is dispatched.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl ToolCall {
    /// A call with the given provider identity, tool name and JSON arguments.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }
}

/// The exact provider-native form of an assistant response, replayed verbatim
/// when the same protocol sees the conversation again.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "protocol", content = "items", rename_all = "snake_case")]
pub enum NativeContent {
    /// Anthropic Messages content blocks, including signed thinking.
    Anthropic(Vec<Value>),
    /// Responses API output items, including encrypted reasoning.
    Responses(Vec<Value>),
    /// Gemini content parts, including thought signatures.
    Gemini(Vec<Value>),
    /// Bedrock Converse content blocks, including signed reasoning.
    Bedrock(Vec<Value>),
}

/// An image hydrated for a single provider request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineImage {
    /// `image/png`, `image/jpeg`, `image/webp` or `image/gif`.
    pub media_type: String,
    /// Standard base64 of the image bytes.
    pub data: String,
}

/// One conversation message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    /// Text content; empty when the message carries only tool calls.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// The call a tool result answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Durable identity of user guidance inserted by the store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Visible reasoning text the provider returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<NativeContent>,
    /// Managed image references attached to a user message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<ImageAttachment>,
    /// Marks the model-written summary that replaced earlier history.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub summary: bool,
    /// Hydrated attachment bytes for one request; never stored.
    #[serde(skip)]
    pub images: Vec<InlineImage>,
}

impl Message {
    fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
            reasoning: None,
            native: None,
            attachments: Vec::new(),
            summary: false,
            images: Vec::new(),
        }
    }

    /// A system message.
    pub fn system(content: impl Into<String>) -> Self {
        Self::new(Role::System, content)
    }

    /// A user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self::new(Role::User, content)
    }

    /// An assistant text message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new(Role::Assistant, content)
    }

    /// An assistant message that requests tool calls.
    pub fn assistant_with_tool_calls(content: impl Into<String>, calls: Vec<ToolCall>) -> Self {
        let mut message = Self::new(Role::Assistant, content);
        message.tool_calls = calls;
        message
    }

    /// The result of the tool call `tool_call_id`.
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        let mut message = Self::new(Role::Tool, content);
        message.tool_call_id = Some(tool_call_id.into());
        message
    }

    /// Renames one of this message's tool calls everywhere it appears,
    /// including the provider-native replay content.
    pub fn rename_tool_call(&mut self, id: &str, name: &str) {
        for call in self.tool_calls.iter_mut().filter(|call| call.id == id) {
            call.name = name.to_owned();
        }
        let Some((items, id_pointer, name_pointer)) = self.native_calls() else {
            return;
        };
        for item in items {
            if item.pointer(id_pointer).and_then(Value::as_str) == Some(id)
                && let Some(slot) = item.pointer_mut(name_pointer)
            {
                *slot = Value::String(name.to_owned());
            }
        }
    }

    /// Gives the `occurrence`-th call (counting from 0) with identity `id`
    /// the identity `new_id`, in the calls and in the native replay content.
    pub fn rename_tool_call_id(&mut self, id: &str, occurrence: usize, new_id: &str) {
        if let Some(call) = self
            .tool_calls
            .iter_mut()
            .filter(|call| call.id == id)
            .nth(occurrence)
        {
            call.id = new_id.to_owned();
        }
        let Some((items, id_pointer, _)) = self.native_calls() else {
            return;
        };
        if let Some(slot) = items
            .iter_mut()
            .filter_map(|item| item.pointer_mut(id_pointer))
            .filter(|slot| slot.as_str() == Some(id))
            .nth(occurrence)
        {
            *slot = Value::String(new_id.to_owned());
        }
    }

    /// The native items that describe tool calls, with the JSON pointers of
    /// a call's identity and name.
    fn native_calls(&mut self) -> Option<(&mut Vec<Value>, &'static str, &'static str)> {
        Some(match self.native.as_mut()? {
            NativeContent::Anthropic(items) => (items, "/id", "/name"),
            NativeContent::Responses(items) => (items, "/call_id", "/name"),
            NativeContent::Gemini(items) => (items, "/functionCall/id", "/functionCall/name"),
            NativeContent::Bedrock(items) => (items, "/toolUse/toolUseId", "/toolUse/name"),
        })
    }
}

/// A tool offered to the model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    /// JSON Schema of the arguments object.
    pub parameters: Value,
}

/// Token counts a provider reported for one request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Why the model stopped generating.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    ToolCalls,
    /// The output token limit was reached.
    Length,
}

/// One increment of a streamed response, in provider order.
#[derive(Clone, Debug, PartialEq)]
pub enum StreamEvent {
    Text(String),
    Reasoning(String),
    /// A fragment of tool call `index`; later fragments append `arguments`.
    ToolCall {
        index: u32,
        id: Option<String>,
        name: Option<String>,
        arguments: Option<String>,
    },
    /// The complete native form of the response, sent once near the end.
    Native(NativeContent),
    Usage(ModelUsage),
    Finished(FinishReason),
}

/// A complete model response.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelResponse {
    pub message: Message,
    pub usage: Option<ModelUsage>,
    pub finish: FinishReason,
}

/// What to send to the model.
#[derive(Clone, Debug)]
pub struct ModelRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
    /// Output token limit for this request.
    pub max_tokens: u32,
    pub reasoning: Reasoning,
}

/// How much reasoning effort a request asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reasoning {
    /// The effort the user chose for the model.
    Configured,
    /// The lowest effort the model supports, for small utility requests
    /// (titles) whose output limit reasoning would otherwise exhaust.
    Lowest,
}

/// A typed provider failure. `retryable` and `kind` drive the agent loop's
/// retries and the UI's Retry-versus-Reconnect choice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelError {
    /// Credentials are missing, expired or rejected.
    Authentication(String),
    /// The provider asked us to slow down.
    RateLimited {
        retry_after: Option<Duration>,
        message: String,
    },
    /// The provider or the network failed.
    Provider {
        retryable: bool,
        retry_after: Option<Duration>,
        message: String,
    },
    /// The request itself is invalid for this provider or model.
    InvalidRequest(String),
    /// The conversation does not fit the model's context window.
    ContextOverflow(String),
    /// A deadline passed; the request may be retried.
    Timeout(String),
    /// The task was cancelled.
    Interrupted,
    /// A local fault (storage, encoding).
    Io(String),
}

impl ModelError {
    /// A provider failure that is worth retrying.
    pub fn transient(message: impl Into<String>) -> Self {
        Self::Provider {
            retryable: true,
            retry_after: None,
            message: message.into(),
        }
    }

    /// A provider failure that repeating the same request will not fix.
    pub fn permanent(message: impl Into<String>) -> Self {
        Self::Provider {
            retryable: false,
            retry_after: None,
            message: message.into(),
        }
    }

    /// Whether repeating the same request may succeed.
    pub fn retryable(&self) -> bool {
        match self {
            Self::RateLimited { .. } | Self::Timeout(_) => true,
            Self::Provider { retryable, .. } => *retryable,
            Self::Authentication(_)
            | Self::InvalidRequest(_)
            | Self::ContextOverflow(_)
            | Self::Interrupted
            | Self::Io(_) => false,
        }
    }

    /// The delay the provider asked for, when it named one.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after, .. } | Self::Provider { retry_after, .. } => {
                *retry_after
            }
            _ => None,
        }
    }

    /// The product classification of this failure.
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::Authentication(_) => ErrorKind::Authentication,
            Self::RateLimited { .. } => ErrorKind::RateLimited,
            Self::Provider { .. } | Self::Timeout(_) | Self::InvalidRequest(_) => {
                ErrorKind::Provider
            }
            Self::ContextOverflow(_) => ErrorKind::ContextOverflow,
            Self::Interrupted | Self::Io(_) => ErrorKind::Internal,
        }
    }
}

impl fmt::Display for ModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Authentication(message)
            | Self::RateLimited { message, .. }
            | Self::Provider { message, .. }
            | Self::InvalidRequest(message)
            | Self::ContextOverflow(message)
            | Self::Timeout(message)
            | Self::Io(message) => formatter.write_str(message),
            Self::Interrupted => formatter.write_str("The task was cancelled"),
        }
    }
}

impl std::error::Error for ModelError {}

/// A provider's streamed response.
pub type EventStream = BoxStream<'static, Result<StreamEvent, ModelError>>;

/// A model behind one wire protocol, or a layer that wraps one.
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// Streams one response. Cancelling `cancel` ends the stream with
    /// [`ModelError::Interrupted`] promptly.
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> EventStream;

    /// One complete response, assembled from [`ModelProvider::stream`].
    async fn complete(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ModelResponse, ModelError> {
        let mut stream = self.stream(request, cancel);
        let mut response = Assembler::default();
        while let Some(event) = stream.next().await {
            response.push(event?);
        }
        response.finish()
    }
}

/// Builds a [`ModelResponse`] from stream events.
#[derive(Default)]
pub struct Assembler {
    text: String,
    reasoning: String,
    calls: BTreeMap<u32, ToolCall>,
    native: Option<NativeContent>,
    usage: Option<ModelUsage>,
    finish: Option<FinishReason>,
}

impl Assembler {
    /// Adds one event.
    pub fn push(&mut self, event: StreamEvent) {
        match event {
            StreamEvent::Text(text) => self.text.push_str(&text),
            StreamEvent::Reasoning(text) => self.reasoning.push_str(&text),
            StreamEvent::ToolCall {
                index,
                id,
                name,
                arguments,
            } => {
                let call = self
                    .calls
                    .entry(index)
                    .or_insert_with(|| ToolCall::new("", "", ""));
                if let Some(id) = id {
                    call.id = id;
                }
                if let Some(name) = name {
                    call.name = name;
                }
                if let Some(arguments) = arguments {
                    call.arguments.push_str(&arguments);
                }
            }
            StreamEvent::Native(native) => self.native = Some(native),
            StreamEvent::Usage(usage) => self.usage = Some(usage),
            StreamEvent::Finished(reason) => self.finish = Some(reason),
        }
    }

    /// The complete response; fails when the provider never finished or sent
    /// a tool call without identity.
    pub fn finish(self) -> Result<ModelResponse, ModelError> {
        let finish = self
            .finish
            .ok_or_else(|| ModelError::transient("Provider stream ended before completion"))?;
        let mut calls = self.calls.into_values().collect::<Vec<_>>();
        if calls
            .iter()
            .any(|call| call.id.is_empty() || call.name.is_empty())
        {
            return Err(ModelError::permanent(
                "Provider returned an incomplete function call",
            ));
        }
        for call in &mut calls {
            if call.arguments.trim().is_empty() {
                call.arguments = "{}".into();
            }
        }
        let mut message = Message::assistant_with_tool_calls(self.text, calls);
        message.reasoning = (!self.reasoning.is_empty()).then_some(self.reasoning);
        message.native = self.native;
        let finish = if message.tool_calls.is_empty() || finish == FinishReason::Length {
            finish
        } else {
            FinishReason::ToolCalls
        };
        Ok(ModelResponse {
            message,
            usage: self.usage,
            finish,
        })
    }
}

#[cfg(test)]
mod tests;
