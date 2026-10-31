//! SSE framing and per-protocol event decoding into [`StreamEvent`]s. Every
//! protocol must reach its own terminal event; a stream that merely stops is a
//! retryable failure, never a successful empty answer.
use super::*;
use crate::model::{FinishReason, NativeContent, StreamEvent};
use serde_json::json;
use std::collections::{BTreeMap, HashMap};

fn failure(message: impl Into<String>) -> ModelError {
    ModelError::permanent(message)
}

/// Stream error events whose code or type means "try again later".
fn transient_code(value: &Value) -> bool {
    let error = value.get("error").unwrap_or(value);
    ["type", "code", "status"].iter().any(|key| {
        error.get(*key).and_then(Value::as_str).is_some_and(|code| {
            matches!(
                code,
                "overloaded_error"
                    | "rate_limit_error"
                    | "rate_limit_exceeded"
                    | "server_error"
                    | "api_error"
                    | "internal_error"
                    | "internal_server_error"
                    | "service_unavailable"
                    | "UNAVAILABLE"
                    | "RESOURCE_EXHAUSTED"
                    | "INTERNAL"
            )
        })
    })
}

#[derive(Default)]
pub(super) struct SseDecoder {
    pending: Vec<u8>,
    data: Vec<u8>,
    kind: String,
    total: usize,
}

pub(super) struct Event {
    pub(super) kind: String,
    pub(super) data: String,
}

impl SseDecoder {
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Event>, ModelError> {
        self.total = self.total.saturating_add(bytes.len());
        if self.total > 32 * 1024 * 1024 || self.pending.len() + bytes.len() > 8 * 1024 * 1024 {
            return Err(failure("Provider stream exceeded its byte budget"));
        }
        self.pending.extend_from_slice(bytes);
        let mut output = Vec::new();
        let mut used = 0;
        while let Some(end) = self.pending[used..].iter().position(|byte| *byte == b'\n') {
            let end = used + end;
            let line = self.pending[used..end]
                .strip_suffix(b"\r")
                .unwrap_or(&self.pending[used..end]);
            if line.is_empty() {
                if !self.data.is_empty() {
                    if self.data.last() == Some(&b'\n') {
                        self.data.pop();
                    }
                    let data = String::from_utf8(std::mem::take(&mut self.data))
                        .map_err(|_| failure("Provider stream contains invalid UTF-8"))?;
                    output.push(Event {
                        kind: std::mem::take(&mut self.kind),
                        data,
                    });
                } else {
                    self.kind.clear();
                }
            } else if let Some(data) = line.strip_prefix(b"data:") {
                let data = data.strip_prefix(b" ").unwrap_or(data);
                if self.data.len() + data.len() > 8 * 1024 * 1024 {
                    return Err(failure("Provider SSE event exceeds 8 MiB"));
                }
                self.data.extend_from_slice(data);
                self.data.push(b'\n');
            } else if let Some(kind) = line.strip_prefix(b"event:") {
                self.kind = String::from_utf8(kind.strip_prefix(b" ").unwrap_or(kind).to_vec())
                    .map_err(|_| failure("Invalid provider SSE event name"))?;
            }
            used = end + 1;
        }
        self.pending.drain(..used);
        Ok(output)
    }

    pub fn finish(&self) -> Result<(), ModelError> {
        if !self.pending.is_empty() || !self.data.is_empty() {
            return Err(ModelError::transient(
                "Provider stream ended inside an SSE frame",
            ));
        }
        Ok(())
    }
}

fn tool(
    index: u32,
    id: Option<String>,
    name: Option<String>,
    arguments: Option<String>,
) -> StreamEvent {
    StreamEvent::ToolCall {
        index,
        id,
        name,
        arguments,
    }
}

/// A streamed tool input as JSON: empty input is an empty object, and
/// `None` means the input stopped before it was valid JSON.
pub(super) fn tool_input(arguments: &str) -> Option<Value> {
    if arguments.trim().is_empty() {
        return Some(json!({}));
    }
    serde_json::from_str(arguments).ok()
}

