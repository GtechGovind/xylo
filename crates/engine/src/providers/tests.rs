//! HTTP fixtures qualify wire behavior, not acceptance by an external account.
use super::*;
use hermes_core::MessageRole;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

struct Recorded {
    line: String,
    headers: String,
    body: Value,
}
async fn fixture(
    payload: Vec<u8>,
    status: u16,
    content_type: &'static str,
) -> (String, tokio::task::JoinHandle<Recorded>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        socket.set_nodelay(true).unwrap();
        let mut socket = BufReader::new(socket);
        let mut first = String::new();
        socket.read_line(&mut first).await.unwrap();
        let mut headers = String::new();
        let mut length = 0;
        loop {
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse::<usize>().unwrap();
            }
            headers.push_str(&line);
            if line == "\r\n" {
                break;
            }
        }
        assert!(length < 2 * 1024 * 1024);
        let mut body = vec![0; length];
        socket.read_exact(&mut body).await.unwrap();
        let header = format!(
            "HTTP/1.1 {status} Fixture\r\nContent-Type: {content_type}\r\nx-request-id: req_transport_fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        );
        socket.get_mut().write_all(header.as_bytes()).await.unwrap();
        for chunk in payload.chunks(3) {
            if socket.get_mut().write_all(chunk).await.is_err() {
                break;
            }
            tokio::task::yield_now().await;
        }
        Recorded {
            line: first,
            headers: headers.to_lowercase(),
            body: serde_json::from_slice(&body).unwrap(),
        }
    });
    (format!("http://{address}"), server)
}
fn config(family: ApiFamily, endpoint: String) -> ProviderConfig {
    ProviderConfig {
        family,
        endpoint,
        model: "fixture-model".into(),
        ..Default::default()
    }
}
fn provider(family: ApiFamily, endpoint: String) -> NativeProvider {
    NativeProvider::new(
        config(family, endpoint),
        ProviderCredential::ApiKey("fixture-key".into()),
    )
    .unwrap()
}
fn tool_schema() -> ToolSchema {
    serde_json::from_value(json!({"name":"lookup","description":"Look up a fixture.","parameters":{"type":"object","properties":{"q":{"type":"string"}},"required":["q"],"additionalProperties":false}})).unwrap()
}
fn sse(events: &[Value]) -> Vec<u8> {
    events
        .iter()
        .map(|event| format!("data: {event}\r\n\r\n"))
        .collect::<String>()
        .into_bytes()
}
async fn collect(
    provider: &dyn LlmProvider,
    messages: &[Message],
    extra: Option<&Value>,
) -> Result<LlmResponse, AgentError> {
    let mut stream =
        provider.chat_completion_stream(messages, &[tool_schema()], Some(100), None, None, extra);
    let mut collected = stream::Collected::default();
    while let Some(chunk) = stream.next().await {
        collected.push(&chunk?)?;
    }
    collected.response("fixture-model")
}
fn image_message() -> Message {
    Message::user(format!(
        "__hermes_acp_parts_json__:{}",
        json!([{"type":"text","text":"What is shown?"},{"type":"image_url","image_url":{"url":"data:image/png;base64,aGVsbG8="}}])
    ))
}

