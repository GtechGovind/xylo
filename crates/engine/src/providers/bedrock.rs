//! AWS Bedrock Converse adapter.
//!
//! Credentials come only from the vault (a bearer API key or explicit AWS
//! keys signed with SigV4); process environment and shared AWS files are
//! never consulted. Requests reject redirects and are bounded in size. The
//! binary event stream is decoded frame by frame with both CRCs validated,
//! must end with `messageStop` carrying a real terminal reason, and its exact
//! content blocks (signed reasoning, tool use) are kept for replay. Error
//! messages never reflect response bodies.
use super::*;
use crate::model::{FinishReason, Message, NativeContent, Role, StreamEvent, ToolSchema};
use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};

/// Largest accepted event-stream frame and buffered stream data.
const MAX_FRAME: usize = 8 * 1024 * 1024;
const DEFAULT_REGION: &str = "us-east-1";
const ANTHROPIC_BETAS: [&str; 3] = [
    "context-1m-2025-08-07",
    "interleaved-thinking-2025-05-14",
    "fine-grained-tool-streaming-2025-05-14",
];

/// Explicit AWS access keys. Debug output never shows them.
#[derive(Clone, PartialEq, Eq)]
struct AwsCredentials {
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
}

#[derive(Clone, PartialEq, Eq)]
enum BedrockAuth {
    Bearer(String),
    SigV4(AwsCredentials),
}

impl std::fmt::Debug for BedrockAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("BedrockAuth(<redacted>)")
    }
}

#[derive(Clone)]
pub(super) struct BedrockProvider {
    client: reqwest::Client,
    base_url: String,
    region: String,
    model: String,
    auth: BedrockAuth,
}

impl BedrockProvider {
    pub(super) fn new(config: &ProviderConfig, credential: ProviderCredential) -> Result<Self> {
        let normalized = normalize_provider(config)?;
        let auth = match credential {
            ProviderCredential::ApiKey(token) | ProviderCredential::Bearer(token) => {
                BedrockAuth::Bearer(token)
            }
            ProviderCredential::Aws {
                access_key_id,
                secret_access_key,
                session_token,
            } => BedrockAuth::SigV4(AwsCredentials {
                access_key_id,
                secret_access_key,
                session_token,
            }),
            ProviderCredential::None => {
                anyhow::bail!("Save a Bedrock API key or AWS access key before connecting")
            }
            ProviderCredential::ChatGpt { .. } => {
                anyhow::bail!("ChatGPT credentials can only be used with the public Responses API")
            }
        };
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(config.timeout_seconds.clamp(1, 3600)))
            .retry(reqwest::retry::never())
            .build()
            .context("Create Bedrock HTTP client")?;
        Ok(Self {
            client,
            base_url: normalized
                .base_url
                .as_str()
                .trim_end_matches('/')
                .to_owned(),
            region: normalized
                .aws_region
                .unwrap_or_else(|| DEFAULT_REGION.to_owned()),
            model: config.model.trim().to_owned(),
            auth,
        })
    }

    fn url(&self) -> String {
        format!(
            "{}/model/{}/converse-stream",
            self.base_url,
            percent_encode(&self.model, false)
        )
    }

    async fn send(&self, body: Vec<u8>) -> Result<reqwest::Response, ModelError> {
        let url = self.url();
        let beta = is_anthropic_model(&self.model).then(|| ANTHROPIC_BETAS.join(","));
        let headers = request_headers(
            HeaderRequest {
                method: "POST",
                url: &url,
                region: &self.region,
                body: &body,
                anthropic_beta: beta.as_deref(),
                now: Utc::now(),
            },
            &self.auth,
        )?;
        let mut request = self.client.post(url.as_str()).body(body);
        for (key, value) in headers {
            request = request.header(key, value);
        }
        let response = request.send().await.map_err(|error| ModelError::Provider {
            retryable: error.is_connect(),
            retry_after: None,
            message: format!("Bedrock request failed: {}", error.without_url()),
        })?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| retry_after_seconds(value, Utc::now().timestamp()))
            .map(Duration::from_secs);
        let payload = read_bounded(response).await.unwrap_or_default();
        Err(with_retry_after(
            map_error(status, &String::from_utf8_lossy(&payload)),
            retry_after,
        ))
    }
}

