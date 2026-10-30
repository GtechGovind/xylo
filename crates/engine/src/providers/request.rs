use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use hermes_core::{FunctionCall, MessageRole, ToolCall, UsageStats};
use serde_json::json;

pub(super) fn invalid(message: impl Into<String>) -> AgentError {
    AgentError::Config(message.into())
}
fn api_error(message: impl Into<String>) -> AgentError {
    AgentError::LlmApi(message.into())
}
fn args(call: &ToolCall) -> Result<Value, AgentError> {
    serde_json::from_str(&call.function.arguments)
        .map_err(|_| invalid("Tool arguments must be a JSON object"))
        .and_then(|value: Value| {
            if value.is_object() {
                Ok(value)
            } else {
                Err(invalid("Tool arguments must be a JSON object"))
            }
        })
}
fn function(tool: &ToolSchema) -> Value {
    json!({"type":"function","name":tool.name,"description":tool.description,"parameters":tool.parameters,"strict":false})
}
fn native_blocks(message: &Message, marker: &str) -> Option<Vec<Value>> {
    message
        .anthropic_content_blocks
        .as_ref()
        .and_then(|blocks| {
            let selected = blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some(marker))
                .filter_map(|block| block.get("item").cloned())
                .collect::<Vec<_>>();
            (!selected.is_empty()).then_some(selected)
        })
}