#[tokio::test]
async fn managed_image_refs_rehydrate_into_actual_streamed_images_for_every_api_family() {
    use crate::attachments::{AttachmentProvider, user_message};
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use xylo_core::{AttachmentSource, PermissionMode, TurnRequest};
    use xylo_services::attachments::AttachmentStore;
    let directory = tempfile::tempdir().unwrap();
    let assets = AttachmentStore::new(directory.path().join("artifacts")).unwrap();
    let session = uuid::Uuid::new_v4();
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(24, 16)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    let attachment = assets
        .import_bytes(
            session,
            "actual.png",
            png.get_ref(),
            AttachmentSource::Picker,
            &tokio_util::sync::CancellationToken::new(),
        )
        .unwrap();
    let expected = assets
        .read_provider_image(session, &attachment)
        .unwrap()
        .bytes;
    for family in [
        ApiFamily::OpenAiChat,
        ApiFamily::OpenAiResponses,
        ApiFamily::Anthropic,
        ApiFamily::Gemini,
        ApiFamily::Azure,
        ApiFamily::Bedrock,
        ApiFamily::Vertex,
    ] {
        let mut payload = match family {
            ApiFamily::OpenAiChat | ApiFamily::Azure => {
                let mut bytes = sse(&[
                    json!({"choices":[{"delta":{"content":"Seen fixture image"},"finish_reason":"stop"}]}),
                ]);
                bytes.extend_from_slice(b"data: [DONE]\r\n\r\n");
                bytes
            }
            ApiFamily::OpenAiResponses => sse(&[
                json!({"type":"response.output_text.delta","delta":"Seen fixture image"}),
                json!({"type":"response.completed","response":{"status":"completed","model":"fixture-model","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Seen fixture image"}]}],"usage":{"input_tokens":3,"output_tokens":3}}}),
            ]),
            ApiFamily::Anthropic => sse(&[
                json!({"type":"message_start","message":{"usage":{"input_tokens":3}}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Seen fixture image"}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),
                json!({"type":"message_stop"}),
            ]),
            ApiFamily::Gemini | ApiFamily::Vertex => sse(&[
                json!({"candidates":[{"content":{"parts":[{"text":"Seen fixture image"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":3}}),
            ]),
            ApiFamily::Bedrock => {
                let mut bytes = Vec::new();
                for (kind, data) in [
                    ("messageStart", json!({"role":"assistant"})),
                    (
                        "contentBlockDelta",
                        json!({"contentBlockIndex":0,"delta":{"text":"Seen fixture image"}}),
                    ),
                    ("messageStop", json!({"stopReason":"end_turn"})),
                    (
                        "metadata",
                        json!({"usage":{"inputTokens":3,"outputTokens":3,"totalTokens":6}}),
                    ),
                ] {
                    bytes.extend(aws_frame(kind, data));
                }
                bytes
            }
        };
        let content_type = if family == ApiFamily::Bedrock {
            "application/vnd.amazon.eventstream"
        } else {
            "text/event-stream"
        };
        let (endpoint, server) = fixture(std::mem::take(&mut payload), 200, content_type).await;
        let endpoint = if family == ApiFamily::Vertex {
            format!("{endpoint}/v1/projects/fixture/locations/us-central1/publishers/google")
        } else {
            endpoint
        };
        let provider_config = config(family.clone(), endpoint);
        let inner = build(&provider_config, Some("fixture-key".into())).unwrap();
        let wrapped = AttachmentProvider::new(
            inner,
            assets.clone(),
            session,
            tokio_util::sync::CancellationToken::new(),
        );
        let turn = TurnRequest {
            session_id: session,
            turn_id: uuid::Uuid::new_v4(),
            prompt: "Describe this managed image".into(),
            workspace: directory.path().into(),
            provider: provider_config,
            permissions: PermissionMode::ReadOnly,
            attachments: vec![attachment.clone()],
        };
        let message = user_message(&turn).unwrap();
        let durable = serde_json::to_string(&message).unwrap();
        assert!(!durable.contains("base64"));
        assert!(!durable.contains("data:image"));
        let response = collect(&wrapped, std::slice::from_ref(&message), None)
            .await
            .unwrap();
        assert_eq!(
            response.message.content.as_deref(),
            Some("Seen fixture image")
        );
        assert_eq!(serde_json::to_string(&message).unwrap(), durable);
        let body = server.await.unwrap().body;
        let data = match family {
            ApiFamily::OpenAiChat | ApiFamily::Azure => {
                body["messages"][0]["content"][1]["image_url"]["url"]
                    .as_str()
                    .unwrap()
                    .split_once(',')
                    .unwrap()
                    .1
            }
            ApiFamily::OpenAiResponses => {
                body["input"][0]["content"][1]["image_url"]
                    .as_str()
                    .unwrap()
                    .split_once(',')
                    .unwrap()
                    .1
            }
            ApiFamily::Anthropic => body["messages"][0]["content"][1]["source"]["data"]
                .as_str()
                .unwrap(),
            ApiFamily::Gemini | ApiFamily::Vertex => body["contents"][0]["parts"][1]["inlineData"]
                ["data"]
                .as_str()
                .unwrap(),
            ApiFamily::Bedrock => body["messages"][0]["content"][1]["image"]["source"]["bytes"]
                .as_str()
                .unwrap(),
        };
        assert_eq!(
            STANDARD.decode(data).unwrap(),
            expected,
            "image bytes for {family:?}"
        );
    }
}

#[tokio::test]
async fn remote_image_urls_are_rejected_without_http_dispatch_and_selected_model_failure_is_explicit()
 {
    let native = provider(ApiFamily::OpenAiChat, "http://127.0.0.1:1".into());
    let message = Message::user(format!(
        "__hermes_acp_parts_json__:{}",
        json!([{"type":"image_url","image_url":{"url":"https://outside.invalid/private.png"}}])
    ));
    let error = native
        .chat_completion(&[message], &[], Some(100), None, None, None)
        .await
        .err()
        .unwrap();
    assert!(matches!(error, AgentError::Config(_)));
    let (endpoint, server) = fixture(
        json!({"error":{"code":"unsupported_image"}})
            .to_string()
            .into_bytes(),
        400,
        "application/json",
    )
    .await;
    let error = provider(ApiFamily::OpenAiChat, endpoint)
        .chat_completion(&[image_message()], &[], Some(100), None, None, None)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("chosen model supports image input"));
    assert!(error.contains("no other model was selected"));
    server.await.unwrap();
}

#[test]
fn sse_framing_preserves_utf8_across_every_byte_and_multiline_data() {
    let payload = "event: fixture\r\ndata: {\"text\":\r\ndata: \"हैलो 🦀\"}\r\n\r\n";
    let mut decoder = stream::SseDecoder::default();
    let mut events = Vec::new();
    for byte in payload.as_bytes() {
        events.extend(decoder.feed(&[*byte]).unwrap());
    }
    decoder.finish().unwrap();
    assert_eq!(events.len(), 1);
    let value: Value = serde_json::from_str(&events[0].data).unwrap();
    assert_eq!(value["text"], "हैलो 🦀");
    let mut partial = stream::SseDecoder::default();
    partial.feed(b"data: {\"text\":").unwrap();
    assert!(partial.finish().is_err());
}

#[tokio::test]
async fn chat_and_azure_use_correct_paths_headers_images_schema_and_tool_fragments() {
    for family in [ApiFamily::OpenAiChat, ApiFamily::Azure] {
        let mut payload = sse(&[
            json!({"choices":[{"delta":{"content":"हैलो 🦀"},"finish_reason":null}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":4,"id":"call-fixture","function":{"name":"lookup","arguments":"{\"q\":"}}]},"finish_reason":null}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":4,"function":{"arguments":"\"done\"}"}}]},"finish_reason":"tool_calls"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":9,"completion_tokens":4}}),
        ]);
        payload.extend_from_slice(b"data: [DONE]\r\n\r\n");
        let (endpoint, server) = fixture(payload, 200, "text/event-stream").await;
        let native = provider(family.clone(), endpoint);
        let extra = json!({"response_format":{"type":"json_schema","json_schema":{"name":"answer","schema":{"type":"object"}}}});
        let response = collect(&native, &[image_message()], Some(&extra))
            .await
            .unwrap();
        assert_eq!(response.message.content.as_deref(), Some("हैलो 🦀"));
        let calls = response.message.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.arguments, "{\"q\":\"done\"}");
        assert_eq!(response.usage.unwrap().prompt_tokens, 9);
        let sent = server.await.unwrap();
        assert!(sent.line.contains(if family == ApiFamily::Azure {
            "/openai/v1/chat/completions"
        } else {
            "/v1/chat/completions"
        }));
        assert_eq!(
            sent.body["messages"][0]["content"][1]["image_url"]["url"],
            "data:image/png;base64,aGVsbG8="
        );
        assert_eq!(sent.body["response_format"]["type"], "json_schema");
        if family == ApiFamily::Azure {
            assert!(sent.headers.contains("api-key: fixture-key"));
            assert!(!sent.headers.contains("authorization:"));
        } else {
            assert!(sent.headers.contains("authorization: bearer fixture-key"));
        }
    }
}