impl ModelProvider for BedrockProvider {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> EventStream {
        let provider = self.clone();
        Box::pin(async_stream::try_stream! {
            let body = converse_body(&provider.model, &request.messages, &request.tools, request.max_tokens)?;
            let body = serde_json::to_vec(&body)
                .map_err(|error| ModelError::Io(format!("Serialize Bedrock request: {error}")))?;
            let response = tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(ModelError::Interrupted),
                response = provider.send(body) => response,
            }?;
            let mut bytes = response.bytes_stream();
            let mut buffer = Vec::new();
            let mut decoder = StreamDecoder::default();
            loop {
                let chunk = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => Err(ModelError::Interrupted),
                    chunk = bytes.next() => Ok(chunk),
                }?;
                let Some(chunk) = chunk else { break };
                let chunk = chunk.map_err(|error| {
                    ModelError::transient(format!("Bedrock stream read failed: {}", error.without_url()))
                })?;
                if buffer.len().saturating_add(chunk.len()) > MAX_FRAME {
                    Err(ModelError::permanent("Bedrock stream frame exceeds 8 MiB"))?;
                }
                buffer.extend_from_slice(&chunk);
                while let Some(frame) = take_frame(&mut buffer)? {
                    if let Some(event) = decode_frame(&frame)? {
                        for event in decoder.event(&event)? {
                            yield event;
                        }
                    }
                }
            }
            if !buffer.is_empty() {
                Err(ModelError::transient(format!(
                    "Incomplete Bedrock event stream frame: {} trailing bytes",
                    buffer.len()
                )))?;
            }
            decoder.finish()?;
        })
    }
}

/// Converts Converse stream events into [`StreamEvent`]s while keeping the
/// exact content blocks for replay.
#[derive(Default)]
struct StreamDecoder {
    tool_indexes: HashMap<u64, u32>,
    blocks: BTreeMap<u64, Value>,
    tool_arguments: HashMap<u64, String>,
    stopped: bool,
}

impl StreamDecoder {
    fn tool_index(&mut self, native: u64) -> u32 {
        let next = self.tool_indexes.len() as u32;
        *self.tool_indexes.entry(native).or_insert(next)
    }

    fn event(&mut self, event: &Value) -> Result<Vec<StreamEvent>, ModelError> {
        let mut output = Vec::new();
        if let Some(start) = event.get("contentBlockStart") {
            let index = start["contentBlockIndex"].as_u64().unwrap_or(0);
            if let Some(tool) = start.pointer("/start/toolUse") {
                self.blocks.insert(
                    index,
                    json!({"toolUse":{"toolUseId":tool["toolUseId"],"name":tool["name"],"input":{}}}),
                );
                let index = self.tool_index(index);
                output.push(StreamEvent::ToolCall {
                    index,
                    id: tool["toolUseId"].as_str().map(str::to_owned),
                    name: tool["name"].as_str().map(str::to_owned),
                    arguments: None,
                });
            }
        }
        if let Some(delta) = event.get("contentBlockDelta") {
            let index = delta["contentBlockIndex"].as_u64().unwrap_or(0);
            let change = &delta["delta"];
            if let Some(text) = change["text"].as_str() {
                let block = self
                    .blocks
                    .entry(index)
                    .or_insert_with(|| json!({"text":""}));
                let prior = block["text"].as_str().unwrap_or("").to_owned();
                block["text"] = json!(prior + text);
                output.push(StreamEvent::Text(text.into()));
            }
            if let Some(reasoning) = change.get("reasoningContent") {
                let block = self.blocks.entry(index).or_insert_with(
                    || json!({"reasoningContent":{"reasoningText":{"text":"","signature":""}}}),
                );
                for key in ["text", "signature"] {
                    if let Some(text) = reasoning[key].as_str() {
                        let slot = &mut block["reasoningContent"]["reasoningText"][key];
                        let prior = slot.as_str().unwrap_or("").to_owned();
                        *slot = json!(prior + text);
                    }
                }
                if let Some(redacted) = reasoning.get("redactedContent") {
                    block["reasoningContent"] = json!({"redactedContent":redacted});
                }
                if let Some(text) = reasoning["text"].as_str() {
                    output.push(StreamEvent::Reasoning(text.into()));
                }
            }
            if let Some(input) = change.pointer("/toolUse/input") {
                let input = input
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| input.to_string());
                self.tool_arguments
                    .entry(index)
                    .or_default()
                    .push_str(&input);
                let index = self.tool_index(index);
                output.push(StreamEvent::ToolCall {
                    index,
                    id: None,
                    name: None,
                    arguments: Some(input),
                });
            }
        }
        if let Some(stop) = event.get("messageStop") {
            let finish = terminal_reason(stop["stopReason"].as_str())?;
            for (index, arguments) in &self.tool_arguments {
                // Empty input is an empty object; input cut off by the output
                // limit is kept as a call the loop answers with an error.
                let parsed = match stream::tool_input(arguments) {
                    Some(parsed) => parsed,
                    None if finish == FinishReason::Length => json!({}),
                    None => {
                        return Err(ModelError::permanent("Incomplete Bedrock tool arguments"));
                    }
                };
                if let Some(block) = self.blocks.get_mut(index) {
                    block["toolUse"]["input"] = parsed;
                }
            }
            self.stopped = true;
            output.push(StreamEvent::Native(NativeContent::Bedrock(
                self.blocks.values().cloned().collect(),
            )));
            output.push(StreamEvent::Finished(finish));
        }
        if let Some(usage) = event.pointer("/metadata/usage") {
            output.push(StreamEvent::Usage(super::usage(
                usage["inputTokens"].as_u64().unwrap_or(0),
                usage["outputTokens"].as_u64().unwrap_or(0),
            )));
        }
        Ok(output)
    }

    fn finish(&self) -> Result<(), ModelError> {
        if !self.stopped {
            return Err(ModelError::transient(
                "Bedrock stream ended without messageStop",
            ));
        }
        Ok(())
    }
}