/// Maps a Chat Completions `finish_reason` or Anthropic `stop_reason`.
///
/// Every reason that means "the output ran out of room" is [`Length`]
/// (Anthropic `model_context_window_exceeded`, Mistral `model_length`), so
/// the loop keeps the partial answer and recovers instead of failing the
/// turn. `eos`/`eos_token` are the plain stop of Text Generation Inference
/// and llama.cpp-derived servers.
///
/// [`Length`]: FinishReason::Length
fn finish_reason(reason: &str) -> Result<FinishReason, ModelError> {
    match reason {
        "stop" | "end_turn" | "stop_sequence" | "eos" | "eos_token" => Ok(FinishReason::Stop),
        "tool_calls" | "tool_use" | "function_call" => Ok(FinishReason::ToolCalls),
        "length" | "max_tokens" | "model_length" | "model_context_window_exceeded" => {
            Ok(FinishReason::Length)
        }
        "content_filter" | "refusal" => Err(failure("Provider filtered the response")),
        other => Err(failure(format!("Provider generation stopped ({other})"))),
    }
}

pub(super) struct EventState {
    protocol: ProviderProtocol,
    error_redactions: Arc<Vec<String>>,
    request_id: Option<String>,
    terminal: bool,
    choice_finished: bool,
    tools: HashMap<u64, u32>,
    tool_arguments: HashMap<u64, String>,
    blocks: BTreeMap<u64, Value>,
    /// Streamed text of an Anthropic block field, kept as a growing string
    /// and written into its block when the block (or message) ends, so a
    /// long block costs linear rather than quadratic time.
    block_text: BTreeMap<(u64, &'static str), String>,
    gemini_parts: Vec<Value>,
    input: u64,
    output: u64,
    /// Prompt-cache reads and writes, when the provider reports them.
    cache_read: Option<u64>,
    cache_write: Option<u64>,
    finish_reason: Option<FinishReason>,
    text_emitted: bool,
    /// A tool input stopped before it was valid JSON.
    truncated_input: bool,
}

impl EventState {
    pub fn new(
        protocol: ProviderProtocol,
        error_redactions: Arc<Vec<String>>,
        request_id: Option<String>,
    ) -> Self {
        Self {
            protocol,
            error_redactions,
            request_id,
            terminal: false,
            choice_finished: false,
            tools: HashMap::new(),
            tool_arguments: HashMap::new(),
            blocks: BTreeMap::new(),
            block_text: BTreeMap::new(),
            gemini_parts: Vec::new(),
            input: 0,
            output: 0,
            cache_read: None,
            cache_write: None,
            finish_reason: None,
            text_emitted: false,
            truncated_input: false,
        }
    }

    /// Compacts provider indexes (which may skip text blocks) to 0, 1, 2…
    fn index(&mut self, native: u64) -> u32 {
        let next = self.tools.len() as u32;
        *self.tools.entry(native).or_insert(next)
    }

    fn stream_error(&self, value: &Value, fallback: &str) -> ModelError {
        let message = diagnostics::stream_error(
            value,
            &self.error_redactions,
            fallback,
            self.request_id.as_deref(),
        );
        if transient_code(value) {
            ModelError::transient(message)
        } else {
            ModelError::permanent(message)
        }
    }

    pub fn event(&mut self, event: Event) -> Result<Vec<StreamEvent>, ModelError> {
        if event.data == "[DONE]" {
            if self.protocol != ProviderProtocol::ChatCompletions || !self.choice_finished {
                return Err(ModelError::transient(
                    "Provider ended a stream without a terminal completion",
                ));
            }
            self.terminal = true;
            return Ok(Vec::new());
        }
        let value: Value = serde_json::from_str(&event.data)
            .map_err(|error| failure(format!("Invalid provider SSE JSON: {error}")))?;
        if value.get("error").is_some() || event.kind == "error" || value["type"] == "error" {
            return Err(self.stream_error(&value, "stream_error"));
        }
        match self.protocol {
            ProviderProtocol::ChatCompletions => self.chat(&value),
            ProviderProtocol::Responses => self.responses(&value, &event.kind),
            ProviderProtocol::AnthropicMessages => self.anthropic(&value, &event.kind),
            ProviderProtocol::GeminiGenerateContent => self.gemini(&value),
            ProviderProtocol::BedrockConverse => {
                Err(failure("Use the native Bedrock streaming decoder"))
            }
        }
    }