#[tokio::test]
async fn gemini_native_stream_retains_tool_signature_for_the_next_turn() {
    let payload = sse(&[
        json!({"candidates":[{"content":{"parts":[{"text":"हैलो"}]}}]}),
        json!({"candidates":[{"content":{"parts":[{"functionCall":{"id":"gemini-call","name":"lookup","args":{"q":"done"}},"thoughtSignature":"opaque-signature"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":2,"thoughtsTokenCount":3}}),
    ]);
    let (endpoint, server) = fixture(payload, 200, "text/event-stream").await;
    let native = provider(ApiFamily::Gemini, endpoint);
    let response = collect(&native, &[Message::system("System"), image_message()], None)
        .await
        .unwrap();
    assert_eq!(response.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(response.usage.unwrap().completion_tokens, 5);
    let mut result = Message::user("result");
    result.role = MessageRole::Tool;
    result.tool_call_id = Some("gemini-call".into());
    let replay = native
        .body(
            &[Message::user("query"), response.message, result],
            &[tool_schema()],
            Some(100),
            None,
            None,
            None,
            false,
        )
        .unwrap();
    assert_eq!(
        replay["contents"][1]["parts"][1]["thoughtSignature"],
        "opaque-signature"
    );
    assert_eq!(
        replay["contents"][2]["parts"][0]["functionResponse"]["name"],
        "lookup"
    );
    let sent = server.await.unwrap();
    assert!(
        sent.line
            .contains("/v1beta/models/fixture-model:streamGenerateContent?alt=sse")
    );
    assert!(sent.headers.contains("x-goog-api-key: fixture-key"));
    assert!(!sent.headers.contains("authorization:"));
    assert_eq!(
        sent.body["contents"][0]["parts"][1]["inlineData"]["mimeType"],
        "image/png"
    );
    assert!(sent.body.get("messages").is_none());
}

#[tokio::test]
async fn vertex_uses_project_publisher_endpoint_bearer_and_native_schema() {
    let payload=json!({"candidates":[{"content":{"parts":[{"text":"{\"ok\":true}"}]},"finishReason":"STOP"}]}).to_string().into_bytes();
    let (endpoint, server) = fixture(payload, 200, "application/json").await;
    let configuration = config(
        ApiFamily::Vertex,
        format!("{endpoint}/v1/projects/project/locations/region/publishers/google"),
    );
    let native = NativeProvider::new(
        configuration,
        ProviderCredential::Bearer("access-token".into()),
    )
    .unwrap();
    let extra = json!({"response_format":{"type":"json_schema","json_schema":{"schema":{"type":"object","properties":{"ok":{"type":"boolean"}}}}}});
    native
        .chat_completion(&[image_message()], &[], Some(100), None, None, Some(&extra))
        .await
        .unwrap();
    let sent = server.await.unwrap();
    assert!(sent.line.contains("/v1/projects/project/locations/region/publishers/google/models/fixture-model:generateContent"));
    assert!(sent.headers.contains("authorization: bearer access-token"));
    assert_eq!(
        sent.body["generationConfig"]["responseMimeType"],
        "application/json"
    );
    assert_eq!(
        sent.body["generationConfig"]["responseJsonSchema"]["type"],
        "object"
    );
}

#[tokio::test]
async fn anthropic_stream_preserves_interleaved_thinking_signature_and_compact_tool_indexes() {
    let payload = sse(&[
        json!({"type":"message_start","message":{"usage":{"input_tokens":8,"cache_read_input_tokens":2}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Consider"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"signed-thinking"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hello 🦀"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"anthropic-call","name":"lookup","input":{}}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"q\":\"done\"}"}}),
        json!({"type":"content_block_stop","index":2}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":6}}),
        json!({"type":"message_stop"}),
    ]);
    let (endpoint, server) = fixture(payload, 200, "text/event-stream").await;
    let native = provider(ApiFamily::Anthropic, endpoint);
    let response = collect(&native, &[image_message()], None).await.unwrap();
    assert_eq!(response.message.tool_calls.as_ref().unwrap().len(), 1);
    assert_eq!(
        response.message.anthropic_content_blocks.as_ref().unwrap()[0]["signature"],
        "signed-thinking"
    );
    assert_eq!(response.usage.unwrap().prompt_tokens, 10);
    let replay = native
        .body(
            &[Message::user("query"), response.message],
            &[],
            Some(100),
            None,
            None,
            None,
            false,
        )
        .unwrap();
    assert_eq!(
        replay["messages"][1]["content"][0]["signature"],
        "signed-thinking"
    );
    let sent = server.await.unwrap();
    assert!(sent.line.contains("/v1/messages"));
    assert!(sent.headers.contains("x-api-key: fixture-key"));
    assert!(sent.headers.contains("anthropic-version: 2023-06-01"));
    assert!(!sent.headers.contains("user-agent: claude"));
    assert_eq!(
        sent.body["messages"][0]["content"][1]["source"]["type"],
        "base64"
    );
}

#[tokio::test]
async fn responses_stream_uses_public_api_and_preserves_encrypted_history() {
    let output = json!([{"type":"reasoning","id":"reasoning-1","encrypted_content":"opaque-encrypted"},{"type":"function_call","id":"fc-1","call_id":"responses-call","name":"lookup","arguments":"{\"q\":\"done\"}"}]);
    let payload = sse(&[
        json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc-1","call_id":"responses-call","name":"lookup","arguments":""}}),
        json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"q\":\"done\"}"}),
        json!({"type":"response.output_item.done","output_index":1,"item":output[1]}),
        json!({"type":"response.completed","response":{"status":"completed","model":"fixture-model","output":output,"usage":{"input_tokens":8,"output_tokens":3}}}),
    ]);
    let (endpoint, server) = fixture(payload, 200, "text/event-stream").await;
    let native = provider(ApiFamily::OpenAiResponses, endpoint);
    let extra = json!({"response_format":{"type":"json_schema","json_schema":{"name":"answer","strict":true,"schema":{"type":"object"}}}});
    let response = collect(
        &native,
        &[Message::system("System"), image_message()],
        Some(&extra),
    )
    .await
    .unwrap();
    assert_eq!(response.message.tool_calls.as_ref().unwrap().len(), 1);
    let replay = native
        .body(
            &[Message::user("query"), response.message],
            &[],
            Some(100),
            None,
            None,
            None,
            true,
        )
        .unwrap();
    assert_eq!(replay["input"][1]["encrypted_content"], "opaque-encrypted");
    let sent = server.await.unwrap();
    assert!(sent.line.contains("/v1/responses"));
    assert_eq!(sent.body["store"], false);
    assert_eq!(sent.body["instructions"], "System");
    assert_eq!(sent.body["input"][0]["content"][1]["type"], "input_image");
    assert_eq!(sent.body["text"]["format"]["type"], "json_schema");
}

#[tokio::test]
async fn stream_without_terminal_and_late_responses_failure_are_errors() {
    for (family, payload) in [
        (
            ApiFamily::OpenAiChat,
            sse(&[json!({"choices":[{"delta":{"content":"partial"},"finish_reason":null}]})]),
        ),
        (
            ApiFamily::Gemini,
            sse(&[json!({"candidates":[{"content":{"parts":[{"text":"partial"}]}}]})]),
        ),
        (
            ApiFamily::Anthropic,
            sse(&[json!({"type":"message_start","message":{"usage":{}}})]),
        ),
        (
            ApiFamily::OpenAiResponses,
            sse(&[
                json!({"type":"response.output_text.delta","delta":"partial"}),
                json!({"type":"response.failed","response":{"error":{"code":"subscription_sharing_usage_limit_exceeded"}}}),
            ]),
        ),
    ] {
        let (endpoint, server) = fixture(payload, 200, "text/event-stream").await;
        let native = provider(family, endpoint);
        assert!(
            collect(&native, &[Message::user("query")], None)
                .await
                .is_err()
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn provider_http_auth_error_does_not_echo_a_reflected_credential() {
    let (endpoint, server) = fixture(
        json!({"error":{"code":"invalid_api_key","message":"reflected fixture-key"}})
            .to_string()
            .into_bytes(),
        401,
        "application/json",
    )
    .await;
    let native = provider(ApiFamily::OpenAiChat, endpoint);
    let error = native
        .chat_completion(&[Message::user("query")], &[], Some(100), None, None, None)
        .await
        .err()
        .unwrap();
    assert!(matches!(error, AgentError::AuthFailed(_)));
    assert!(!error.to_string().contains("fixture-key"));
    server.await.unwrap();
}

#[test]
fn chatgpt_plan_requests_obey_public_responses_restrictions() {
    let credential = ProviderCredential::ChatGpt {
        access_token: "fixture".into(),
        client_id: "oaiapp_fixture".into(),
        subject: "account".into(),
        host_id: "host".into(),
        scopes: vec!["chatgpt.tokens.use.direct".into()],
    };
    assert!(
        NativeProvider::new(
            config(
                ApiFamily::OpenAiResponses,
                "https://chatgpt.com/backend-api/codex".into()
            ),
            credential.clone()
        )
        .is_err()
    );
    let native = NativeProvider::new(
        config(
            ApiFamily::OpenAiResponses,
            "https://api.openai.com/v1".into(),
        ),
        credential,
    )
    .unwrap();
    let body = native
        .body(
            &[Message::system("instructions"), Message::user("query")],
            &[tool_schema()],
            Some(100),
            Some(0.3),
            None,
            None,
            true,
        )
        .unwrap();
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], true);
    assert!(body.get("max_output_tokens").is_none());
    assert!(body.get("temperature").is_none());
    assert_eq!(body["tools"][0]["type"], "namespace");
    assert_eq!(body["tools"][0]["tools"][0]["type"], "function");
    assert_eq!(body["instructions"], "instructions");
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}
fn aws_frame(kind: &str, payload: Value) -> Vec<u8> {
    let mut headers = Vec::new();
    for (name, value) in [
        (":message-type", "event"),
        (":event-type", kind),
        (":content-type", "application/json"),
    ] {
        headers.push(name.len() as u8);
        headers.extend_from_slice(name.as_bytes());
        headers.push(7);
        headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
        headers.extend_from_slice(value.as_bytes());
    }
    let payload = payload.to_string().into_bytes();
    let mut frame = Vec::new();
    frame.extend_from_slice(&((16 + headers.len() + payload.len()) as u32).to_be_bytes());
    frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    frame.extend_from_slice(&crc32(&frame).to_be_bytes());
    frame.extend(headers);
    frame.extend(payload);
    frame.extend_from_slice(&crc32(&frame).to_be_bytes());
    frame
}
#[tokio::test]
async fn bedrock_native_binary_stream_retains_signed_reasoning_tools_images_and_usage() {
    let mut payload = Vec::new();
    for (kind, data) in [
        ("messageStart", json!({"role":"assistant"})),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"think","signature":"signed"}}}),
        ),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":1,"delta":{"text":"Hello 🦀"}}),
        ),
        (
            "contentBlockStart",
            json!({"contentBlockIndex":5,"start":{"toolUse":{"toolUseId":"bedrock-call","name":"lookup"}}}),
        ),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":5,"delta":{"toolUse":{"input":"{\"q\":"}}}),
        ),
        (
            "contentBlockDelta",
            json!({"contentBlockIndex":5,"delta":{"toolUse":{"input":"\"done\"}"}}}),
        ),
        ("messageStop", json!({"stopReason":"tool_use"})),
        (
            "metadata",
            json!({"usage":{"inputTokens":8,"outputTokens":4,"totalTokens":12}}),
        ),
    ] {
        payload.extend(aws_frame(kind, data));
    }
    let (endpoint, server) = fixture(payload, 200, "application/vnd.amazon.eventstream").await;
    let native = build(
        &config(ApiFamily::Bedrock, endpoint),
        Some("bedrock-key".into()),
    )
    .unwrap();
    let response = collect(native.as_ref(), &[image_message()], None)
        .await
        .unwrap();
    assert_eq!(response.message.content.as_deref(), Some("Hello 🦀"));
    assert_eq!(response.message.tool_calls.as_ref().unwrap().len(), 1);
    assert_eq!(
        response.message.tool_calls.as_ref().unwrap()[0]
            .function
            .arguments,
        "{\"q\":\"done\"}"
    );
    assert_eq!(response.usage.unwrap().total_tokens, 12);
    let replay = hermes_agent::bedrock::build_converse_body(
        "fixture-model",
        &[Message::user("query"), response.message],
        &[],
        None,
        None,
        None,
    );
    assert_eq!(
        replay["messages"][1]["content"][0]["reasoningContent"]["reasoningText"]["signature"],
        "signed"
    );
    let sent = server.await.unwrap();
    assert!(sent.line.contains("/model/fixture-model/converse-stream"));
    assert!(sent.headers.contains("authorization: bearer bedrock-key"));
    assert_eq!(
        sent.body["messages"][0]["content"][1]["image"]["format"],
        "png"
    );
    assert_eq!(
        sent.body["toolConfig"]["tools"][0]["toolSpec"]["name"],
        "lookup"
    );
}
#[tokio::test]
async fn bedrock_signed_requests_use_selected_credentials_and_reject_truncated_or_corrupt_frames() {
    let body=json!({"output":{"message":{"role":"assistant","content":[{"text":"done"}]}},"stopReason":"end_turn","usage":{"inputTokens":2,"outputTokens":1}}).to_string().into_bytes();
    let (endpoint, server) = fixture(body, 200, "application/json").await;
    let native=build(&config(ApiFamily::Bedrock,endpoint),Some(json!({"access_key_id":"selected-id","secret_access_key":"selected-secret","session_token":"selected-session"}).to_string())).unwrap();
    native
        .chat_completion(&[Message::user("query")], &[], Some(100), None, None, None)
        .await
        .unwrap();
    let sent = server.await.unwrap();
    assert!(
        sent.headers
            .contains("authorization: aws4-hmac-sha256 credential=selected-id/")
    );
    assert!(
        sent.headers
            .contains("x-amz-security-token: selected-session")
    );
    assert!(!sent.headers.contains("selected-secret"));
    for corrupt in [false, true] {
        let mut payload = aws_frame(
            "contentBlockDelta",
            json!({"contentBlockIndex":0,"delta":{"text":"partial"}}),
        );
        if corrupt {
            let last = payload.len() - 1;
            payload[last] ^= 1;
        }
        let (endpoint, server) = fixture(payload, 200, "application/vnd.amazon.eventstream").await;
        let native = build(&config(ApiFamily::Bedrock, endpoint), Some("key".into())).unwrap();
        assert!(
            collect(native.as_ref(), &[Message::user("query")], None)
                .await
                .is_err()
        );
        server.await.unwrap();
    }
}
#[tokio::test]
async fn gemini_model_override_changes_native_url_and_chat_input_images_are_normalized() {
    let (endpoint, server) = fixture(
        json!({"candidates":[{"content":{"parts":[{"text":"done"}]},"finishReason":"STOP"}]})
            .to_string()
            .into_bytes(),
        200,
        "application/json",
    )
    .await;
    let native = provider(ApiFamily::Gemini, endpoint);
    native
        .chat_completion(
            &[Message::user("query")],
            &[],
            Some(100),
            None,
            Some("override-model"),
            None,
        )
        .await
        .unwrap();
    assert!(
        server
            .await
            .unwrap()
            .line
            .contains("/models/override-model:generateContent")
    );
    let native = provider(ApiFamily::OpenAiChat, "http://127.0.0.1:1".into());
    let message = Message::user(format!(
        "__hermes_acp_parts_json__:{}",
        json!([{"type":"input_image","image_url":"data:image/png;base64,aGVsbG8="}])
    ));
    let body = native
        .body(&[message], &[], None, None, None, None, false)
        .unwrap();
    assert_eq!(body["messages"][0]["content"][0]["type"], "image_url");
}

