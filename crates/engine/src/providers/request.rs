//! Request bodies for the four HTTP+SSE protocols, and the Responses API
//! output parser used by its terminal stream event.
//!
//! Bodies are built for provider prompt caching. Anthropic gets its four
//! breakpoints (tools, the static system prompt, the newest system block and
//! the newest message); OpenAI requests carry a `prompt_cache_key` derived
//! from the conversation's stable prefix. Tool definitions are emitted in a
//! fixed order with no per-call content, and OpenAI functions are `strict`
//! wherever their schema already meets strict mode's rules.
use super::*;
use crate::model::{
    FinishReason, InlineImage, Message, NativeContent, Reasoning, Role, ToolCall, ToolSchema,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::json;

pub(super) fn invalid(message: impl Into<String>) -> ModelError {
    ModelError::InvalidRequest(message.into())
}

/// The call's arguments as a JSON object. Malformed arguments were already
/// answered with a tool error, so their replay is an empty object.
pub(super) fn arguments(call: &ToolCall) -> Value {
    serde_json::from_str::<Value>(&call.arguments)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

/// Rejects images the providers would refuse anyway, before any network use.
pub(super) fn check_image(image: &InlineImage) -> Result<(), ModelError> {
    if !matches!(
        image.media_type.as_str(),
        "image/png" | "image/jpeg" | "image/webp" | "image/gif"
    ) {
        return Err(invalid("Unsupported inline image media type"));
    }
    if image.data.len() > 8 * 1024 * 1024 || STANDARD.decode(&image.data).is_err() {
        return Err(invalid("Invalid or oversized base64 image"));
    }
    Ok(())
}

fn data_url(image: &InlineImage) -> Result<String, ModelError> {
    check_image(image)?;
    Ok(format!("data:{};base64,{}", image.media_type, image.data))
}

fn function(tool: &ToolSchema) -> Value {
    json!({"type":"function","name":tool.name,"description":tool.description,"parameters":tool.parameters,"strict":strict_compatible(&tool.parameters)})
}

/// Keywords OpenAI's strict function calling accepts in every schema node.
const STRICT_KEYWORDS: &[&str] = &[
    "type",
    "description",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "anyOf",
    "const",
];

/// Whether `schema` already meets strict mode's rules, so `strict: true`
/// changes nothing but the guarantee: an object root, only
/// [`STRICT_KEYWORDS`], and every object closed (`additionalProperties:
/// false`) with all of its properties required. Schemas with optional
/// parameters or bounds stay non-strict rather than being rewritten, since
/// strict mode would force the model to send every optional field.
pub(super) fn strict_compatible(schema: &Value) -> bool {
    fn node(schema: &Value) -> bool {
        let Some(fields) = schema.as_object() else {
            return false;
        };
        if !fields
            .keys()
            .all(|key| STRICT_KEYWORDS.contains(&key.as_str()))
        {
            return false;
        }
        let object = fields.get("type").and_then(Value::as_str) == Some("object")
            || fields.contains_key("properties");
        if object {
            let properties = match fields.get("properties") {
                Some(Value::Object(properties)) => properties.clone(),
                None => serde_json::Map::new(),
                Some(_) => return false,
            };
            let required: Vec<&str> = fields
                .get("required")
                .and_then(Value::as_array)
                .map(|names| names.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if fields.get("additionalProperties") != Some(&Value::Bool(false))
                || required.len() != properties.len()
                || !properties
                    .keys()
                    .all(|name| required.contains(&name.as_str()))
                || !properties.values().all(node)
            {
                return false;
            }
        }
        fields.get("items").is_none_or(node)
            && fields.get("anyOf").is_none_or(|options| {
                options
                    .as_array()
                    .is_some_and(|options| options.iter().all(node))
            })
    }
    schema.get("type").and_then(Value::as_str) == Some("object") && node(schema)
}

/// A `prompt_cache_key` that is the same for every request of one
/// conversation: a digest of the model, the system instructions and the
/// first conversation message, which is exactly the prefix the provider's
/// cache can match. It changes only when that prefix changes (a compaction
/// that replaces the start of history, a new day in the system prompt), when
/// the old cache entry could not be used anyway. No content leaves in clear.
fn prompt_cache_key(model: &str, messages: &[Message]) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(model.as_bytes());
    for message in messages.iter().filter(|m| m.role == Role::System) {
        digest.update([0]);
        digest.update(message.content.as_bytes());
    }
    if let Some(first) = messages.iter().find(|m| m.role != Role::System) {
        digest.update([1]);
        digest.update(first.content.as_bytes());
    }
    format!("xylo-{}", &hex::encode(digest.finalize())[..32])
}

/// Whether the endpoint routes by `prompt_cache_key`: every Responses
/// endpoint, and Chat Completions only on OpenAI itself, since local and
/// third-party Chat Completions servers may reject unknown fields.
fn routes_prompt_cache(provider: &NativeProvider) -> bool {
    match provider.endpoint.protocol {
        ProviderProtocol::Responses => true,
        ProviderProtocol::ChatCompletions => {
            provider.endpoint.base_url.host_str() == Some("api.openai.com")
        }
        _ => false,
    }
}

pub(super) fn body(
    provider: &NativeProvider,
    messages: &[Message],
    tools: &[ToolSchema],
    max_tokens: u32,
    reasoning: Reasoning,
) -> Result<Value, ModelError> {
    if messages.is_empty() {
        return Err(invalid("Provider request requires conversation messages"));
    }
    let model = provider.config.model.as_str();
    let mut body = match provider.endpoint.protocol {
        ProviderProtocol::ChatCompletions => chat_body(model, messages, tools, max_tokens)?,
        ProviderProtocol::Responses => responses_body(provider, messages, tools, max_tokens)?,
        ProviderProtocol::AnthropicMessages => anthropic_body(model, messages, tools, max_tokens)?,
        ProviderProtocol::GeminiGenerateContent => gemini_body(messages, tools, max_tokens)?,
        ProviderProtocol::BedrockConverse => {
            return Err(invalid("Use the native Bedrock provider"));
        }
    };
    if routes_prompt_cache(provider) {
        body["prompt_cache_key"] = json!(prompt_cache_key(model, messages));
    }
    // Apply only a capability-checked selection of reasoning effort.
    if let Some(effort) = reasoning_effort(&provider.config, reasoning) {
        match provider.endpoint.protocol {
            ProviderProtocol::ChatCompletions => body["reasoning_effort"] = json!(effort),
            ProviderProtocol::Responses => body["reasoning"] = json!({"effort": effort}),
            _ => {}
        }
    }
    Ok(body)
}

/// The effort to send: the user's capability-checked choice, or the lowest
/// effort the model supports.
fn reasoning_effort(config: &ProviderConfig, reasoning: Reasoning) -> Option<&str> {
    match reasoning {
        Reasoning::Configured => config.effective_reasoning_effort(),
        Reasoning::Lowest => {
            const ORDER: [&str; 6] = ["none", "minimal", "low", "medium", "high", "xhigh"];
            config
                .supported_reasoning_efforts()
                .into_iter()
                .min_by_key(|effort| {
                    ORDER
                        .iter()
                        .position(|known| known == effort)
                        .unwrap_or(ORDER.len())
                })
        }
    }
}

fn chat_body(
    model: &str,
    messages: &[Message],
    tools: &[ToolSchema],
    max_tokens: u32,
) -> Result<Value, ModelError> {
    let mut output = Vec::new();
    for message in messages {
        let role = match message.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        let content = match message.role {
            Role::User if !message.images.is_empty() => {
                let mut parts = vec![json!({"type":"text","text":message.content})];
                for image in &message.images {
                    parts.push(json!({"type":"image_url","image_url":{"url":data_url(image)?}}));
                }
                Value::Array(parts)
            }
            Role::Assistant if message.content.is_empty() => Value::Null,
            _ => json!(message.content),
        };
        let mut item = json!({"role":role,"content":content});
        if !message.tool_calls.is_empty() {
            item["tool_calls"] = Value::Array(
                message
                    .tool_calls
                    .iter()
                    .map(|call| json!({"id":call.id,"type":"function","function":{"name":call.name,"arguments":call.arguments}}))
                    .collect(),
            );
        }
        if let Some(id) = &message.tool_call_id {
            item["tool_call_id"] = json!(id);
        }
        if let Some(reasoning) = &message.reasoning {
            item["reasoning_content"] = json!(reasoning);
        }
        output.push(item);
    }
    let mut body = json!({"model":model,"messages":output,"stream":true,"stream_options":{"include_usage":true}});
    if !tools.is_empty() {
        body["tools"] = Value::Array(
            tools
                .iter()
                .map(|tool| json!({"type":"function","function":{"name":tool.name,"description":tool.description,"parameters":tool.parameters}}))
                .collect(),
        );
    }
    let modern = ["gpt-5", "gpt-6", "o1", "o3", "o4"]
        .iter()
        .any(|prefix| model.starts_with(prefix));
    body[if modern {
        "max_completion_tokens"
    } else {
        "max_tokens"
    }] = json!(max_tokens);
    Ok(body)
}

fn responses_body(
    provider: &NativeProvider,
    messages: &[Message],
    tools: &[ToolSchema],
    max_tokens: u32,
) -> Result<Value, ModelError> {
    let mut input = Vec::new();
    let mut instructions = Vec::new();
    for message in messages {
        match message.role {
            Role::System => instructions.push(message.content.clone()),
            Role::Tool => input.push(json!({"type":"function_call_output","call_id":message.tool_call_id,"output":message.content})),
            Role::User => {
                let mut content = vec![json!({"type":"input_text","text":message.content})];
                for image in &message.images {
                    content.push(json!({"type":"input_image","image_url":data_url(image)?}));
                }
                input.push(json!({"role":"user","content":content}));
            }
            Role::Assistant => {
                let replayed = match &message.native {
                    Some(NativeContent::Responses(items)) => {
                        input.extend(items.iter().cloned());
                        items
                            .iter()
                            .filter(|item| item["type"] == "function_call")
                            .filter_map(|item| item["call_id"].as_str())
                            .collect::<Vec<_>>()
                    }
                    _ => {
                        if !message.content.is_empty() {
                            input.push(json!({"role":"assistant","content":[{"type":"output_text","text":message.content}]}));
                        }
                        Vec::new()
                    }
                };
                // Every tool result needs its call in the input; add any call
                // the native replay items do not carry.
                for call in message
                    .tool_calls
                    .iter()
                    .filter(|call| !replayed.contains(&call.id.as_str()))
                {
                    let mut item = json!({"type":"function_call","call_id":call.id,"name":call.name,"arguments":call.arguments});
                    if provider.chatgpt_plan {
                        item["namespace"] = json!("xylo");
                    }
                    input.push(item);
                }
            }
        }
    }
    let mut body = json!({"model":provider.config.model,"input":input,"stream":true,"store":false,"include":["reasoning.encrypted_content"]});
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
    if !provider.chatgpt_plan {
        body["max_output_tokens"] = json!(max_tokens);
    }
    Ok(body)
}

fn anthropic_body(
    model: &str,
    messages: &[Message],
    tools: &[ToolSchema],
    max_tokens: u32,
) -> Result<Value, ModelError> {
    let mut system = Vec::new();
    let mut output = Vec::new();
    for message in messages {
        let mut content = Vec::new();
        match message.role {
            Role::System => {
                system.push(json!({"type":"text","text":message.content}));
                continue;
            }
            Role::Tool => content.push(json!({"type":"tool_result","tool_use_id":message.tool_call_id,"content":message.content})),
            Role::User => {
                content.push(json!({"type":"text","text":message.content}));
                for image in &message.images {
                    check_image(image)?;
                    content.push(json!({"type":"image","source":{"type":"base64","media_type":image.media_type,"data":image.data}}));
                }
            }
            Role::Assistant => {
                if let Some(NativeContent::Anthropic(blocks)) = &message.native {
                    content.extend(
                        blocks
                            .iter()
                            .filter(|block| {
                                matches!(
                                    block["type"].as_str(),
                                    Some("text" | "tool_use" | "thinking" | "redacted_thinking")
                                )
                            })
                            .cloned(),
                    );
                }
                if content.is_empty() {
                    if !message.content.is_empty() {
                        content.push(json!({"type":"text","text":message.content}));
                    }
                    for call in &message.tool_calls {
                        content.push(json!({"type":"tool_use","id":call.id,"name":call.name,"input":arguments(call)}));
                    }
                }
            }
        }
        let role = if message.role == Role::Assistant {
            "assistant"
        } else {
            "user"
        };
        if !content.is_empty() {
            push_parts(&mut output, "content", role, content);
        }
    }
    // Prompt-cache breakpoints (Anthropic allows four): tools, the static
    // system prompt, the newest system block (a compaction summary, when
    // there is one) and the newest message. Agent loops resend the same
    // prefix on every step, so later steps read it from cache instead of
    // reprocessing it, and a new summary leaves the static prompt cached.
    let ephemeral = json!({"type":"ephemeral"});
    if let Some(first) = system.first_mut() {
        first["cache_control"] = ephemeral.clone();
    }
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
    let mut body = json!({"model":model,"messages":output,"max_tokens":max_tokens,"stream":true});
    if !system.is_empty() {
        body["system"] = Value::Array(system);
    }
    if !tools.is_empty() {
        let mut definitions: Vec<Value> = tools
            .iter()
            .map(|tool| json!({"name":tool.name,"description":tool.description,"input_schema":tool.parameters}))
            .collect();
        if let Some(last) = definitions.last_mut() {
            last["cache_control"] = ephemeral;
        }
        body["tools"] = Value::Array(definitions);
    }
    Ok(body)
}

fn gemini_body(
    messages: &[Message],
    tools: &[ToolSchema],
    max_tokens: u32,
) -> Result<Value, ModelError> {
    let mut system = Vec::new();
    let mut contents = Vec::new();
    let mut names = std::collections::HashMap::new();
    for message in messages {
        for call in &message.tool_calls {
            names.insert(call.id.clone(), call.name.clone());
        }
        let mut parts = Vec::new();
        match message.role {
            Role::System => {
                system.push(json!({"text":message.content}));
                continue;
            }
            Role::Tool => {
                let id = message.tool_call_id.as_deref().unwrap_or("");
                let name = names
                    .get(id)
                    .ok_or_else(|| invalid("Gemini tool result has no matching function call"))?;
                let result: Value = serde_json::from_str(&message.content)
                    .unwrap_or_else(|_| json!({"result":message.content}));
                let response = if result.is_object() {
                    result
                } else {
                    json!({"result":result})
                };
                parts.push(json!({"functionResponse":{"id":id,"name":name,"response":response}}));
            }
            Role::User => {
                parts.push(json!({"text":message.content}));
                for image in &message.images {
                    check_image(image)?;
                    parts.push(
                        json!({"inlineData":{"mimeType":image.media_type,"data":image.data}}),
                    );
                }
            }
            Role::Assistant => {
                if let Some(NativeContent::Gemini(raw)) = &message.native {
                    parts = raw.clone();
                } else {
                    if !message.content.is_empty() {
                        parts.push(json!({"text":message.content}));
                    }
                    for call in &message.tool_calls {
                        parts.push(json!({"functionCall":{"id":call.id,"name":call.name,"args":arguments(call)}}));
                    }
                }
            }
        }
        if !parts.is_empty() {
            let role = if message.role == Role::Assistant {
                "model"
            } else {
                "user"
            };
            push_parts(&mut contents, "parts", role, parts);
        }
    }
    let mut body = json!({"contents":contents,"generationConfig":{"maxOutputTokens":max_tokens}});
    if !system.is_empty() {
        body["systemInstruction"] = json!({"parts":system});
    }
    if !tools.is_empty() {
        body["tools"] = json!([{"functionDeclarations":tools.iter().map(|tool|json!({"name":tool.name,"description":tool.description,"parametersJsonSchema":tool.parameters})).collect::<Vec<_>>()}]);
    }
    Ok(body)
}

/// Appends `parts` to the last message when it has the same role, so
/// protocols that require alternating roles receive one merged message.
fn push_parts(output: &mut Vec<Value>, field: &str, role: &str, parts: Vec<Value>) {
    if let Some(existing) = output
        .last_mut()
        .filter(|last| last["role"] == role)
        .and_then(|last| last[field].as_array_mut())
    {
        existing.extend(parts);
    } else {
        output.push(json!({"role":role, field: parts}));
    }
}

/// A complete Responses API response.
pub(super) struct ResponsesOutput {
    pub text: String,
    pub calls: Vec<ToolCall>,
    pub items: Vec<Value>,
    pub usage: ModelUsage,
    pub finish: FinishReason,
}

/// Parses the `response` object of a `response.completed` event.
pub(super) fn parse_responses(value: &Value) -> Result<ResponsesOutput, ModelError> {
    // An answer cut off by the output limit arrives as `incomplete` and is
    // kept, like a `length` finish in the other protocols.
    let truncated = value["status"] == "incomplete"
        && value
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str)
            == Some("max_output_tokens");
    if value["status"] != "completed" && !truncated {
        return Err(ModelError::permanent(format!(
            "Responses request did not complete ({})",
            value["status"].as_str().unwrap_or("unknown")
        )));
    }
    let items = match value["output"].as_array() {
        Some(items) => items.clone(),
        None if truncated => Vec::new(),
        None => return Err(ModelError::permanent("Responses output is missing")),
    };
    let mut text = String::new();
    let mut calls = Vec::new();
    for item in &items {
        match item["type"].as_str() {
            Some("message") => {
                for part in item["content"].as_array().into_iter().flatten() {
                    if part["type"] == "refusal" {
                        return Err(ModelError::permanent("Provider refused the request"));
                    }
                    if let Some(part) = part["text"].as_str() {
                        text.push_str(part);
                    }
                }
            }
            Some("function_call") => calls.push(ToolCall::new(
                item["call_id"].as_str().unwrap_or(""),
                item["name"].as_str().unwrap_or(""),
                item["arguments"].as_str().unwrap_or("{}"),
            )),
            _ => {}
        }
    }
    if calls
        .iter()
        .any(|call| call.id.is_empty() || call.name.is_empty())
    {
        return Err(ModelError::permanent(
            "Provider returned an incomplete function call",
        ));
    }
    Ok(ResponsesOutput {
        text,
        finish: if truncated {
            FinishReason::Length
        } else if calls.is_empty() {
            FinishReason::Stop
        } else {
            FinishReason::ToolCalls
        },
        calls,
        items,
        // `input_tokens` already includes the cached ones.
        usage: usage(
            value["usage"]["input_tokens"].as_u64().unwrap_or(0),
            value["usage"]["output_tokens"].as_u64().unwrap_or(0),
        )
        .with_cache(
            value
                .pointer("/usage/input_tokens_details/cached_tokens")
                .and_then(Value::as_u64),
            None,
        ),
    })
}