/// Converse success requires a real terminal reason; an HTTP 200 or a
/// `messageStop` envelope alone does not establish an unfiltered completion.
fn terminal_reason(reason: Option<&str>) -> Result<FinishReason, ModelError> {
    match reason {
        Some("end_turn" | "stop_sequence") => Ok(FinishReason::Stop),
        Some("tool_use") => Ok(FinishReason::ToolCalls),
        Some("max_tokens") => Ok(FinishReason::Length),
        Some("content_filtered" | "guardrail_intervened") => {
            Err(ModelError::permanent("Bedrock filtered the response"))
        }
        _ => Err(ModelError::permanent(
            "Bedrock response has no valid terminal stop reason",
        )),
    }
}

fn converse_body(
    model: &str,
    messages: &[Message],
    tools: &[ToolSchema],
    max_tokens: u32,
) -> Result<Value, ModelError> {
    let mut system = Vec::new();
    let mut converted = Vec::new();
    for message in messages {
        match message.role {
            Role::System => {
                if !message.content.trim().is_empty() {
                    system.push(json!({"text": message.content}));
                }
            }
            Role::User => {
                let mut content = text_or_placeholder(&message.content);
                for image in &message.images {
                    request::check_image(image)?;
                    let format = image.media_type.trim_start_matches("image/");
                    content.push(json!({"image":{"format":format,"source":{"bytes":image.data}}}));
                }
                push_message(&mut converted, "user", content);
            }
            Role::Assistant => {
                if let Some(NativeContent::Bedrock(blocks)) = &message.native {
                    push_message(&mut converted, "assistant", blocks.clone());
                    continue;
                }
                let mut content = Vec::new();
                if !message.content.trim().is_empty() {
                    content.push(json!({"text": message.content}));
                }
                for call in &message.tool_calls {
                    content.push(json!({"toolUse":{"toolUseId":call.id,"name":call.name,"input":request::arguments(call)}}));
                }
                if content.is_empty() {
                    content.push(json!({"text": " "}));
                }
                push_message(&mut converted, "assistant", content);
            }
            Role::Tool => push_message(
                &mut converted,
                "user",
                vec![json!({"toolResult":{
                    "toolUseId": message.tool_call_id.clone().unwrap_or_default(),
                    "content": text_or_placeholder(&message.content),
                }})],
            ),
        }
    }
    // Converse requires the conversation to start and end with a user turn.
    if converted
        .first()
        .is_none_or(|message| message["role"] != "user")
    {
        converted.insert(0, json!({"role":"user","content":[{"text":" "}]}));
    }
    if converted
        .last()
        .is_some_and(|message| message["role"] != "user")
    {
        converted.push(json!({"role":"user","content":[{"text":" "}]}));
    }
    let mut body = json!({"messages": converted, "inferenceConfig": {"maxTokens": max_tokens}});
    if !system.is_empty() {
        body["system"] = Value::Array(system);
    }
    if !tools.is_empty() {
        let specs = tools
            .iter()
            .map(|tool| {
                let schema = Some(tool.parameters.clone())
                    .filter(|schema| schema.as_object().is_some_and(|fields| !fields.is_empty()))
                    .unwrap_or_else(|| json!({"type":"object","properties":{}}));
                json!({"toolSpec":{"name":tool.name,"description":tool.description,"inputSchema":{"json":schema}}})
            })
            .collect::<Vec<_>>();
        body["toolConfig"] = json!({"tools": specs});
    }
    if is_anthropic_model(model) {
        body["additionalModelRequestFields"] = json!({"anthropic_beta": ANTHROPIC_BETAS});
    }
    Ok(body)
}