#[tokio::test]
async fn bedrock_requires_a_valid_unfiltered_terminal_reason_even_after_http_success() {
    for stop in [
        None,
        Some("content_filtered"),
        Some("guardrail_intervened"),
        Some("unknown_reason"),
    ] {
        let mut response =
            json!({"output":{"message":{"role":"assistant","content":[{"text":"partial"}]}}});
        if let Some(stop) = stop {
            response["stopReason"] = json!(stop);
        }
        let (endpoint, server) =
            fixture(response.to_string().into_bytes(), 200, "application/json").await;
        let native = build(
            &config(ApiFamily::Bedrock, endpoint),
            Some("fixture-key".into()),
        )
        .unwrap();
        assert!(
            native
                .chat_completion(&[Message::user("query")], &[], None, None, None, None)
                .await
                .is_err()
        );
        server.await.unwrap();
        let payload = aws_frame(
            "messageStop",
            stop.map_or_else(|| json!({}), |stop| json!({"stopReason":stop})),
        );
        let (endpoint, server) = fixture(payload, 200, "application/vnd.amazon.eventstream").await;
        let native = build(
            &config(ApiFamily::Bedrock, endpoint),
            Some("fixture-key".into()),
        )
        .unwrap();
        assert!(
            collect(native.as_ref(), &[Message::user("query")], None)
                .await
                .is_err()
        );
        server.await.unwrap();
    }
}