    fn chat(&mut self, value: &Value) -> Result<Vec<StreamEvent>, ModelError> {
        let mut output = Vec::new();
        if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
            // `prompt_tokens` already includes the cached ones.
            output.push(StreamEvent::Usage(
                super::usage(
                    usage["prompt_tokens"].as_u64().unwrap_or(0),
                    usage["completion_tokens"].as_u64().unwrap_or(0),
                )
                .with_cache(
                    usage
                        .pointer("/prompt_tokens_details/cached_tokens")
                        .and_then(Value::as_u64),
                    None,
                ),
            ));
        }
        let Some(choice) = value.pointer("/choices/0") else {
            return Ok(output);
        };
        if let Some(thinking) = choice["delta"]["reasoning_content"].as_str() {
            output.push(StreamEvent::Reasoning(thinking.into()));
        }
        if let Some(content) = choice["delta"]["content"].as_str() {
            output.push(StreamEvent::Text(content.into()));
        }
        for call in choice["delta"]["tool_calls"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let raw = call["index"]
                .as_u64()
                .ok_or_else(|| failure("Streaming tool call has no index"))?;
            let index = self.index(raw);
            output.push(tool(
                index,
                call["id"].as_str().map(str::to_owned),
                call["function"]["name"].as_str().map(str::to_owned),
                call["function"]["arguments"].as_str().map(str::to_owned),
            ));
        }
        if let Some(finish) = choice["finish_reason"].as_str() {
            self.choice_finished = true;
            output.push(StreamEvent::Finished(finish_reason(finish)?));
        }
        Ok(output)
    }

    fn responses(&mut self, value: &Value, kind: &str) -> Result<Vec<StreamEvent>, ModelError> {
        let mut output = Vec::new();
        let kind = value["type"].as_str().unwrap_or(kind);
        match kind {
            "response.output_text.delta" => {
                self.text_emitted = true;
                output.push(StreamEvent::Text(
                    value["delta"]
                        .as_str()
                        .ok_or_else(|| failure("Responses text delta is invalid"))?
                        .into(),
                ));
            }
            "response.output_item.added" => {
                let item = &value["item"];
                let raw = value["output_index"]
                    .as_u64()
                    .ok_or_else(|| failure("Responses item has no output index"))?;
                if item["type"] == "function_call" {
                    let index = self.index(raw);
                    output.push(tool(
                        index,
                        item["call_id"].as_str().map(str::to_owned),
                        item["name"].as_str().map(str::to_owned),
                        None,
                    ));
                }
            }
            "response.function_call_arguments.delta" => {
                let raw = value["output_index"]
                    .as_u64()
                    .ok_or_else(|| failure("Responses arguments have no output index"))?;
                let args = value["delta"]
                    .as_str()
                    .ok_or_else(|| failure("Responses arguments delta is invalid"))?;
                self.tool_arguments.entry(raw).or_default().push_str(args);
                let index = self.index(raw);
                output.push(tool(index, None, None, Some(args.into())));
            }
            "response.output_item.done" => {
                let raw = value["output_index"]
                    .as_u64()
                    .ok_or_else(|| failure("Responses item has no output index"))?;
                let item = value["item"].clone();
                if item["type"] == "function_call" && !self.tool_arguments.contains_key(&raw) {
                    let index = self.index(raw);
                    output.push(tool(
                        index,
                        item["call_id"].as_str().map(str::to_owned),
                        item["name"].as_str().map(str::to_owned),
                        Some(item["arguments"].as_str().unwrap_or("{}").into()),
                    ));
                }
                self.blocks.insert(raw, item);
            }
            "response.incomplete"
                if value
                    .pointer("/response/incomplete_details/reason")
                    .and_then(Value::as_str)
                    == Some("max_output_tokens") =>
            {
                output.extend(self.responses_terminal(value)?);
            }
            "response.completed" => output.extend(self.responses_terminal(value)?),
            "response.failed" | "response.incomplete" => {
                return Err(self.stream_error(value, kind));
            }
            _ => {}
        }
        Ok(output)
    }

    /// The events closing a Responses stream that completed or stopped at
    /// the output limit.
    fn responses_terminal(&mut self, value: &Value) -> Result<Vec<StreamEvent>, ModelError> {
        let mut output = Vec::new();
        {
            {
                let parsed = request::parse_responses(&value["response"])?;
                if !self.text_emitted && !parsed.text.is_empty() {
                    output.push(StreamEvent::Text(parsed.text));
                }
                if self.tools.is_empty() {
                    for (index, call) in parsed.calls.into_iter().enumerate() {
                        output.push(tool(
                            index as u32,
                            Some(call.id),
                            Some(call.name),
                            Some(call.arguments),
                        ));
                    }
                }
                // The ChatGPT backend streams every output item but sends
                // `response.completed` with an empty output list. Replay must
                // use the streamed items then, or the next request carries
                // tool results without their calls.
                let items = if parsed.items.is_empty() {
                    std::mem::take(&mut self.blocks).into_values().collect()
                } else {
                    parsed.items
                };
                output.push(StreamEvent::Native(NativeContent::Responses(items)));
                output.push(StreamEvent::Usage(parsed.usage));
                output.push(StreamEvent::Finished(parsed.finish));
                self.terminal = true;
            }
        }
        Ok(output)
    }

