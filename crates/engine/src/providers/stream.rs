use super::*;
use hermes_core::{
    FunctionCall, FunctionCallDelta, Message, StreamDelta, ToolCall, ToolCallDelta, UsageStats,
};
use serde_json::json;
use std::collections::{BTreeMap, HashMap};

fn failure(message: impl Into<String>) -> AgentError {
    AgentError::LlmApi(message.into())
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
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Event>, AgentError> {
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
    pub fn finish(&self) -> Result<(), AgentError> {
        if !self.pending.is_empty() || !self.data.is_empty() {
            return Err(failure("Provider stream ended inside an SSE frame"));
        }
        Ok(())
    }
}
fn delta(
    content: Option<String>,
    tools: Option<Vec<ToolCallDelta>>,
    extra: Option<Value>,
) -> StreamChunk {
    StreamChunk {
        delta: Some(StreamDelta {
            content,
            tool_calls: tools,
            extra,
        }),
        finish_reason: None,
        usage: None,
    }
}
fn text(text: &str) -> StreamChunk {
    delta(Some(text.into()), None, None)
}
fn tool(
    index: u32,
    id: Option<String>,
    name: Option<String>,
    arguments: Option<String>,
) -> StreamChunk {
    delta(
        None,
        Some(vec![ToolCallDelta {
            index,
            id,
            function: Some(FunctionCallDelta { name, arguments }),
        }]),
        None,
    )
}
fn native(blocks: Vec<Value>) -> StreamChunk {
    delta(None, None, Some(json!({"xylo_content_blocks":blocks})))
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
    gemini_parts: Vec<Value>,
    input: u64,
    output: u64,
    finish_reason: Option<String>,
    text_emitted: bool,
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
            gemini_parts: Vec::new(),
            input: 0,
            output: 0,
            finish_reason: None,
            text_emitted: false,
        }
    }
    fn index(&mut self, native: u64) -> u32 {
        let next = self.tools.len() as u32;
        *self.tools.entry(native).or_insert(next)
    }
    pub fn event(&mut self, event: Event) -> Result<Vec<StreamChunk>, AgentError> {
        if event.data == "[DONE]" {
            if self.protocol != ProviderProtocol::ChatCompletions || !self.choice_finished {
                return Err(failure(
                    "Provider ended a stream without a terminal completion",
                ));
            }
            self.terminal = true;
            return Ok(Vec::new());
        }
        let value: Value = serde_json::from_str(&event.data)
            .map_err(|error| failure(format!("Invalid provider SSE JSON: {error}")))?;
        if value.get("error").is_some() || event.kind == "error" || value["type"] == "error" {
            return Err(failure(diagnostics::stream_error(
                &value,
                &self.error_redactions,
                "stream_error",
                self.request_id.as_deref(),
            )));
        }
        let mut output = Vec::new();
        match self.protocol {
            ProviderProtocol::ChatCompletions => {
                if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
                    output.push(StreamChunk {
                        delta: None,
                        finish_reason: None,
                        usage: Some(request::usage(
                            usage["prompt_tokens"].as_u64().unwrap_or(0),
                            usage["completion_tokens"].as_u64().unwrap_or(0),
                        )),
                    });
                }
                if let Some(choice) = value.pointer("/choices/0") {
                    let content = choice["delta"]["content"].as_str().map(str::to_owned);
                    let mut tools = Vec::new();
                    for call in choice["delta"]["tool_calls"]
                        .as_array()
                        .into_iter()
                        .flatten()
                    {
                        let raw = call["index"]
                            .as_u64()
                            .ok_or_else(|| failure("Streaming tool call has no index"))?;
                        let index = self.index(raw);
                        tools.push(ToolCallDelta {
                            index,
                            id: call["id"].as_str().map(str::to_owned),
                            function: Some(FunctionCallDelta {
                                name: call["function"]["name"].as_str().map(str::to_owned),
                                arguments: call["function"]["arguments"]
                                    .as_str()
                                    .map(str::to_owned),
                            }),
                        });
                    }
                    let extra = choice["delta"]["reasoning_content"]
                        .as_str()
                        .map(|thinking| json!({"thinking":thinking}));
                    if content.is_some() || !tools.is_empty() || extra.is_some() {
                        output.push(delta(content, (!tools.is_empty()).then_some(tools), extra));
                    }
                    if let Some(finish) = choice["finish_reason"].as_str() {
                        self.choice_finished = true;
                        output.push(StreamChunk {
                            delta: None,
                            finish_reason: Some(finish.into()),
                            usage: None,
                        });
                    }
                }
            }
            ProviderProtocol::Responses => {
                let kind = value["type"].as_str().unwrap_or(&event.kind);
                match kind {
                    "response.output_text.delta" => {
                        self.text_emitted = true;
                        output.push(text(
                            value["delta"]
                                .as_str()
                                .ok_or_else(|| failure("Responses text delta is invalid"))?,
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
                        if item["type"] == "function_call"
                            && !self.tool_arguments.contains_key(&raw)
                        {
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
                    "response.completed" => {
                        let response = &value["response"];
                        let parsed = request::parse_response(
                            self.protocol,
                            response,
                            response["model"].as_str().unwrap_or(""),
                        )?;
                        if !self.text_emitted
                            && let Some(content) = parsed.message.content
                        {
                            output.push(text(&content));
                        }
                        if self.tools.is_empty() {
                            for (index, call) in parsed
                                .message
                                .tool_calls
                                .unwrap_or_default()
                                .into_iter()
                                .enumerate()
                            {
                                output.push(tool(
                                    index as u32,
                                    Some(call.id),
                                    Some(call.function.name),
                                    Some(call.function.arguments),
                                ));
                            }
                        }
                        if let Some(blocks) = parsed.message.anthropic_content_blocks {
                            output.push(native(blocks));
                        }
                        output.push(StreamChunk {
                            delta: None,
                            finish_reason: parsed.finish_reason,
                            usage: parsed.usage,
                        });
                        self.terminal = true;
                    }
                    "response.failed" | "response.incomplete" | "error" => {
                        return Err(failure(diagnostics::stream_error(
                            &value,
                            &self.error_redactions,
                            kind,
                            self.request_id.as_deref(),
                        )));
                    }
                    _ => {}
                }
            }
            ProviderProtocol::AnthropicMessages => {
                let kind = value["type"].as_str().unwrap_or(&event.kind);
                match kind {
                    "message_start" => {
                        let usage = &value["message"]["usage"];
                        self.input = usage["input_tokens"].as_u64().unwrap_or(0)
                            + usage["cache_read_input_tokens"].as_u64().unwrap_or(0)
                            + usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
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
                                    output.push(text(content));
                                }
                            }
                            Some("thinking") => {}
                            Some("redacted_thinking") => {}
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
                            .get_mut(&raw)
                            .ok_or_else(|| failure("Anthropic delta arrived before its block"))?;
                        match change["type"].as_str() {
                            Some("text_delta") => {
                                let content = change["text"]
                                    .as_str()
                                    .ok_or_else(|| failure("Invalid Anthropic text delta"))?;
                                let prior = block["text"].as_str().unwrap_or("").to_owned();
                                block["text"] = json!(prior + content);
                                output.push(text(content));
                            }
                            Some("thinking_delta") => {
                                let thinking = change["thinking"]
                                    .as_str()
                                    .ok_or_else(|| failure("Invalid Anthropic thinking delta"))?;
                                let prior = block["thinking"].as_str().unwrap_or("").to_owned();
                                block["thinking"] = json!(prior + thinking);
                                output.push(delta(None, None, Some(json!({"thinking":thinking}))));
                            }
                            Some("signature_delta") => {
                                let prior = block["signature"].as_str().unwrap_or("").to_owned();
                                block["signature"] =
                                    json!(prior + change["signature"].as_str().unwrap_or(""));
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
                        if let Some(args) = self.tool_arguments.get(&raw) {
                            let input: Value = serde_json::from_str(args).map_err(|_| {
                                failure("Anthropic tool input ended before valid JSON")
                            })?;
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
                        self.finish_reason = value["delta"]["stop_reason"].as_str().map(|reason| {
                            match reason {
                                "end_turn" | "stop_sequence" => "stop",
                                "tool_use" => "tool_calls",
                                "max_tokens" => "length",
                                other => other,
                            }
                            .into()
                        });
                    }
                    "message_stop" => {
                        let finish = self.finish_reason.clone().ok_or_else(|| {
                            failure("Anthropic stream ended without a stop reason")
                        })?;
                        if !matches!(finish.as_str(), "stop" | "tool_calls" | "length") {
                            return Err(failure(format!(
                                "Anthropic generation stopped ({finish})"
                            )));
                        }
                        output.push(native(self.blocks.values().cloned().collect()));
                        output.push(StreamChunk {
                            delta: None,
                            finish_reason: Some(finish),
                            usage: Some(request::usage(self.input, self.output)),
                        });
                        self.terminal = true;
                    }
                    _ => {}
                }
            }
            ProviderProtocol::GeminiGenerateContent => {
                if let Some(usage) = value.get("usageMetadata") {
                    self.input = usage["promptTokenCount"].as_u64().unwrap_or(self.input);
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
                            if part["thought"] == true {
                                output.push(delta(None, None, Some(json!({"thinking":content}))));
                            } else {
                                output.push(text(content));
                            }
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
                        let finish = match reason {
                            "STOP" => {
                                if self.tools.is_empty() {
                                    "stop"
                                } else {
                                    "tool_calls"
                                }
                            }
                            "MAX_TOKENS" => "length",
                            other => {
                                return Err(failure(format!(
                                    "Gemini generation stopped ({other})"
                                )));
                            }
                        };
                        self.finish_reason = Some(finish.into());
                        self.terminal = true;
                    }
                } else if value.get("promptFeedback").is_some() {
                    return Err(failure("Gemini blocked the prompt"));
                }
                if self.terminal {
                    output.push(native(
                        self.gemini_parts
                            .iter()
                            .map(|part| json!({"type":"xylo_gemini_part","item":part}))
                            .collect(),
                    ));
                    output.push(StreamChunk {
                        delta: None,
                        finish_reason: self.finish_reason.clone(),
                        usage: Some(request::usage(self.input, self.output)),
                    });
                }
            }
            ProviderProtocol::BedrockConverse => {
                return Err(failure("Use the native Bedrock streaming decoder"));
            }
        }
        Ok(output)
    }
    pub fn finish(&self) -> Result<(), AgentError> {
        if !self.terminal {
            return Err(failure("Provider stream ended without its terminal event"));
        }
        Ok(())
    }
}

/// Used by the synchronous Responses facade when the account route mandates SSE.
#[derive(Default)]
pub(super) struct Collected {
    text: String,
    thinking: String,
    calls: BTreeMap<u32, ToolCall>,
    blocks: Option<Vec<Value>>,
    usage: Option<UsageStats>,
    finish: Option<String>,
}
impl Collected {
    pub fn push(&mut self, chunk: &StreamChunk) -> Result<(), AgentError> {
        if let Some(delta) = &chunk.delta {
            if let Some(text) = &delta.content {
                self.text.push_str(text);
            }
            if let Some(extra) = &delta.extra {
                if let Some(thinking) = extra["thinking"].as_str() {
                    self.thinking.push_str(thinking);
                }
                if let Some(blocks) = extra["xylo_content_blocks"].as_array() {
                    self.blocks = Some(blocks.clone());
                }
            }
            for delta in delta.tool_calls.as_deref().unwrap_or(&[]) {
                let call = self.calls.entry(delta.index).or_insert_with(|| ToolCall {
                    id: String::new(),
                    function: FunctionCall {
                        name: String::new(),
                        arguments: String::new(),
                    },
                    extra_content: None,
                });
                if let Some(id) = &delta.id {
                    call.id = id.clone();
                }
                if let Some(function) = &delta.function {
                    if let Some(name) = &function.name {
                        call.function.name = name.clone();
                    }
                    if let Some(args) = &function.arguments {
                        call.function.arguments.push_str(args);
                    }
                }
            }
        }
        if let Some(usage) = &chunk.usage {
            self.usage = Some(usage.clone());
        }
        if let Some(finish) = &chunk.finish_reason {
            self.finish = Some(finish.clone());
        }
        Ok(())
    }
    pub fn response(self, model: &str) -> Result<LlmResponse, AgentError> {
        if self.finish.is_none() {
            return Err(failure("Provider did not complete"));
        }
        let mut message = Message::assistant(self.text);
        message.reasoning_content = (!self.thinking.is_empty()).then_some(self.thinking);
        message.anthropic_content_blocks = self.blocks;
        let calls = self.calls.into_values().collect::<Vec<_>>();
        if !calls.is_empty() {
            message.tool_calls = Some(calls);
        }
        Ok(LlmResponse {
            message,
            usage: self.usage,
            model: model.into(),
            finish_reason: self.finish,
        })
    }
}