#[test]
fn configured_reasoning_is_serialized_for_each_openai_protocol() {
    for family in [ApiFamily::OpenAiChat, ApiFamily::OpenAiResponses] {
        let mut saved =
            serde_json::to_value(config(family.clone(), "http://127.0.0.1:8080/v1".into()))
                .unwrap();
        saved["model"] = json!("gpt-5.2");
        saved["reasoning_effort"] = json!("high");
        let config: ProviderConfig = serde_json::from_value(saved).unwrap();
        let provider = NativeProvider::new(config, ProviderCredential::None).unwrap();
        let body = provider
            .body(
                &[Message::user("query")],
                &[],
                None,
                None,
                None,
                None,
                false,
            )
            .unwrap();
        let effort = if family == ApiFamily::OpenAiResponses {
            &body["reasoning"]["effort"]
        } else {
            &body["reasoning_effort"]
        };
        assert_eq!(effort, "high");
    }
}

#[test]
fn unsupported_or_overridden_models_do_not_receive_saved_reasoning() {
    for (family, model) in [
        (ApiFamily::OpenAiChat, "gpt-4.1"),
        (ApiFamily::Anthropic, "claude-sonnet-4"),
    ] {
        let mut config = config(family, "http://127.0.0.1:8080/v1".into());
        config.model = model.into();
        config.reasoning_effort = Some("high".into());
        let provider = NativeProvider::new(config, ProviderCredential::None).unwrap();
        let body = provider
            .body(
                &[Message::user("query")],
                &[],
                None,
                None,
                None,
                None,
                false,
            )
            .unwrap();
        assert!(body.get("reasoning_effort").is_none() && body.get("reasoning").is_none());
    }
    let mut config = config(ApiFamily::OpenAiChat, "http://127.0.0.1:8080/v1".into());
    config.model = "gpt-5.2".into();
    config.reasoning_effort = Some("high".into());
    let provider = NativeProvider::new(config, ProviderCredential::None).unwrap();
    let body = provider
        .body(
            &[Message::user("query")],
            &[],
            None,
            None,
            Some("gpt-4.1"),
            None,
            false,
        )
        .unwrap();
    assert!(body.get("reasoning_effort").is_none());
}