fn text_or_placeholder(text: &str) -> Vec<Value> {
    if text.trim().is_empty() {
        vec![json!({"text": " "})]
    } else {
        vec![json!({"text": text})]
    }
}

fn push_message(messages: &mut Vec<Value>, role: &str, content: Vec<Value>) {
    if let Some(existing) = messages
        .last_mut()
        .filter(|last| last["role"] == role)
        .and_then(|last| last["content"].as_array_mut())
    {
        existing.extend(content);
    } else {
        messages.push(json!({"role": role, "content": content}));
    }
}

fn is_anthropic_model(model: &str) -> bool {
    let lower = model.trim().to_ascii_lowercase();
    lower.starts_with("anthropic.")
        || lower
            .split_once('.')
            .is_some_and(|(_, rest)| rest.starts_with("anthropic."))
}

/// Maps an HTTP or stream exception status to a typed error without
/// reflecting the response body.
fn map_error(status: u16, body: &str) -> ModelError {
    let payload = serde_json::from_str::<Value>(body).unwrap_or(Value::Null);
    let code = payload
        .get("__type")
        .or_else(|| payload.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .rsplit('#')
        .next()
        .unwrap_or("");
    let lower = body.to_ascii_lowercase();
    let message = format!("Bedrock API error HTTP {status}");
    if matches!(status, 401 | 403)
        || matches!(
            code,
            "AccessDeniedException" | "UnrecognizedClientException" | "InvalidSignatureException"
        )
    {
        ModelError::Authentication(format!("Bedrock authorization failed (HTTP {status})"))
    } else if status == 400
        && [
            "input is too long",
            "maximum number of input tokens",
            "maximum context length",
            "too many tokens",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        ModelError::ContextOverflow(format!(
            "{message}: the conversation exceeds the model's context window"
        ))
    } else if status == 429 || code == "ThrottlingException" {
        ModelError::RateLimited {
            retry_after: None,
            message,
        }
    } else {
        ModelError::Provider {
            retryable: matches!(status, 408 | 424 | 500..=599),
            retry_after: None,
            message,
        }
    }
}

fn with_retry_after(mut error: ModelError, delay: Option<Duration>) -> ModelError {
    if let ModelError::RateLimited { retry_after, .. } | ModelError::Provider { retry_after, .. } =
        &mut error
    {
        *retry_after = delay;
    }
    error
}

/// One decoded AWS event-stream frame.
#[derive(Debug, PartialEq, Eq)]
struct Frame {
    headers: HashMap<String, String>,
    payload: Vec<u8>,
}

/// Removes one complete frame from `buffer`, validating its length and both
/// CRCs. Returns `None` until a whole frame is buffered.
fn take_frame(buffer: &mut Vec<u8>) -> Result<Option<Frame>, ModelError> {
    if buffer.len() < 12 {
        return Ok(None);
    }
    let total = read_u32(&buffer[0..4]) as usize;
    let headers_len = read_u32(&buffer[4..8]) as usize;
    if total < 16 {
        return Err(ModelError::permanent(format!(
            "Invalid Bedrock event stream frame length: {total}"
        )));
    }
    if total > MAX_FRAME {
        return Err(ModelError::permanent(
            "Bedrock event stream frame exceeds 8 MiB",
        ));
    }
    if total > buffer.len() {
        return Ok(None);
    }
    if headers_len > total - 16 {
        return Err(ModelError::permanent(format!(
            "Invalid Bedrock event stream headers length: {headers_len}"
        )));
    }
    let frame: Vec<u8> = buffer.drain(..total).collect();
    if read_u32(&frame[8..12]) != crc32(&frame[..8]) {
        return Err(ModelError::permanent(
            "Invalid Bedrock event stream prelude checksum",
        ));
    }
    if read_u32(&frame[total - 4..]) != crc32(&frame[..total - 4]) {
        return Err(ModelError::permanent(
            "Invalid Bedrock event stream message checksum",
        ));
    }
    let headers_end = 12 + headers_len;
    Ok(Some(Frame {
        headers: parse_headers(&frame[12..headers_end])?,
        payload: frame[headers_end..total - 4].to_vec(),
    }))
}

const EXCEPTIONS: [(&str, u16); 6] = [
    ("validationException", 400),
    ("throttlingException", 429),
    ("modelTimeoutException", 408),
    ("modelStreamErrorException", 424),
    ("serviceUnavailableException", 503),
    ("internalServerException", 500),
];

/// The frame's event as `{eventType: payload}`, or an error for exceptions.
fn decode_frame(frame: &Frame) -> Result<Option<Value>, ModelError> {
    if frame.payload.is_empty() {
        return Ok(None);
    }
    let payload: Value = serde_json::from_slice(&frame.payload).map_err(|error| {
        ModelError::permanent(format!("Bedrock event stream JSON parse failed: {error}"))
    })?;
    let message_type = frame.headers.get(":message-type").map(String::as_str);
    let event_type = frame
        .headers
        .get(":event-type")
        .or_else(|| frame.headers.get(":exception-type"))
        .map(String::as_str);
    let status = EXCEPTIONS
        .iter()
        .find(|(name, _)| event_type == Some(*name) || payload.get(*name).is_some())
        .map(|(_, status)| *status);
    if message_type == Some("exception")
        || event_type.is_some_and(|event| event.ends_with("Exception"))
        || status.is_some()
    {
        return Err(map_error(status.unwrap_or(500), &payload.to_string()));
    }
    const EVENTS: [&str; 6] = [
        "messageStart",
        "contentBlockStart",
        "contentBlockDelta",
        "contentBlockStop",
        "messageStop",
        "metadata",
    ];
    if EVENTS.iter().any(|key| payload.get(*key).is_some()) {
        return Ok(Some(payload));
    }
    Ok(Some(match event_type.filter(|event| !event.is_empty()) {
        Some(event) => json!({ event: payload }),
        None => payload,
    }))
}

fn parse_headers(raw: &[u8]) -> Result<HashMap<String, String>, ModelError> {
    let malformed = || ModelError::permanent("Malformed Bedrock event stream header");
    let take = |offset: &mut usize, len: usize| -> Result<&[u8], ModelError> {
        let bytes = raw.get(*offset..*offset + len).ok_or_else(malformed)?;
        *offset += len;
        Ok(bytes)
    };
    let mut headers = HashMap::new();
    let mut offset = 0;
    while offset < raw.len() {
        let name_len = take(&mut offset, 1)?[0] as usize;
        if name_len == 0 {
            return Err(malformed());
        }
        let name = std::str::from_utf8(take(&mut offset, name_len)?)
            .map_err(|_| malformed())?
            .to_owned();
        let value = match take(&mut offset, 1)?[0] {
            0 => "true".to_owned(),
            1 => "false".to_owned(),
            2 => i8::from_be_bytes([take(&mut offset, 1)?[0]]).to_string(),
            3 => i16::from_be_bytes(take(&mut offset, 2)?.try_into().map_err(|_| malformed())?)
                .to_string(),
            4 => i32::from_be_bytes(take(&mut offset, 4)?.try_into().map_err(|_| malformed())?)
                .to_string(),
            5 | 8 => i64::from_be_bytes(take(&mut offset, 8)?.try_into().map_err(|_| malformed())?)
                .to_string(),
            6 => {
                let len =
                    u16::from_be_bytes(take(&mut offset, 2)?.try_into().map_err(|_| malformed())?);
                hex::encode(take(&mut offset, len as usize)?)
            }
            7 => {
                let len =
                    u16::from_be_bytes(take(&mut offset, 2)?.try_into().map_err(|_| malformed())?);
                std::str::from_utf8(take(&mut offset, len as usize)?)
                    .map_err(|_| malformed())?
                    .to_owned()
            }
            9 => hex::encode(take(&mut offset, 16)?),
            other => {
                return Err(ModelError::permanent(format!(
                    "Unsupported Bedrock event stream header value type: {other}"
                )));
            }
        };
        headers.insert(name, value);
    }
    Ok(headers)
}

fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// CRC-32 (IEEE), as used by the AWS event-stream encoding.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

#[derive(Clone, Copy)]
struct HeaderRequest<'a> {
    method: &'a str,
    url: &'a str,
    region: &'a str,
    body: &'a [u8],
    anthropic_beta: Option<&'a str>,
    now: DateTime<Utc>,
}