fn user_parts(message: &Message) -> Result<Vec<Value>, AgentError> {
    let text = message.content.as_deref().unwrap_or("");
    if let Some(parts) = text.strip_prefix("__hermes_acp_parts_json__:") {
        let parts: Vec<Value> =
            serde_json::from_str(parts).map_err(|_| invalid("Invalid multimodal message"))?;
        for part in &parts {
            if !matches!(
                part.get("type").and_then(Value::as_str),
                Some("text" | "image_url" | "input_image")
            ) {
                return Err(invalid(
                    "Only text and image inputs are supported by this provider adapter",
                ));
            }
            if part["type"] != "text" {
                image_url(part)?;
            }
        }
        return Ok(parts);
    }
    Ok(vec![json!({"type":"text","text":text})])
}
fn image_url(part: &Value) -> Result<&str, AgentError> {
    let url = part
        .pointer("/image_url/url")
        .or_else(|| part.get("image_url"))
        .or_else(|| part.get("url"))
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("Image input requires inline managed image data"))?;
    image_data(url)?;
    Ok(url)
}
fn image_data(url: &str) -> Result<(&str, &str), AgentError> {
    let (header, data) = url
        .split_once(",")
        .ok_or_else(|| invalid("Inline image data must use a base64 data URI"))?;
    let mime = header
        .strip_prefix("data:")
        .and_then(|header| header.strip_suffix(";base64"))
        .ok_or_else(|| invalid("Inline image data must use a base64 data URI"))?;
    if !matches!(
        mime,
        "image/png" | "image/jpeg" | "image/webp" | "image/gif"
    ) {
        return Err(invalid("Unsupported inline image media type"));
    }
    if data.len() > 8 * 1024 * 1024 || STANDARD.decode(data).is_err() {
        return Err(invalid("Invalid or oversized base64 image"));
    }
    Ok((mime, data))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn body(
    provider: &NativeProvider,
    messages: &[Message],
    tools: &[ToolSchema],
    max_tokens: Option<u32>,
    temperature: Option<f64>,
    model: Option<&str>,
    extra: Option<&Value>,
    stream: bool,
) -> Result<Value, AgentError> {
    if messages.is_empty() {
        return Err(invalid("Provider request requires conversation messages"));
    }
    let model = model.unwrap_or(&provider.config.model);
    let mut body = match provider.endpoint.protocol {
        ProviderProtocol::ChatCompletions => {
            let mut output = Vec::new();
            for message in messages {
                let role = match message.role {
                    MessageRole::System => "system",
                    MessageRole::User => "user",
                    MessageRole::Assistant => "assistant",
                    MessageRole::Tool => "tool",
                };
                let content = if message.role == MessageRole::User {
                    let parts = user_parts(message)?.into_iter().map(|part| {
                        if part["type"] == "input_image" {
                            Ok(json!({"type":"image_url","image_url":{"url":image_url(&part)?}}))
                        } else { Ok(part) }
                    }).collect::<Result<Vec<_>,AgentError>>()?;
                    if parts.len() == 1
                        && parts[0].get("type").and_then(Value::as_str) == Some("text")
                    {
                        parts[0]["text"].clone()
                    } else {
                        Value::Array(parts)
                    }
                } else {
                    message
                        .content
                        .as_ref()
                        .map_or(Value::Null, |text| json!(text))
                };
                let mut item = json!({"role":role,"content":content});
                if let Some(calls) = &message.tool_calls {
                    item["tool_calls"]=Value::Array(calls.iter().map(|call|json!({"id":call.id,"type":"function","function":{"name":call.function.name,"arguments":call.function.arguments}})).collect());
                }
                if let Some(id) = &message.tool_call_id {
                    item["tool_call_id"] = json!(id);
                }
                if let Some(reasoning) = &message.reasoning_content {
                    item["reasoning_content"] = json!(reasoning);
                }
                output.push(item);
            }
            let mut body = json!({"model":model,"messages":output,"stream":stream});
            if stream {
                body["stream_options"] = json!({"include_usage":true});
            }
            if !tools.is_empty() {
                body["tools"]=Value::Array(tools.iter().map(|tool|json!({"type":"function","function":{"name":tool.name,"description":tool.description,"parameters":tool.parameters}})).collect());
            }
            if let Some(max) = max_tokens {
                let modern = model.starts_with("gpt-5")
                    || model.starts_with("gpt-6")
                    || model.starts_with("o1")
                    || model.starts_with("o3")
                    || model.starts_with("o4");
                body[if modern {
                    "max_completion_tokens"
                } else {
                    "max_tokens"
                }] = json!(max);
            }
            body
        }
        ProviderProtocol::Responses => {
            let mut input = Vec::new();
            let mut instructions = Vec::new();
            for message in messages {
                if message.role == MessageRole::System {
                    if let Some(text) = &message.content {
                        instructions.push(text.clone());
                    }
                    continue;
                }
                if message.role == MessageRole::Assistant
                    && let Some(items) = native_blocks(message, "xylo_responses_item")
                {
                    input.extend(items);
                    continue;
                }
                if message.role == MessageRole::Tool {
                    input.push(json!({"type":"function_call_output","call_id":message.tool_call_id,"output":message.content.as_deref().unwrap_or("")}));
                    continue;
                }
                if message.role == MessageRole::User {
                    let mut content = Vec::new();
                    for part in user_parts(message)? {
                        if part["type"] == "text" {
                            content.push(json!({"type":"input_text","text":part["text"]}));
                        } else {
                            content
                                .push(json!({"type":"input_image","image_url":image_url(&part)?}));
                        }
                    }
                    input.push(json!({"role":"user","content":content}));
                }
                if message.role == MessageRole::Assistant {
                    if let Some(text) = &message.content
                        && !text.is_empty()
                    {
                        input.push(json!({"role":"assistant","content":[{"type":"output_text","text":text}]}));
                    }
                    for call in message.tool_calls.as_deref().unwrap_or(&[]) {
                        let mut item = json!({"type":"function_call","call_id":call.id,"name":call.function.name,"arguments":call.function.arguments});
                        if provider.chatgpt_plan {
                            item["namespace"] = json!("xylo");
                        }
                        input.push(item);
                    }
                }
            }
            let mut body = json!({"model":model,"input":input,"stream":stream,"store":false,"include":["reasoning.encrypted_content"]});
            if !instructions.is_empty() {
                body["instructions"] = json!(instructions.join("\n\n"));
            }
            if !tools.is_empty() {
                let functions = tools.iter().map(function).collect::<Vec<_>>();
                body["tools"] = if provider.chatgpt_plan {
                    json!([{"type":"namespace","name":"xylo","description":"Application tools with enforced permissions.","tools":functions}])
                } else {
                    Value::Array(functions)
                };
            }
            if !provider.chatgpt_plan
                && let Some(max) = max_tokens
            {
                body["max_output_tokens"] = json!(max);
            }
            body
        }
        ProviderProtocol::AnthropicMessages => {
            let mut system = Vec::new();
            let mut output = Vec::new();
            for message in messages {
                if message.role == MessageRole::System {
                    if let Some(text) = &message.content {
                        system.push(json!({"type":"text","text":text}));
                    }
                    continue;
                }
                let mut content = Vec::new();
                if message.role == MessageRole::Tool {
                    content.push(json!({"type":"tool_result","tool_use_id":message.tool_call_id,"content":message.content.as_deref().unwrap_or("")}));
                } else if message.role == MessageRole::User {
                    for part in user_parts(message)? {
                        if part["type"] == "text" {
                            content.push(part);
                        } else {
                            let url = image_url(&part)?;
                            let source = if url.starts_with("data:") {
                                let (mime, data) = image_data(url)?;
                                json!({"type":"base64","media_type":mime,"data":data})
                            } else {
                                if !url.starts_with("https://") {
                                    return Err(invalid("Remote images require HTTPS"));
                                }
                                json!({"type":"url","url":url})
                            };
                            content.push(json!({"type":"image","source":source}));
                        }
                    }
                } else {
                    if let Some(blocks) = &message.anthropic_content_blocks {
                        content.extend(
                            blocks
                                .iter()
                                .filter(|block| {
                                    matches!(
                                        block["type"].as_str(),
                                        Some(
                                            "text" | "tool_use" | "thinking" | "redacted_thinking"
                                        )
                                    )
                                })
                                .cloned(),
                        );
                    }
                    if content.is_empty() {
                        if let Some(text) = &message.content
                            && !text.is_empty()
                        {
                            content.push(json!({"type":"text","text":text}));
                        }
                        for call in message.tool_calls.as_deref().unwrap_or(&[]) {
                            content.push(json!({"type":"tool_use","id":call.id,"name":call.function.name,"input":args(call)?}));
                        }
                    }
                }
                let role = if message.role == MessageRole::Assistant {
                    "assistant"
                } else {
                    "user"
                };
                if !content.is_empty() {
                    push_parts(&mut output, role, content);
                }
            }
            // Prompt-cache breakpoints: tools, system policy and the newest
            // message. Agent loops resend the same prefix on every step, so
            // later steps read it from cache instead of reprocessing it.
            let ephemeral = json!({"type":"ephemeral"});
            if let Some(last) = system.last_mut() {
                last["cache_control"] = ephemeral.clone();
            }
            if let Some(block) = output
                .last_mut()
                .and_then(|message| message["content"].as_array_mut())
                .and_then(|content| content.last_mut())
                .filter(|block| {
                    matches!(
                        block["type"].as_str(),
                        Some("text" | "tool_result" | "image" | "tool_use")
                    )
                })
            {
                block["cache_control"] = ephemeral.clone();
            }
            let mut body = json!({"model":model,"messages":output,"max_tokens":max_tokens.unwrap_or(provider.config.output_tokens),"stream":stream});
            if !system.is_empty() {
                body["system"] = Value::Array(system);
            }
            if !tools.is_empty() {
                let mut definitions: Vec<Value> = tools.iter().map(|tool|json!({"name":tool.name,"description":tool.description,"input_schema":tool.parameters})).collect();
                if let Some(last) = definitions.last_mut() {
                    last["cache_control"] = ephemeral;
                }
                body["tools"] = Value::Array(definitions);
            }
            body
        }
        ProviderProtocol::GeminiGenerateContent => {
            let mut system = Vec::new();
            let mut contents = Vec::new();
            let mut names = std::collections::HashMap::new();
            for message in messages {
                for call in message.tool_calls.as_deref().unwrap_or(&[]) {
                    names.insert(call.id.clone(), call.function.name.clone());
                }
                if message.role == MessageRole::System {
                    if let Some(text) = &message.content {
                        system.push(json!({"text":text}));
                    }
                    continue;
                }
                let mut parts = Vec::new();
                if message.role == MessageRole::Tool {
                    let id = message.tool_call_id.as_deref().unwrap_or("");
                    let name = names.get(id).or(message.name.as_ref()).ok_or_else(|| {
                        invalid("Gemini tool result has no matching function call")
                    })?;
                    let text = message.content.as_deref().unwrap_or("");
                    let result: Value =
                        serde_json::from_str(text).unwrap_or_else(|_| json!({"result":text}));
                    parts.push(json!({"functionResponse":{"id":id,"name":name,"response":if result.is_object(){result}else{json!({"result":result})}}}));
                } else if message.role == MessageRole::User {
                    for part in user_parts(message)? {
                        if part["type"] == "text" {
                            parts.push(json!({"text":part["text"]}));
                        } else {
                            let (mime, data) = image_data(image_url(&part)?)?;
                            parts.push(json!({"inlineData":{"mimeType":mime,"data":data}}));
                        }
                    }
                } else if let Some(raw) = native_blocks(message, "xylo_gemini_part") {
                    parts = raw;
                } else {
                    if let Some(text) = &message.content
                        && !text.is_empty()
                    {
                        parts.push(json!({"text":text}));
                    }
                    for call in message.tool_calls.as_deref().unwrap_or(&[]) {
                        parts.push(json!({"functionCall":{"id":call.id,"name":call.function.name,"args":args(call)?}}));
                    }
                }
                if !parts.is_empty() {
                    let role = if message.role == MessageRole::Assistant {
                        "model"
                    } else {
                        "user"
                    };
                    push_gemini_parts(&mut contents, role, parts);
                }
            }
            let mut body = json!({"contents":contents,"generationConfig":{}});
            if !system.is_empty() {
                body["systemInstruction"] = json!({"parts":system});
            }
            if !tools.is_empty() {
                body["tools"] = json!([{"functionDeclarations":tools.iter().map(|tool|json!({"name":tool.name,"description":tool.description,"parametersJsonSchema":tool.parameters})).collect::<Vec<_>>()}]);
            }
            if let Some(max) = max_tokens {
                body["generationConfig"]["maxOutputTokens"] = json!(max);
            }
            body
        }
        ProviderProtocol::BedrockConverse => {
            return Err(invalid("Use the native Bedrock provider"));
        }
    };
    if let Some(temp) = temperature {
        if !temp.is_finite() {
            return Err(invalid("Temperature must be finite"));
        }
        if !provider.chatgpt_plan {
            if provider.endpoint.protocol == ProviderProtocol::GeminiGenerateContent {
                body["generationConfig"]["temperature"] = json!(temp);
            } else {
                body["temperature"] = json!(temp);
            }
        }
    }
    if let Some(extra) = extra.and_then(Value::as_object) {
        for (key, value) in extra {
            match (provider.endpoint.protocol, key.as_str()) {
                (
                    ProviderProtocol::ChatCompletions,
                    "response_format"
                    | "tool_choice"
                    | "parallel_tool_calls"
                    | "reasoning_effort"
                    | "stop",
                ) => {
                    body[key] = value.clone();
                }
                (ProviderProtocol::Responses, "text" | "reasoning" | "tool_choice") => {
                    body[key] = value.clone();
                }
                (
                    ProviderProtocol::AnthropicMessages,
                    "output_config" | "thinking" | "tool_choice" | "stop_sequences",
                ) => {
                    body[key] = value.clone();
                }
                (ProviderProtocol::GeminiGenerateContent, "generationConfig") => {
                    if let Some(fields) = value.as_object() {
                        for (key, value) in fields {
                            if key != "maxOutputTokens" {
                                body["generationConfig"][key] = value.clone();
                            }
                        }
                    }
                }
                (ProviderProtocol::GeminiGenerateContent, "response_format") => {
                    if value["type"] == "json_schema" {
                        body["generationConfig"]["responseMimeType"] = json!("application/json");
                        body["generationConfig"]["responseJsonSchema"] =
                            value["json_schema"]["schema"].clone();
                    } else if value["type"] == "json_object" {
                        body["generationConfig"]["responseMimeType"] = json!("application/json");
                    }
                }
                (ProviderProtocol::Responses, "response_format") => {
                    if value["type"] == "json_schema" {
                        let mut format = value["json_schema"].clone();
                        format["type"] = json!("json_schema");
                        body["text"]["format"] = format;
                    }
                }
                (ProviderProtocol::AnthropicMessages, "response_format")
                    if value["type"] == "json_schema" =>
                {
                    body["output_config"]["format"] =
                        json!({"type":"json_schema","schema":value["json_schema"]["schema"]});
                }
                _ => {}
            }
        }
    }
    // Apply only a capability-checked user selection. The selected model can be
    // overridden by the agent, so do not infer support from the original model.
    let mut reasoning_config = provider.config.clone();
    if reasoning_config.model != model {
        reasoning_config.reasoning_options = None;
        reasoning_config.default_reasoning_effort = None;
    }
    reasoning_config.model = model.to_owned();
    if let Some(effort) = reasoning_config.effective_reasoning_effort() {
        match provider.endpoint.protocol {
            ProviderProtocol::ChatCompletions => {
                body["reasoning_effort"] = json!(effort);
            }
            ProviderProtocol::Responses => {
                body["reasoning"] = json!({"effort": effort});
            }
            _ => {}
        }
    }
    Ok(body)
}
fn push_parts(output: &mut Vec<Value>, role: &str, parts: Vec<Value>) {
    if let Some(last) = output.last_mut()
        && last["role"] == role
    {
        last["content"].as_array_mut().unwrap().extend(parts);
    } else {
        output.push(json!({"role":role,"content":parts}));
    }
}
fn push_gemini_parts(output: &mut Vec<Value>, role: &str, parts: Vec<Value>) {
    if let Some(last) = output.last_mut()
        && last["role"] == role
    {
        last["parts"].as_array_mut().unwrap().extend(parts);
    } else {
        output.push(json!({"role":role,"parts":parts}));
    }
}

pub(super) fn usage(input: u64, output: u64) -> UsageStats {
    UsageStats {
        prompt_tokens: input,
        completion_tokens: output,
        total_tokens: input + output,
        estimated_cost: None,
    }
}
pub(super) fn parse_response(
    protocol: ProviderProtocol,
    value: &Value,
    model: &str,
) -> Result<LlmResponse, AgentError> {
    let mut message = Message::assistant("");
    let mut calls = Vec::new();
    let mut text = String::new();
    let mut thinking = String::new();
    let (finish, usage) = match protocol {
        ProviderProtocol::ChatCompletions => {
            let choice = value
                .pointer("/choices/0")
                .ok_or_else(|| api_error("Provider returned no completion choice"))?;
            let msg = &choice["message"];
            if msg["refusal"]
                .as_str()
                .is_some_and(|refusal| !refusal.is_empty())
            {
                return Err(api_error("Provider refused the request"));
            }
            text = msg["content"].as_str().unwrap_or("").into();
            for call in msg["tool_calls"].as_array().into_iter().flatten() {
                calls.push(ToolCall {
                    id: call["id"].as_str().unwrap_or("").into(),
                    function: FunctionCall {
                        name: call["function"]["name"].as_str().unwrap_or("").into(),
                        arguments: call["function"]["arguments"]
                            .as_str()
                            .unwrap_or("{}")
                            .into(),
                    },
                    extra_content: None,
                });
            }
            (
                match choice["finish_reason"].as_str() {
                    Some(reason @ ("stop" | "length" | "tool_calls")) => reason.to_owned(),
                    Some("content_filter") => {
                        return Err(api_error("Provider filtered the response"));
                    }
                    _ => return Err(api_error("Provider response has no terminal finish reason")),
                },
                Some(usage(
                    value["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
                    value["usage"]["completion_tokens"].as_u64().unwrap_or(0),
                )),
            )
        }
        ProviderProtocol::Responses => {
            if value["status"] != "completed" {
                return Err(api_error(format!(
                    "Responses request did not complete ({})",
                    value["status"].as_str().unwrap_or("unknown")
                )));
            }
            let output = value["output"]
                .as_array()
                .ok_or_else(|| api_error("Responses output is missing"))?;
            for item in output {
                match item["type"].as_str() {
                    Some("message") => {
                        for part in item["content"].as_array().into_iter().flatten() {
                            if part["type"] == "refusal" {
                                return Err(api_error("Provider refused the request"));
                            }
                            if let Some(part) = part["text"].as_str() {
                                text.push_str(part);
                            }
                        }
                    }
                    Some("function_call") => calls.push(ToolCall {
                        id: item["call_id"].as_str().unwrap_or("").into(),
                        function: FunctionCall {
                            name: item["name"].as_str().unwrap_or("").into(),
                            arguments: item["arguments"].as_str().unwrap_or("{}").into(),
                        },
                        extra_content: None,
                    }),
                    _ => {}
                }
            }
            message.anthropic_content_blocks = Some(
                output
                    .iter()
                    .map(|item| json!({"type":"xylo_responses_item","item":item}))
                    .collect(),
            );
            (
                if calls.is_empty() {
                    "stop"
                } else {
                    "tool_calls"
                }
                .into(),
                Some(usage(
                    value["usage"]["input_tokens"].as_u64().unwrap_or(0),
                    value["usage"]["output_tokens"].as_u64().unwrap_or(0),
                )),
            )
        }
        ProviderProtocol::AnthropicMessages => {
            let content = value["content"]
                .as_array()
                .ok_or_else(|| api_error("Anthropic content blocks are missing"))?;
            for part in content {
                match part["type"].as_str() {
                    Some("text") => text.push_str(part["text"].as_str().unwrap_or("")),
                    Some("thinking") => thinking.push_str(part["thinking"].as_str().unwrap_or("")),
                    Some("tool_use") => calls.push(ToolCall {
                        id: part["id"].as_str().unwrap_or("").into(),
                        function: FunctionCall {
                            name: part["name"].as_str().unwrap_or("").into(),
                            arguments: part["input"].to_string(),
                        },
                        extra_content: None,
                    }),
                    _ => {}
                }
            }
            message.anthropic_content_blocks = Some(content.clone());
            let finish = match value["stop_reason"].as_str() {
                Some("tool_use") => "tool_calls",
                Some("max_tokens") => "length",
                Some("refusal") => return Err(api_error("Anthropic refused the request")),
                Some("end_turn" | "stop_sequence") => "stop",
                _ => return Err(api_error("Anthropic response has no terminal stop reason")),
            };
            (
                finish.into(),
                Some(usage(
                    value["usage"]["input_tokens"].as_u64().unwrap_or(0)
                        + value["usage"]["cache_creation_input_tokens"]
                            .as_u64()
                            .unwrap_or(0)
                        + value["usage"]["cache_read_input_tokens"]
                            .as_u64()
                            .unwrap_or(0),
                    value["usage"]["output_tokens"].as_u64().unwrap_or(0),
                )),
            )
        }
        ProviderProtocol::GeminiGenerateContent => {
            let candidate = value.pointer("/candidates/0").ok_or_else(|| {
                api_error("Gemini returned no candidate (the prompt may have been blocked)")
            })?;
            let mut parts = candidate["content"]["parts"]
                .as_array()
                .ok_or_else(|| api_error("Gemini candidate has no content parts"))?
                .clone();
            for part in &mut parts {
                if let Some(part_text) = part["text"].as_str() {
                    if part["thought"] == true {
                        thinking.push_str(part_text);
                    } else {
                        text.push_str(part_text);
                    }
                }
                if part.get("functionCall").is_some() {
                    if part["functionCall"]["id"].as_str().is_none() {
                        part["functionCall"]["id"] = json!(uuid::Uuid::new_v4().to_string());
                    }
                    let call = &part["functionCall"];
                    calls.push(ToolCall {
                        id: call["id"]
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                        function: FunctionCall {
                            name: call["name"].as_str().unwrap_or("").into(),
                            arguments: call
                                .get("args")
                                .cloned()
                                .unwrap_or_else(|| json!({}))
                                .to_string(),
                        },
                        extra_content: part
                            .get("thoughtSignature")
                            .cloned()
                            .map(|signature| json!({"thoughtSignature":signature})),
                    });
                }
            }
            message.anthropic_content_blocks = Some(
                parts
                    .iter()
                    .map(|part| json!({"type":"xylo_gemini_part","item":part}))
                    .collect(),
            );
            let finish = match candidate["finishReason"].as_str() {
                Some("STOP") => {
                    if calls.is_empty() {
                        "stop"
                    } else {
                        "tool_calls"
                    }
                }
                Some("MAX_TOKENS") => "length",
                Some(reason) => {
                    return Err(api_error(format!("Gemini generation stopped ({reason})")));
                }
                None => return Err(api_error("Gemini response has no terminal finish reason")),
            };
            (
                finish.into(),
                Some(usage(
                    value["usageMetadata"]["promptTokenCount"]
                        .as_u64()
                        .unwrap_or(0),
                    value["usageMetadata"]["candidatesTokenCount"]
                        .as_u64()
                        .unwrap_or(0)
                        + value["usageMetadata"]["thoughtsTokenCount"]
                            .as_u64()
                            .unwrap_or(0),
                )),
            )
        }
        ProviderProtocol::BedrockConverse => {
            return Err(invalid("Use the native Bedrock provider"));
        }
    };
    if calls
        .iter()
        .any(|call| call.id.is_empty() || call.function.name.is_empty())
    {
        return Err(api_error("Provider returned an incomplete function call"));
    }
    message.content = (!text.is_empty()).then_some(text);
    message.reasoning_content = (!thinking.is_empty()).then_some(thinking);
    message.tool_calls = (!calls.is_empty()).then_some(calls);
    Ok(LlmResponse {
        message,
        usage,
        model: value["model"].as_str().unwrap_or(model).into(),
        finish_reason: Some(finish),
    })
}