#[test]
fn future_model_advertised_reasoning_is_sent_without_a_name_allowlist() {
    for family in [ApiFamily::OpenAiChat, ApiFamily::OpenAiResponses] {
        let mut value =
            serde_json::to_value(config(family.clone(), "http://127.0.0.1:8080/v1".into()))
                .unwrap();
        value["model"] = json!("future-account-model");
        value["reasoning_options"] = json!(["low", "high"]);
        value["default_reasoning_effort"] = json!("low");
        value["reasoning_effort"] = json!("high");
        let config: ProviderConfig = serde_json::from_value(value).unwrap();
        let provider = NativeProvider::new(config, ProviderCredential::None).unwrap();
        let body = provider
            .body(
                &[Message::user("query")],
                &[],
                None,
                None,
                None,
                None,
                false,
            )
            .unwrap();
        let effort = if family == ApiFamily::OpenAiResponses {
            &body["reasoning"]["effort"]
        } else {
            &body["reasoning_effort"]
        };
        assert_eq!(effort, "high");
    }
}

#[test]
fn model_override_does_not_inherit_another_models_catalog_metadata() {
    let config = ProviderConfig {
        model: "original-model".into(),
        reasoning_options: Some(vec!["high".into()]),
        reasoning_effort: Some("high".into()),
        ..config(
            ApiFamily::OpenAiResponses,
            "http://127.0.0.1:8080/v1".into(),
        )
    };
    let provider = NativeProvider::new(config, ProviderCredential::None).unwrap();
    let body = provider
        .body(
            &[Message::user("query")],
            &[],
            None,
            None,
            Some("different-unknown-model"),
            None,
            false,
        )
        .unwrap();
    assert!(body.get("reasoning").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plan_delegate_collects_required_responses_stream_with_inherited_model_and_reasoning() {
    use hermes_agent::{
        AgentConfig, InterruptController, SubAgentOrchestrator, SubAgentOrchestratorConfig,
        SubAgentRequest,
    };
    let output = json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Plan child complete"}]}]);
    let payload = sse(&[
        json!({"type":"response.completed","response":{"status":"completed","model":"gpt-6.1-sol","output":output,"usage":{"input_tokens":5,"output_tokens":2}}}),
    ]);
    let (endpoint, server) = fixture(payload, 200, "text/event-stream").await;
    let mut config = config(ApiFamily::OpenAiResponses, endpoint);
    config.model = "gpt-6.1-sol".into();
    config.reasoning_options = Some(vec!["high".into()]);
    config.reasoning_effort = Some("high".into());
    // Only transport is local: exercise plan serialization/collector with a fake
    // API key, without relaxing production official-origin credential checks.
    let mut native =
        NativeProvider::new(config, ProviderCredential::ApiKey("fixture-key".into())).unwrap();
    native.chatgpt_plan = true;
    let directory = tempfile::tempdir().unwrap();
    let orchestrator = Arc::new(SubAgentOrchestrator::new(SubAgentOrchestratorConfig {
        parent_config: AgentConfig {
            native_product_finalization: true,
            model: "gpt-6.1-sol".into(),
            skip_memory: true,
            skip_context_files: true,
            hermes_home: Some(directory.path().to_string_lossy().into_owned()),
            ..Default::default()
        },
        tool_registry: Arc::new(hermes_agent::agent_loop::ToolRegistry::new()),
        llm_provider: Arc::new(native),
        parent_credential_pool: None,
        parent_interrupt: InterruptController::new(),
        hermes_home: directory.path().into(),
        parent_session_id: Some("plan-parent".into()),
        timeout: Duration::from_secs(5),
        background_delegation_callback: None,
    }));
    let result = orchestrator
        .execute(SubAgentRequest {
            task: "Return a short result".into(),
            child_depth: 1,
            max_depth: 1,
            ..Default::default()
        })
        .await;
    let result: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(result["status"], "completed");
    assert_eq!(result["result"], "Plan child complete");
    let sent = server.await.unwrap();
    assert_eq!(sent.body["model"], "gpt-6.1-sol");
    assert_eq!(sent.body["stream"], true);
    assert_eq!(sent.body["store"], false);
    assert_eq!(sent.body["reasoning"]["effort"], "high");
    assert!(sent.body.get("max_output_tokens").is_none());
    let lineage: Value = serde_json::from_slice(
        &std::fs::read(
            directory
                .path()
                .join("subagents")
                .join(format!("{}.json", result["sub_agent_id"].as_str().unwrap())),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(lineage["model"], "gpt-6.1-sol");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejected_delegate_model_fails_once_with_actionable_model_identity() {
    use hermes_agent::{
        AgentConfig, InterruptController, SubAgentOrchestrator, SubAgentOrchestratorConfig,
        SubAgentRequest,
    };
    let (endpoint, server) = fixture(
        br#"{"error":{"code":"model_not_found","message":"must not reflect credential details"}}"#
            .to_vec(),
        400,
        "application/json",
    )
    .await;
    let mut config = config(ApiFamily::OpenAiResponses, endpoint);
    config.model = "gpt-6.1-sol".into();
    let native =
        NativeProvider::new(config, ProviderCredential::ApiKey("fixture-key".into())).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let mut parent = AgentConfig {
        native_product_finalization: true,
        model: "gpt-6.1-sol".into(),
        skip_memory: true,
        skip_context_files: true,
        hermes_home: Some(directory.path().to_string_lossy().into_owned()),
        ..Default::default()
    };
    parent.allow_model_fallbacks = false;
    parent.retry.max_retries = 1;
    parent.retry.base_delay_ms = 1;
    let orchestrator = Arc::new(SubAgentOrchestrator::new(SubAgentOrchestratorConfig {
        parent_config: parent,
        tool_registry: Arc::new(hermes_agent::agent_loop::ToolRegistry::new()),
        llm_provider: Arc::new(native),
        parent_credential_pool: None,
        parent_interrupt: InterruptController::new(),
        hermes_home: directory.path().into(),
        parent_session_id: Some("parent".into()),
        timeout: Duration::from_secs(5),
        background_delegation_callback: None,
    }));
    let result: Value = serde_json::from_str(
        &orchestrator
            .execute(SubAgentRequest {
                task: "Return a short result".into(),
                child_depth: 1,
                max_depth: 1,
                ..Default::default()
            })
            .await,
    )
    .unwrap();
    assert_eq!(result["status"], "failed");
    let error = result["error"].as_str().unwrap();
    assert!(error.contains("gpt-6.1-sol"), "{error}");
    assert!(error.contains("Responses"), "{error}");
    assert!(error.contains("Refresh models"), "{error}");
    assert!(
        !error.contains("transport failed"),
        "A fatal catalog rejection must not retry or select another model: {error}"
    );
    assert!(!error.contains("credential details"));
    assert_eq!(server.await.unwrap().body["model"], "gpt-6.1-sol");
}

#[tokio::test]
async fn native_selected_model_survives_sdk_cost_guard_without_substitution() {
    use hermes_agent::{AgentConfig, AgentLoop};
    let response = json!({"status":"completed","model":"gpt-6.1-sol","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Finished"}]}],"usage":{"input_tokens":10,"output_tokens":2}});
    let (endpoint, server) =
        fixture(response.to_string().into_bytes(), 200, "application/json").await;
    let mut config = config(ApiFamily::OpenAiResponses, endpoint);
    config.model = "gpt-6.1-sol".into();
    let native = Arc::new(
        NativeProvider::new(config, ProviderCredential::ApiKey("fixture-key".into())).unwrap(),
    );
    let directory = tempfile::tempdir().unwrap();
    let agent = AgentLoop::new(
        AgentConfig {
            model: "gpt-6.1-sol".into(),
            native_product_finalization: true,
            allow_model_fallbacks: false,
            max_cost_usd: Some(1.0),
            cost_guard_degrade_at_ratio: 0.1,
            prompt_cost_per_million_usd: Some(50000.0),
            completion_cost_per_million_usd: Some(0.0),
            skip_memory: true,
            skip_context_files: true,
            hermes_home: Some(directory.path().to_string_lossy().into_owned()),
            ..Default::default()
        },
        Arc::new(hermes_agent::agent_loop::ToolRegistry::new()),
        native,
    );
    let result = agent
        .run(vec![Message::user("Return a short result")], Some(vec![]))
        .await
        .unwrap();
    assert!(result.finished_naturally);
    assert!(result.messages.iter().any(|message| {
        message
            .content
            .as_deref()
            .is_some_and(|text| text.contains("Cost guard warning"))
    }));
    assert!(!result.messages.iter().any(|message| {
        message
            .content
            .as_deref()
            .is_some_and(|text| text.contains("Switching to cheaper model"))
    }));
    assert_eq!(server.await.unwrap().body["model"], "gpt-6.1-sol");
}

#[tokio::test]
async fn sse_request_rejection_preserves_safe_detail_and_redacts_reflected_credentials() {
    let payload = sse(&[
        json!({"type":"error","request_id":"req_fixture123","error":{"type":"invalid_request_error","code":"invalid_tool_output","param":"input[7].call_id","message":"No tool output found for function call 'call_private'. Authorization: Bearer fixture-key. Check https://user:password@provider.invalid/trace?api_key=private-query and API key sk-reflected-unknown-secret."}}),
    ]);
    let (endpoint, server) = fixture(payload, 200, "text/event-stream").await;
    let error = collect(
        &provider(ApiFamily::OpenAiResponses, endpoint),
        &[Message::user("query")],
        None,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("No tool output found"), "{error}");
    assert!(error.contains("invalid_tool_output"), "{error}");
    assert!(error.contains("invalid_request_error"));
    assert!(error.contains("input[7].call_id"));
    assert!(error.contains("req_fixture123"));
    for secret in [
        "fixture-key",
        "call_private",
        "https://",
        "user:password",
        "private-query",
        "sk-reflected-unknown-secret",
    ] {
        assert!(!error.contains(secret), "Leaked {secret}: {error}");
    }
    assert!(error.len() < 1200);
    server.await.unwrap();
}

#[tokio::test]
async fn failed_response_diagnostics_are_bounded_even_with_adversarial_metadata() {
    let payload = sse(&[
        json!({"type":"response.failed","response":{"error":{"type":"invalid_request_error","code":"fixture-key","param":"https://provider.invalid/?token=fixture-key","request_id":"Bearer fixture-key","message":format!("Invalid 'input[4].content': {} fixture-key", "敏感".repeat(10000))}}}),
    ]);
    let (endpoint, server) = fixture(payload, 200, "text/event-stream").await;
    let error = collect(
        &provider(ApiFamily::OpenAiResponses, endpoint),
        &[Message::user("query")],
        None,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("invalid_request_error"), "{error}");
    assert!(error.contains("input[4].content"), "{error}");
    assert!(!error.contains("fixture-key"));
    assert!(!error.contains("https://"));
    assert!(error.len() < 1200);
    server.await.unwrap();
}

#[tokio::test]
async fn sse_error_uses_safe_transport_request_id_when_body_omits_it() {
    let payload = sse(&[
        json!({"type":"error","code":"invalid_request_error","param":"input","message":"Invalid request field 'input'."}),
    ]);
    let (endpoint, server) = fixture(payload, 200, "text/event-stream").await;
    let error = collect(
        &provider(ApiFamily::OpenAiResponses, endpoint),
        &[Message::user("query")],
        None,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("request=req_transport_fixture"), "{error}");
    assert!(error.contains("Invalid request field input"), "{error}");
    server.await.unwrap();
}

async fn classified_error_request_count(
    status: u16,
    request_id: &'static str,
    streaming: bool,
) -> usize {
    use hermes_agent::{AgentConfig, AgentLoop};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let stop = tokio_util::sync::CancellationToken::new();
    let server_stop = stop.clone();
    let server = tokio::spawn(async move {
        let mut requests = 0;
        loop {
            let accepted = tokio::select! {_=server_stop.cancelled()=>break,value=listener.accept()=>value.unwrap()};
            let mut socket = BufReader::new(accepted.0);
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            let mut length = 0;
            loop {
                let mut line = String::new();
                socket.read_line(&mut line).await.unwrap();
                if let Some((key, value)) = line.split_once(':') {
                    if key.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                }
                if line == "\r\n" {
                    break;
                }
            }
            let mut bytes = vec![0; length];
            socket.read_exact(&mut bytes).await.unwrap();
            requests += 1;
            let body = if status == 200 {
                format!(
                    "data: {}\n\n",
                    json!({"type":"error","request_id":request_id,"error":{"type":"invalid_request_error","code":"invalid_request_error","message":"Invalid input field 'input'."}})
                )
            } else {
                json!({"error":{"code":if status==429 {"rate_limit_exceeded"} else {"server_error"}}}).to_string()
            };
            socket.get_mut().write_all(format!("HTTP/1.1 {status} Fixture\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",if status==200 {"text/event-stream"} else {"application/json"},body.len()).as_bytes()).await.unwrap();
        }
        requests
    });
    // Delegated plan calls use the non-streaming AgentLoop entry point, while
    // the native plan adapter collects the required HTTP SSE stream internally.
    let mut native = provider(ApiFamily::OpenAiResponses, endpoint);
    native.chatgpt_plan = true;
    let native = Arc::new(native);
    let directory = tempfile::tempdir().unwrap();
    let mut config = AgentConfig {
        model: "fixture-model".into(),
        allow_model_fallbacks: false,
        native_product_finalization: true,
        skip_memory: true,
        skip_context_files: true,
        hermes_home: Some(directory.path().to_string_lossy().into_owned()),
        ..Default::default()
    };
    config.retry.max_retries = 1;
    config.stream_read_max_retries = 1;
    config.retry.base_delay_ms = 1;
    config.retry.max_delay_ms = 2;
    let agent = AgentLoop::new(
        config,
        Arc::new(hermes_agent::agent_loop::ToolRegistry::new()),
        native,
    );
    if streaming {
        agent
            .run_stream(vec![Message::user("Return a result")], Some(vec![]), None)
            .await
            .unwrap_err();
    } else {
        agent
            .run(vec![Message::user("Return a result")], Some(vec![]))
            .await
            .unwrap_err();
    }
    stop.cancel();
    server.await.unwrap()
}

#[tokio::test]
async fn invalid_request_ids_with_status_digits_are_terminal_without_retry() {
    for request_id in ["req_503abc", "req_429abc"] {
        assert_eq!(
            classified_error_request_count(200, request_id, false).await,
            1,
            "Request metadata must not change invalid-request classification: {request_id}"
        );
    }
}

#[tokio::test]
async fn actual_http_rate_limits_and_server_failures_still_retry() {
    for status in [429, 503] {
        assert_eq!(
            classified_error_request_count(status, "req_fixture", false).await,
            2,
            "HTTP status {status} must retain the configured retry"
        );
    }
}

#[tokio::test]
async fn stream_request_ids_with_connection_text_do_not_retry_invalid_requests() {
    assert_eq!(
        classified_error_request_count(200, "req_connection_503abc", true).await,
        1
    );
}