fn request_headers(
    request: HeaderRequest<'_>,
    auth: &BedrockAuth,
) -> Result<BTreeMap<String, String>, ModelError> {
    let mut headers = BTreeMap::new();
    headers.insert("accept".to_owned(), "application/json".to_owned());
    headers.insert("content-type".to_owned(), "application/json".to_owned());
    if let Some(beta) = request.anthropic_beta {
        headers.insert("anthropic-beta".to_owned(), beta.to_owned());
    }
    match auth {
        BedrockAuth::Bearer(token) => {
            headers.insert("authorization".to_owned(), format!("Bearer {token}"));
            Ok(headers)
        }
        BedrockAuth::SigV4(credentials) => sign(request, credentials, headers),
    }
}

/// AWS Signature Version 4 for the `bedrock` service. As for every service
/// except S3, the canonical URI encodes the already-encoded path again.
fn sign(
    request: HeaderRequest<'_>,
    credentials: &AwsCredentials,
    mut headers: BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, ModelError> {
    let url = url::Url::parse(request.url)
        .map_err(|error| ModelError::InvalidRequest(format!("Invalid Bedrock URL: {error}")))?;
    let host = url
        .host_str()
        .ok_or_else(|| ModelError::InvalidRequest("Bedrock URL has no host".into()))?;
    let amz_date = request.now.format("%Y%m%dT%H%M%SZ").to_string();
    let short_date = request.now.format("%Y%m%d").to_string();
    let payload_hash = hex::encode(Sha256::digest(request.body));
    headers.insert(
        "host".to_owned(),
        url.port()
            .map_or_else(|| host.to_owned(), |port| format!("{host}:{port}")),
    );
    headers.insert("x-amz-date".to_owned(), amz_date.clone());
    headers.insert("x-amz-content-sha256".to_owned(), payload_hash.clone());
    if let Some(token) = credentials
        .session_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
    {
        headers.insert("x-amz-security-token".to_owned(), token.to_owned());
    }
    let canonical_headers = headers
        .iter()
        .map(|(key, value)| {
            format!(
                "{key}:{}\n",
                value.split_whitespace().collect::<Vec<_>>().join(" ")
            )
        })
        .collect::<String>();
    let signed_headers = headers.keys().cloned().collect::<Vec<_>>().join(";");
    let canonical_uri = url
        .path()
        .split('/')
        .map(|segment| percent_encode(segment, false))
        .collect::<Vec<_>>()
        .join("/");
    let mut query = url
        .query_pairs()
        .map(|(key, value)| {
            format!(
                "{}={}",
                percent_encode(&key, true),
                percent_encode(&value, true)
            )
        })
        .collect::<Vec<_>>();
    query.sort();
    let canonical_request = format!(
        "{}\n{}\n{}\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
        request.method,
        if canonical_uri.is_empty() {
            "/"
        } else {
            &canonical_uri
        },
        query.join("&"),
    );
    let scope = format!("{short_date}/{}/bedrock/aws4_request", request.region);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex::encode(Sha256::digest(canonical_request.as_bytes()))
    );
    let mut key = [b"AWS4".as_slice(), credentials.secret_access_key.as_bytes()].concat();
    for part in [
        short_date.as_bytes(),
        request.region.as_bytes(),
        b"bedrock",
        b"aws4_request",
    ] {
        key = hmac(&key, part)?;
    }
    let signature = hex::encode(hmac(&key, string_to_sign.as_bytes())?);
    headers.insert(
        "authorization".to_owned(),
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            credentials.access_key_id
        ),
    );
    Ok(headers)
}

fn hmac(key: &[u8], value: &[u8]) -> Result<Vec<u8>, ModelError> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key)
        .map_err(|error| ModelError::Io(format!("SigV4 HMAC init failed: {error}")))?;
    mac.update(value);
    Ok(mac.finalize().into_bytes().to_vec())
}

/// RFC 3986 unreserved-character encoding; `strict` also encodes `~`.
fn percent_encode(input: &str, strict: bool) -> String {
    let mut out = String::new();
    for &byte in input.as_bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.')
            || (!strict && byte == b'~')
        {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
pub(super) mod tests;