    fn anthropic(&mut self, value: &Value, kind: &str) -> Result<Vec<StreamEvent>, ModelError> {
        let mut output = Vec::new();
        let kind = value["type"].as_str().unwrap_or(kind);
        match kind {
            "message_start" => {
                let usage = &value["message"]["usage"];
                self.cache_read = usage["cache_read_input_tokens"].as_u64();
                self.cache_write = usage["cache_creation_input_tokens"].as_u64();
                self.input = usage["input_tokens"].as_u64().unwrap_or(0)
                    + self.cache_read.unwrap_or(0)
                    + self.cache_write.unwrap_or(0);
            }
            "content_block_start" => {
                let raw = value["index"]
                    .as_u64()
                    .ok_or_else(|| failure("Anthropic content block has no index"))?;
                let block = value["content_block"].clone();
                match block["type"].as_str() {
                    Some("tool_use") => {
                        let index = self.index(raw);
                        output.push(tool(
                            index,
                            block["id"].as_str().map(str::to_owned),
                            block["name"].as_str().map(str::to_owned),
                            None,
                        ));
                    }
                    Some("text") => {
                        if let Some(content) =
                            block["text"].as_str().filter(|content| !content.is_empty())
                        {
                            output.push(StreamEvent::Text(content.into()));
                        }
                    }
                    Some("thinking" | "redacted_thinking") => {}
                    _ => return Err(failure("Unsupported Anthropic content block")),
                }
                self.blocks.insert(raw, block);
            }
            "content_block_delta" => {
                let raw = value["index"]
                    .as_u64()
                    .ok_or_else(|| failure("Anthropic content delta has no index"))?;
                let change = &value["delta"];
                let block = self
                    .blocks
                    .get(&raw)
                    .ok_or_else(|| failure("Anthropic delta arrived before its block"))?;
                let block_text = &mut self.block_text;
                let mut append = |field: &'static str, text: &str| {
                    block_text
                        .entry((raw, field))
                        .or_insert_with(|| block[field].as_str().unwrap_or("").to_owned())
                        .push_str(text);
                };
                match change["type"].as_str() {
                    Some("text_delta") => {
                        let content = change["text"]
                            .as_str()
                            .ok_or_else(|| failure("Invalid Anthropic text delta"))?;
                        append("text", content);
                        output.push(StreamEvent::Text(content.into()));
                    }
                    Some("thinking_delta") => {
                        let thinking = change["thinking"]
                            .as_str()
                            .ok_or_else(|| failure("Invalid Anthropic thinking delta"))?;
                        append("thinking", thinking);
                        output.push(StreamEvent::Reasoning(thinking.into()));
                    }
                    Some("signature_delta") => {
                        append("signature", change["signature"].as_str().unwrap_or(""));
                    }
                    Some("input_json_delta") => {
                        let args = change["partial_json"]
                            .as_str()
                            .ok_or_else(|| failure("Invalid Anthropic input delta"))?;
                        self.tool_arguments.entry(raw).or_default().push_str(args);
                        let index = self.index(raw);
                        output.push(tool(index, None, None, Some(args.into())));
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let raw = value["index"]
                    .as_u64()
                    .ok_or_else(|| failure("Anthropic stop has no block index"))?;
                self.settle_block_text(Some(raw));
                if let Some(args) = self.tool_arguments.get(&raw) {
                    // A tool without arguments streams an empty input; input
                    // cut off by the output limit is settled at message_stop.
                    let input = match tool_input(args) {
                        Some(input) => input,
                        None => {
                            self.truncated_input = true;
                            json!({})
                        }
                    };
                    self.blocks
                        .get_mut(&raw)
                        .ok_or_else(|| failure("Anthropic tool block disappeared"))?["input"] =
                        input;
                } else if let Some(block) = self
                    .blocks
                    .get(&raw)
                    .filter(|block| block["type"] == "tool_use")
                {
                    let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                    let index = self.tools[&raw];
                    output.push(tool(index, None, None, Some(input.to_string())));
                }
            }
            "message_delta" => {
                self.output = value["usage"]["output_tokens"]
                    .as_u64()
                    .unwrap_or(self.output);
                if let Some(reason) = value["delta"]["stop_reason"].as_str() {
                    self.finish_reason = Some(finish_reason(reason)?);
                }
            }
            "message_stop" => {
                let finish = self
                    .finish_reason
                    .ok_or_else(|| failure("Anthropic stream ended without a stop reason"))?;
                if self.truncated_input && finish != FinishReason::Length {
                    return Err(failure("Anthropic tool input ended before valid JSON"));
                }
                self.settle_block_text(None);
                output.push(StreamEvent::Native(NativeContent::Anthropic(
                    self.blocks.values().cloned().collect(),
                )));
                output.push(StreamEvent::Usage(
                    super::usage(self.input, self.output)
                        .with_cache(self.cache_read, self.cache_write),
                ));
                output.push(StreamEvent::Finished(finish));
                self.terminal = true;
            }
            _ => {}
        }
        Ok(output)
    }

    /// Writes the streamed text of block `raw` (or of every block) into it.
    fn settle_block_text(&mut self, raw: Option<u64>) {
        let (settled, pending): (BTreeMap<_, _>, BTreeMap<_, _>) =
            std::mem::take(&mut self.block_text)
                .into_iter()
                .partition(|((index, _), _)| raw.is_none_or(|raw| *index == raw));
        self.block_text = pending;
        for ((index, field), text) in settled {
            if let Some(block) = self.blocks.get_mut(&index) {
                block[field] = Value::String(text);
            }
        }
    }

    fn gemini(&mut self, value: &Value) -> Result<Vec<StreamEvent>, ModelError> {
        let mut output = Vec::new();
        if let Some(usage) = value.get("usageMetadata") {
            // `promptTokenCount` already includes the cached content.
            self.input = usage["promptTokenCount"].as_u64().unwrap_or(self.input);
            self.cache_read = usage["cachedContentTokenCount"]
                .as_u64()
                .or(self.cache_read);
            self.output = usage["candidatesTokenCount"].as_u64().unwrap_or(0)
                + usage["thoughtsTokenCount"].as_u64().unwrap_or(0);
        }
        if let Some(candidate) = value.pointer("/candidates/0") {
            for part in candidate["content"]["parts"]
                .as_array()
                .into_iter()
                .flatten()
            {
                let mut part = part.clone();
                if let Some(content) = part["text"].as_str() {
                    output.push(if part["thought"] == true {
                        StreamEvent::Reasoning(content.into())
                    } else {
                        StreamEvent::Text(content.into())
                    });
                }
                if let Some(call) = part.get_mut("functionCall") {
                    let raw = self.tools.len() as u64;
                    let id = call["id"]
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                    call["id"] = json!(id);
                    let index = self.index(raw);
                    output.push(tool(
                        index,
                        Some(id),
                        call["name"].as_str().map(str::to_owned),
                        Some(
                            call.get("args")
                                .cloned()
                                .unwrap_or_else(|| json!({}))
                                .to_string(),
                        ),
                    ));
                }
                self.gemini_parts.push(part);
            }
            if let Some(reason) = candidate["finishReason"].as_str() {
                self.finish_reason = Some(match reason {
                    "STOP" if self.tools.is_empty() => FinishReason::Stop,
                    "STOP" => FinishReason::ToolCalls,
                    "MAX_TOKENS" => FinishReason::Length,
                    // A recoverable model mistake: the loop asks for the
                    // call again instead of failing the turn.
                    "MALFORMED_FUNCTION_CALL" => FinishReason::MalformedToolCall,
                    other => return Err(failure(format!("Gemini generation stopped ({other})"))),
                });
                self.terminal = true;
            }
        } else if value.get("promptFeedback").is_some() {
            return Err(failure("Gemini blocked the prompt"));
        }
        if self.terminal {
            output.push(StreamEvent::Native(NativeContent::Gemini(
                self.gemini_parts.clone(),
            )));
            output.push(StreamEvent::Usage(
                super::usage(self.input, self.output).with_cache(self.cache_read, self.cache_write),
            ));
            if let Some(finish) = self.finish_reason {
                output.push(StreamEvent::Finished(finish));
            }
        }
        Ok(output)
    }

    pub fn finish(&self) -> Result<(), ModelError> {
        if !self.terminal {
            return Err(ModelError::transient(
                "Provider stream ended without its terminal event",
            ));
        }
        Ok(())
    }
}
