//! HTTP fixtures qualify wire behavior, not acceptance by an external account.
use super::*;
use crate::model::{
    Assembler, FinishReason, InlineImage, Message, ModelResponse, NativeContent, Role, ToolCall,
    ToolSchema,
};
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
    ToolSchema {
        name: "lookup".into(),
        description: "Look up a fixture.".into(),
        parameters: json!({"type":"object","properties":{"q":{"type":"string"}},"required":["q"],"additionalProperties":false}),
    }
}

fn sse(events: &[Value]) -> Vec<u8> {
    events
        .iter()
        .map(|event| format!("data: {event}\r\n\r\n"))
        .collect::<String>()
        .into_bytes()
}

async fn collect(
    provider: &dyn ModelProvider,
    messages: &[Message],
) -> Result<ModelResponse, ModelError> {
    let mut stream = provider.stream(
        ModelRequest {
            messages: messages.to_vec(),
            tools: vec![tool_schema()],
            max_tokens: 100,
            reasoning: crate::model::Reasoning::Configured,
        },
        CancellationToken::new(),
    );
    let mut response = Assembler::default();
    while let Some(event) = stream.next().await {
        response.push(event?);
    }
    response.finish()
}

fn image_message() -> Message {
    let mut message = Message::user("What is shown?");
    message.images.push(InlineImage {
        media_type: "image/png".into(),
        data: "aGVsbG8=".into(),
    });
    message
}

fn aws_frame(kind: &str, payload: Value) -> Vec<u8> {
    bedrock::tests::frame(kind, json!({ kind: payload }))
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
            &CancellationToken::new(),
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
        let payload = match family {
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
            ApiFamily::Bedrock => [
                aws_frame("messageStart", json!({"role":"assistant"})),
                aws_frame(
                    "contentBlockDelta",
                    json!({"contentBlockIndex":0,"delta":{"text":"Seen fixture image"}}),
                ),
                aws_frame("messageStop", json!({"stopReason":"end_turn"})),
                aws_frame(
                    "metadata",
                    json!({"usage":{"inputTokens":3,"outputTokens":3,"totalTokens":6}}),
                ),
            ]
            .concat(),
        };
        let content_type = if family == ApiFamily::Bedrock {
            "application/vnd.amazon.eventstream"
        } else {
            "text/event-stream"
        };
        let (endpoint, server) = fixture(payload, 200, content_type).await;
        let endpoint = if family == ApiFamily::Vertex {
            format!("{endpoint}/v1/projects/fixture/locations/us-central1/publishers/google")
        } else {
            endpoint
        };
        let provider_config = config(family.clone(), endpoint);
        let inner = build(&provider_config, Some("fixture-key".into())).unwrap();
        let wrapped = AttachmentProvider::new(inner, assets.clone(), session);
        let turn = TurnRequest {
            session_id: session,
            turn_id: uuid::Uuid::new_v4(),
            prompt: "Describe this managed image".into(),
            workspace: directory.path().into(),
            provider: provider_config,
            permissions: PermissionMode::ReadOnly,
            attachments: vec![attachment.clone()],
            approved_rules: Vec::new(),
        };
        let message = user_message(&turn);
        let durable = serde_json::to_string(&message).unwrap();
        assert!(!durable.contains("base64"));
        assert!(!durable.contains("data:image"));
        let response = collect(&wrapped, std::slice::from_ref(&message))
            .await
            .unwrap();
        assert_eq!(response.message.content, "Seen fixture image");
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
async fn image_rejection_names_the_selected_model_without_switching() {
    let (endpoint, server) = fixture(
        json!({"error":{"code":"unsupported_image"}})
            .to_string()
            .into_bytes(),
        400,
        "application/json",
    )
    .await;
    let error = collect(
        &provider(ApiFamily::OpenAiChat, endpoint),
        &[image_message()],
    )
    .await
    .unwrap_err();
    assert!(!error.retryable());
    let error = error.to_string();
    assert!(error.contains("chosen model supports image input"));
    assert!(error.contains("no other model was selected"));
    server.await.unwrap();
    let mut invalid = image_message();
    invalid.images[0].media_type = "image/svg+xml".into();
    let native = provider(ApiFamily::OpenAiChat, "http://127.0.0.1:1".into());
    assert!(matches!(
        request::body(
            &native,
            &[invalid],
            &[],
            100,
            crate::model::Reasoning::Configured
        ),
        Err(ModelError::InvalidRequest(_))
    ));
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
async fn chat_and_azure_use_correct_paths_headers_images_and_tool_fragments() {
    for family in [ApiFamily::OpenAiChat, ApiFamily::Azure] {
        let mut payload = sse(&[
            json!({"choices":[{"delta":{"content":"हैलो 🦀"},"finish_reason":null}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":4,"id":"call-fixture","function":{"name":"lookup","arguments":"{\"q\":"}}]},"finish_reason":null}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":4,"function":{"arguments":"\"done\"}"}}]},"finish_reason":"tool_calls"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":9,"completion_tokens":4,"prompt_tokens_details":{"cached_tokens":6}}}),
        ]);
        payload.extend_from_slice(b"data: [DONE]\r\n\r\n");
        let (endpoint, server) = fixture(payload, 200, "text/event-stream").await;
        let native = provider(family.clone(), endpoint);
        let response = collect(&native, &[image_message()]).await.unwrap();
        assert_eq!(response.message.content, "हैलो 🦀");
        assert_eq!(response.message.tool_calls.len(), 1);
        assert_eq!(response.message.tool_calls[0].arguments, "{\"q\":\"done\"}");
        assert_eq!(response.usage.unwrap().input_tokens, 9);
        assert_eq!(response.usage.unwrap().cache_read_tokens, Some(6));
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
        assert_eq!(sent.body["stream"], true);
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
        json!({"candidates":[{"content":{"parts":[{"functionCall":{"id":"gemini-call","name":"lookup","args":{"q":"done"}},"thoughtSignature":"opaque-signature"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":2,"thoughtsTokenCount":3,"cachedContentTokenCount":4}}),
    ]);
    let (endpoint, server) = fixture(payload, 200, "text/event-stream").await;
    let native = provider(ApiFamily::Gemini, endpoint);
    let response = collect(&native, &[Message::system("System"), image_message()])
        .await
        .unwrap();
    assert_eq!(response.finish, FinishReason::ToolCalls);
    assert_eq!(response.usage.unwrap().output_tokens, 5);
    assert_eq!(response.usage.unwrap().cache_read_tokens, Some(4));
    let replay = request::body(
        &native,
        &[
            Message::user("query"),
            response.message,
            Message::tool_result("gemini-call", "result"),
        ],
        &[tool_schema()],
        100,
        crate::model::Reasoning::Configured,
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
    assert_eq!(sent.body["systemInstruction"]["parts"][0]["text"], "System");
}

#[tokio::test]
async fn vertex_uses_project_publisher_endpoint_and_bearer() {
    let payload =
        sse(&[json!({"candidates":[{"content":{"parts":[{"text":"ok"}]},"finishReason":"STOP"}]})]);
    let (endpoint, server) = fixture(payload, 200, "text/event-stream").await;
    let configuration = config(
        ApiFamily::Vertex,
        format!("{endpoint}/v1/projects/project/locations/region/publishers/google"),
    );
    let native = NativeProvider::new(
        configuration,
        ProviderCredential::Bearer("access-token".into()),
    )
    .unwrap();
    collect(&native, &[image_message()]).await.unwrap();
    let sent = server.await.unwrap();
    assert!(sent.line.contains("/v1/projects/project/locations/region/publishers/google/models/fixture-model:streamGenerateContent"));
    assert!(sent.headers.contains("authorization: bearer access-token"));
    assert_eq!(sent.body["generationConfig"]["maxOutputTokens"], 100);
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
    let response = collect(&native, &[Message::system("Policy"), image_message()])
        .await
        .unwrap();
    assert_eq!(response.message.tool_calls.len(), 1);
    assert_eq!(response.message.reasoning.as_deref(), Some("Consider"));
    let Some(NativeContent::Anthropic(blocks)) = &response.message.native else {
        panic!("native blocks missing");
    };
    assert_eq!(blocks[0]["signature"], "signed-thinking");
    assert_eq!(response.usage.unwrap().input_tokens, 10);
    assert_eq!(response.usage.unwrap().cache_read_tokens, Some(2));
    let replay = request::body(
        &native,
        &[Message::user("query"), response.message],
        &[],
        100,
        crate::model::Reasoning::Configured,
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
    assert_eq!(
        sent.body["messages"][0]["content"][1]["source"]["type"],
        "base64"
    );
    assert_eq!(sent.body["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(sent.body["tools"][0]["cache_control"]["type"], "ephemeral");
}

#[tokio::test]
async fn a_long_anthropic_thinking_block_streams_to_identical_content() {
    let pieces: Vec<String> = (0..4000).map(|i| format!("step {i}; ")).collect();
    let mut events = vec![
        json!({"type":"message_start","message":{"usage":{"input_tokens":8}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"Begin: ","signature":""}}),
    ];
    events.extend(pieces.iter().map(|piece| {
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":piece}})
    }));
    events.extend([
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"nature"}}),
        json!({"type":"content_block_stop","index":0}),
        // A text block the stream never stops is still settled at message_stop.
        json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":"Hi"}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":" there"}}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":6}}),
        json!({"type":"message_stop"}),
    ]);
    let (endpoint, _server) = fixture(sse(&events), 200, "text/event-stream").await;
    let native = provider(ApiFamily::Anthropic, endpoint);
    let response = collect(&native, &[Message::user("think")]).await.unwrap();
    let thinking = format!("Begin: {}", pieces.concat());
    let Some(NativeContent::Anthropic(blocks)) = &response.message.native else {
        panic!("native blocks missing");
    };
    assert_eq!(
        blocks,
        &vec![
            json!({"type":"thinking","thinking":thinking,"signature":"sig-nature"}),
            json!({"type":"text","text":"Hi there"}),
        ]
    );
    assert_eq!(
        response.message.reasoning.as_deref(),
        Some(pieces.concat().as_str())
    );
}

#[test]
fn malformed_tool_arguments_replay_as_an_empty_object() {
    let native = provider(ApiFamily::Anthropic, "http://127.0.0.1:1".into());
    let call = crate::model::ToolCall::new("c1", "lookup", "{not json");
    let body = request::body(
        &native,
        &[
            Message::user("query"),
            Message::assistant_with_tool_calls("", vec![call]),
            Message::tool_result("c1", "error"),
        ],
        &[],
        100,
        crate::model::Reasoning::Configured,
    )
    .unwrap();
    assert_eq!(body["messages"][1]["content"][0]["input"], json!({}));
}

#[tokio::test]
async fn responses_stream_uses_public_api_and_preserves_encrypted_history() {
    let output = json!([{"type":"reasoning","id":"reasoning-1","encrypted_content":"opaque-encrypted"},{"type":"function_call","id":"fc-1","call_id":"responses-call","name":"lookup","arguments":"{\"q\":\"done\"}"}]);
    let payload = sse(&[
        json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc-1","call_id":"responses-call","name":"lookup","arguments":""}}),
        json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"q\":\"done\"}"}),
        json!({"type":"response.output_item.done","output_index":1,"item":output[1]}),
        json!({"type":"response.completed","response":{"status":"completed","model":"fixture-model","output":output,"usage":{"input_tokens":8,"output_tokens":3,"input_tokens_details":{"cached_tokens":5}}}}),
    ]);
    let (endpoint, server) = fixture(payload, 200, "text/event-stream").await;
    let native = provider(ApiFamily::OpenAiResponses, endpoint);
    let response = collect(&native, &[Message::system("System"), image_message()])
        .await
        .unwrap();
    assert_eq!(response.message.tool_calls.len(), 1);
    assert_eq!(response.usage.unwrap().cache_read_tokens, Some(5));
    let replay = request::body(
        &native,
        &[Message::user("query"), response.message],
        &[],
        100,
        crate::model::Reasoning::Configured,
    )
    .unwrap();
    assert_eq!(replay["input"][1]["encrypted_content"], "opaque-encrypted");
    let sent = server.await.unwrap();
    assert!(sent.line.contains("/v1/responses"));
    assert_eq!(sent.body["store"], false);
    assert_eq!(sent.body["instructions"], "System");
    assert_eq!(sent.body["input"][0]["content"][1]["type"], "input_image");
}

/// The ChatGPT backend streams output items but completes with an empty
/// output list; replay must still carry the function call for its result.
#[tokio::test]
async fn responses_replay_keeps_streamed_calls_when_completed_output_is_empty() {
    let reasoning = json!({"type":"reasoning","id":"reasoning-1","encrypted_content":"opaque"});
    let call = json!({"type":"function_call","id":"fc-1","call_id":"call-1","name":"file_read","arguments":"{\"path\":\"a\"}"});
    let payload = sse(&[
        json!({"type":"response.output_item.done","output_index":0,"item":reasoning}),
        json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc-1","call_id":"call-1","name":"file_read","arguments":""}}),
        json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"path\":\"a\"}"}),
        json!({"type":"response.output_item.done","output_index":1,"item":call}),
        json!({"type":"response.completed","response":{"status":"completed","model":"fixture-model","output":[],"usage":{"input_tokens":8,"output_tokens":3}}}),
    ]);
    let (endpoint, server) = fixture(payload, 200, "text/event-stream").await;
    let native = provider(ApiFamily::OpenAiResponses, endpoint);
    let response = collect(&native, &[Message::user("query")]).await.unwrap();
    server.await.unwrap();
    assert_eq!(response.message.tool_calls.len(), 1);
    let replay = request::body(
        &native,
        &[
            Message::user("query"),
            response.message,
            Message::tool_result("call-1", "contents"),
        ],
        &[],
        100,
        crate::model::Reasoning::Configured,
    )
    .unwrap();
    let input = replay["input"].as_array().unwrap();
    let call_index = input
        .iter()
        .position(|item| item["type"] == "function_call" && item["call_id"] == "call-1")
        .expect("the streamed call is replayed");
    let output_index = input
        .iter()
        .position(|item| item["type"] == "function_call_output" && item["call_id"] == "call-1")
        .unwrap();
    assert!(call_index < output_index);
    assert!(
        input
            .iter()
            .any(|item| item["encrypted_content"] == "opaque")
    );
}

/// Native replay items that lack a call (e.g. reasoning only) still produce
/// the call, so a tool result never arrives without it.
#[test]
fn responses_replay_adds_calls_missing_from_native_items() {
    let native = provider(
        ApiFamily::OpenAiResponses,
        "https://example.invalid/v1".into(),
    );
    let mut assistant =
        Message::assistant_with_tool_calls("", vec![ToolCall::new("call-2", "file_read", "{}")]);
    assistant.native = Some(NativeContent::Responses(vec![
        json!({"type":"reasoning","id":"r","encrypted_content":"x"}),
    ]));
    let body = request::body(
        &native,
        &[
            Message::user("q"),
            assistant,
            Message::tool_result("call-2", "ok"),
        ],
        &[],
        100,
        crate::model::Reasoning::Configured,
    )
    .unwrap();
    let calls = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] == "function_call" && item["call_id"] == "call-2")
        .count();
    assert_eq!(calls, 1);
}

#[tokio::test]
async fn stream_without_terminal_and_late_responses_failure_are_errors() {
    for (family, payload, retryable) in [
        (
            ApiFamily::OpenAiChat,
            sse(&[json!({"choices":[{"delta":{"content":"partial"},"finish_reason":null}]})]),
            true,
        ),
        (
            ApiFamily::Gemini,
            sse(&[json!({"candidates":[{"content":{"parts":[{"text":"partial"}]}}]})]),
            true,
        ),
        (
            ApiFamily::Anthropic,
            sse(&[json!({"type":"message_start","message":{"usage":{}}})]),
            true,
        ),
        (
            ApiFamily::OpenAiResponses,
            sse(&[
                json!({"type":"response.output_text.delta","delta":"partial"}),
                json!({"type":"response.failed","response":{"error":{"code":"subscription_sharing_usage_limit_exceeded"}}}),
            ]),
            false,
        ),
    ] {
        let (endpoint, server) = fixture(payload, 200, "text/event-stream").await;
        let native = provider(family.clone(), endpoint);
        let error = collect(&native, &[Message::user("query")])
            .await
            .unwrap_err();
        assert_eq!(error.retryable(), retryable, "{family:?}: {error}");
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
    let error = collect(
        &provider(ApiFamily::OpenAiChat, endpoint),
        &[Message::user("query")],
    )
    .await
    .unwrap_err();
    assert!(matches!(error, ModelError::Authentication(_)));
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
    let body = request::body(
        &native,
        &[Message::system("instructions"), Message::user("query")],
        &[tool_schema()],
        100,
        crate::model::Reasoning::Configured,
    )
    .unwrap();
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], true);
    assert!(body.get("max_output_tokens").is_none());
    assert_eq!(body["tools"][0]["type"], "namespace");
    assert_eq!(body["tools"][0]["tools"][0]["type"], "function");
    assert_eq!(body["instructions"], "instructions");
}

#[tokio::test]
async fn bedrock_native_binary_stream_retains_signed_reasoning_tools_images_and_usage() {
    let payload = [
        aws_frame("messageStart", json!({"role":"assistant"})),
        aws_frame(
            "contentBlockDelta",
            json!({"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"think","signature":"signed"}}}),
        ),
        aws_frame(
            "contentBlockDelta",
            json!({"contentBlockIndex":1,"delta":{"text":"Hello 🦀"}}),
        ),
        aws_frame(
            "contentBlockStart",
            json!({"contentBlockIndex":5,"start":{"toolUse":{"toolUseId":"bedrock-call","name":"lookup"}}}),
        ),
        aws_frame(
            "contentBlockDelta",
            json!({"contentBlockIndex":5,"delta":{"toolUse":{"input":"{\"q\":"}}}),
        ),
        aws_frame(
            "contentBlockDelta",
            json!({"contentBlockIndex":5,"delta":{"toolUse":{"input":"\"done\"}"}}}),
        ),
        aws_frame("messageStop", json!({"stopReason":"tool_use"})),
        aws_frame(
            "metadata",
            json!({"usage":{"inputTokens":8,"outputTokens":4,"totalTokens":12,"cacheReadInputTokens":20,"cacheWriteInputTokens":2}}),
        ),
    ]
    .concat();
    let (endpoint, server) = fixture(payload, 200, "application/vnd.amazon.eventstream").await;
    let native = build(
        &config(ApiFamily::Bedrock, endpoint.clone()),
        Some("bedrock-key".into()),
    )
    .unwrap();
    let response = collect(native.as_ref(), &[image_message()]).await.unwrap();
    assert_eq!(response.message.content, "Hello 🦀");
    assert_eq!(response.message.tool_calls.len(), 1);
    assert_eq!(response.message.tool_calls[0].arguments, "{\"q\":\"done\"}");
    let usage = response.usage.unwrap();
    assert_eq!(
        usage.input_tokens, 30,
        "cached tokens count as prompt tokens"
    );
    assert_eq!(
        (usage.cache_read_tokens, usage.cache_write_tokens),
        (Some(20), Some(2))
    );
    let Some(NativeContent::Bedrock(blocks)) = &response.message.native else {
        panic!("native blocks missing");
    };
    assert_eq!(
        blocks[0]["reasoningContent"]["reasoningText"]["signature"],
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
    assert_eq!(response.message.role, Role::Assistant);
}

#[tokio::test]
async fn bedrock_signed_requests_use_selected_credentials_and_reject_truncated_or_corrupt_frames() {
    let payload = [
        aws_frame(
            "contentBlockDelta",
            json!({"contentBlockIndex":0,"delta":{"text":"done"}}),
        ),
        aws_frame("messageStop", json!({"stopReason":"end_turn"})),
    ]
    .concat();
    let (endpoint, server) = fixture(payload, 200, "application/vnd.amazon.eventstream").await;
    let native = build(
        &config(ApiFamily::Bedrock, endpoint),
        Some(json!({"access_key_id":"selected-id","secret_access_key":"selected-secret","session_token":"selected-session"}).to_string()),
    )
    .unwrap();
    collect(native.as_ref(), &[Message::user("query")])
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
            collect(native.as_ref(), &[Message::user("query")])
                .await
                .is_err()
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn bedrock_requires_a_valid_unfiltered_terminal_reason() {
    for stop in [
        None,
        Some("content_filtered"),
        Some("guardrail_intervened"),
        Some("unknown_reason"),
    ] {
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
            collect(native.as_ref(), &[Message::user("query")])
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
        let body = request::body(
            &provider,
            &[Message::user("query")],
            &[],
            100,
            crate::model::Reasoning::Configured,
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
fn unsupported_models_do_not_receive_saved_reasoning() {
    for (family, model) in [
        (ApiFamily::OpenAiChat, "gpt-4.1"),
        (ApiFamily::Anthropic, "claude-sonnet-4"),
    ] {
        let mut config = config(family, "http://127.0.0.1:8080/v1".into());
        config.model = model.into();
        config.reasoning_effort = Some("high".into());
        let provider = NativeProvider::new(config, ProviderCredential::None).unwrap();
        let body = request::body(
            &provider,
            &[Message::user("query")],
            &[],
            100,
            crate::model::Reasoning::Configured,
        )
        .unwrap();
        assert!(body.get("reasoning_effort").is_none() && body.get("reasoning").is_none());
    }
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
        let body = request::body(
            &provider,
            &[Message::user("query")],
            &[],
            100,
            crate::model::Reasoning::Configured,
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

#[tokio::test]
async fn sse_request_rejection_preserves_safe_detail_and_redacts_reflected_credentials() {
    let payload = sse(&[
        json!({"type":"error","request_id":"req_fixture123","error":{"type":"invalid_request_error","code":"invalid_tool_output","param":"input[7].call_id","message":"No tool output found for function call 'call_private'. Authorization: Bearer fixture-key. Check https://user:password@provider.invalid/trace?api_key=private-query and API key sk-reflected-unknown-secret."}}),
    ]);
    let (endpoint, server) = fixture(payload, 200, "text/event-stream").await;
    let error = collect(
        &provider(ApiFamily::OpenAiResponses, endpoint),
        &[Message::user("query")],
    )
    .await
    .unwrap_err();
    assert!(!error.retryable());
    let error = error.to_string();
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
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("request=req_transport_fixture"), "{error}");
    assert!(error.contains("Invalid request field input"), "{error}");
    server.await.unwrap();
}

#[tokio::test]
async fn http_failures_are_classified_by_status_and_code_not_by_request_ids() {
    for (status, code, retryable, rate_limited) in [
        (429, "rate_limit_exceeded", true, true),
        (503, "server_error", true, false),
        (500, "req_503abc", true, false),
        (400, "req_503abc", false, false),
        (404, "model_not_found", false, false),
    ] {
        let (endpoint, server) = fixture(
            json!({"error":{"code":code}}).to_string().into_bytes(),
            status,
            "application/json",
        )
        .await;
        let error = collect(
            &provider(ApiFamily::OpenAiResponses, endpoint),
            &[Message::user("q")],
        )
        .await
        .unwrap_err();
        assert_eq!(error.retryable(), retryable, "{status} {code}: {error}");
        assert_eq!(
            matches!(error, ModelError::RateLimited { .. }),
            rate_limited
        );
        server.await.unwrap();
    }
    let payload = sse(&[
        json!({"type":"error","request_id":"req_connection_503abc","error":{"type":"invalid_request_error","message":"Invalid input field 'input'."}}),
    ]);
    let (endpoint, server) = fixture(payload, 200, "text/event-stream").await;
    let error = collect(
        &provider(ApiFamily::OpenAiResponses, endpoint),
        &[Message::user("q")],
    )
    .await
    .unwrap_err();
    assert!(!error.retryable());
    server.await.unwrap();
    let overloaded =
        sse(&[json!({"type":"error","error":{"type":"overloaded_error","message":"busy"}})]);
    let (endpoint, server) = fixture(overloaded, 200, "text/event-stream").await;
    let error = collect(
        &provider(ApiFamily::Anthropic, endpoint),
        &[Message::user("q")],
    )
    .await
    .unwrap_err();
    assert!(error.retryable());
    server.await.unwrap();
}

#[tokio::test]
async fn rate_limit_retry_after_is_reported() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = BufReader::new(socket);
        let mut length = 0;
        loop {
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse::<usize>().unwrap();
            }
            if line == "\r\n" {
                break;
            }
        }
        let mut body = vec![0; length];
        socket.read_exact(&mut body).await.unwrap();
        let payload = r#"{"error":{"code":"rate_limit_exceeded"}}"#;
        socket
            .get_mut()
            .write_all(format!("HTTP/1.1 429 Slow\r\nRetry-After: 60\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}", payload.len()).as_bytes())
            .await
            .unwrap();
    });
    let error = collect(
        &provider(ApiFamily::OpenAiChat, endpoint),
        &[Message::user("q")],
    )
    .await
    .unwrap_err();
    assert_eq!(error.retry_after(), Some(Duration::from_secs(60)));
    server.await.unwrap();
}

#[test]
fn retry_after_accepts_delta_seconds_and_http_dates() {
    assert_eq!(retry_after_seconds("17", 0), Some(17));
    assert_eq!(
        retry_after_seconds("Thu, 01 Jan 1970 00:00:30 GMT", 10),
        Some(20)
    );
    assert_eq!(
        retry_after_seconds("Thu, 01 Jan 1970 00:00:30 GMT", 50),
        Some(0)
    );
    assert_eq!(retry_after_seconds("req_429", 0), None);
}

#[tokio::test]
async fn only_exact_chatgpt_http401_token_expired_is_eligible_for_renewal() {
    for (chatgpt, status, code, eligible) in [
        (true, 401, "token_expired", true),
        (true, 403, "token_expired", false),
        (true, 401, "invalid_api_key", false),
        (false, 401, "token_expired", false),
    ] {
        let (endpoint, recorded) = fixture(
            serde_json::to_vec(&json!({"error":{"code":code}})).unwrap(),
            status,
            "application/json",
        )
        .await;
        let mut provider = provider(ApiFamily::OpenAiResponses, endpoint);
        // Only the fixture's classification flag changes; the constructor
        // still refuses ChatGPT credentials at a custom or loopback origin.
        provider.chatgpt_plan = chatgpt;
        let error = collect(&provider, &[Message::user("local fixture")])
            .await
            .unwrap_err();
        assert_eq!(
            matches!(&error, ModelError::Authentication(message) if message == renewal::TOKEN_EXPIRED),
            eligible
        );
        recorded.await.unwrap();
    }
}

#[tokio::test]
async fn cancelling_an_open_stream_ends_it_promptly() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = BufReader::new(socket);
        loop {
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            if line == "\r\n" {
                break;
            }
        }
        socket.get_mut().write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n").await.unwrap();
        tokio::time::sleep(Duration::from_secs(30)).await;
    });
    let native = provider(ApiFamily::OpenAiChat, endpoint);
    let cancel = CancellationToken::new();
    let mut stream = native.stream(
        ModelRequest {
            messages: vec![Message::user("q")],
            tools: Vec::new(),
            max_tokens: 16,
            reasoning: crate::model::Reasoning::Configured,
        },
        cancel.clone(),
    );
    assert!(matches!(stream.next().await, Some(Ok(_))));
    cancel.cancel();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .unwrap(),
        Some(Err(ModelError::Interrupted))
    );
    server.abort();
}

#[tokio::test]
async fn anthropic_tool_without_arguments_streams_an_empty_object() {
    for with_delta in [true, false] {
        let mut events = vec![
            json!({"type":"message_start","message":{"usage":{"input_tokens":3}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"git_status","input":{}}}),
        ];
        if with_delta {
            events.push(json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":""}}));
        }
        events.extend([
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":2}}),
            json!({"type":"message_stop"}),
        ]);
        let (endpoint, server) = fixture(sse(&events), 200, "text/event-stream").await;
        let response = collect(
            &provider(ApiFamily::Anthropic, endpoint),
            &[Message::user("q")],
        )
        .await
        .unwrap();
        assert_eq!(response.message.tool_calls[0].arguments, "{}");
        assert_eq!(response.finish, FinishReason::ToolCalls);
        let Some(NativeContent::Anthropic(blocks)) = &response.message.native else {
            panic!("native blocks missing");
        };
        assert_eq!(blocks[0]["input"], json!({}));
        server.await.unwrap();
    }
}

#[tokio::test]
async fn anthropic_tool_input_cut_by_the_output_limit_finishes_with_length() {
    let events = [
        json!({"type":"message_start","message":{"usage":{"input_tokens":3}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"file_write","input":{}}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":100}}),
        json!({"type":"message_stop"}),
    ];
    let (endpoint, server) = fixture(sse(&events), 200, "text/event-stream").await;
    let response = collect(
        &provider(ApiFamily::Anthropic, endpoint),
        &[Message::user("q")],
    )
    .await
    .unwrap();
    assert_eq!(response.finish, FinishReason::Length);
    // The cut-off call is reported and its partial JSON cleared, so it is
    // never dispatched and later requests replay valid arguments.
    assert_eq!(response.truncated_calls.len(), 1);
    assert_eq!(response.message.tool_calls[0].arguments, "{}");
    server.await.unwrap();
    // The same cut-off input without the output limit is a provider fault.
    let mut events = events.to_vec();
    events[4] = json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}});
    let (endpoint, server) = fixture(sse(&events), 200, "text/event-stream").await;
    assert!(
        collect(
            &provider(ApiFamily::Anthropic, endpoint),
            &[Message::user("q")]
        )
        .await
        .is_err()
    );
    server.await.unwrap();
}

#[tokio::test]
async fn bedrock_tool_without_arguments_and_cut_off_arguments_are_not_turn_failures() {
    for (input, stop, arguments, finish) in [
        ("", "tool_use", "{}", FinishReason::ToolCalls),
        ("{\"path\":\"a", "max_tokens", "{}", FinishReason::Length),
    ] {
        let payload = [
            aws_frame(
                "contentBlockStart",
                json!({"contentBlockIndex":0,"start":{"toolUse":{"toolUseId":"b1","name":"git_status"}}}),
            ),
            aws_frame(
                "contentBlockDelta",
                json!({"contentBlockIndex":0,"delta":{"toolUse":{"input":input}}}),
            ),
            aws_frame("messageStop", json!({"stopReason":stop})),
        ]
        .concat();
        let (endpoint, server) = fixture(payload, 200, "application/vnd.amazon.eventstream").await;
        let native = build(&config(ApiFamily::Bedrock, endpoint), Some("key".into())).unwrap();
        let response = collect(native.as_ref(), &[Message::user("q")])
            .await
            .unwrap();
        assert_eq!(response.message.tool_calls[0].arguments, arguments);
        assert_eq!(response.finish, finish);
        assert_eq!(
            response.truncated_calls.len(),
            usize::from(finish == FinishReason::Length)
        );
        let Some(NativeContent::Bedrock(blocks)) = &response.message.native else {
            panic!("native blocks missing");
        };
        assert_eq!(blocks[0]["toolUse"]["input"], json!({}));
        server.await.unwrap();
    }
}

#[tokio::test]
async fn responses_cut_off_by_the_output_limit_keep_their_text_with_length() {
    let payload = sse(&[
        json!({"type":"response.output_text.delta","delta":"Partial ans"}),
        json!({"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Partial ans"}]}],"usage":{"input_tokens":5,"output_tokens":64}}}),
    ]);
    let (endpoint, server) = fixture(payload, 200, "text/event-stream").await;
    let response = collect(
        &provider(ApiFamily::OpenAiResponses, endpoint),
        &[Message::user("q")],
    )
    .await
    .unwrap();
    assert_eq!(response.finish, FinishReason::Length);
    assert_eq!(response.message.content, "Partial ans");
    server.await.unwrap();
    let filtered = sse(&[
        json!({"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"content_filter"},"output":[]}}),
    ]);
    let (endpoint, server) = fixture(filtered, 200, "text/event-stream").await;
    assert!(
        collect(
            &provider(ApiFamily::OpenAiResponses, endpoint),
            &[Message::user("q")]
        )
        .await
        .is_err()
    );
    server.await.unwrap();
}

#[test]
fn lowest_reasoning_picks_the_smallest_supported_effort() {
    for family in [ApiFamily::OpenAiChat, ApiFamily::OpenAiResponses] {
        let mut config = config(family.clone(), "http://127.0.0.1:8080/v1".into());
        config.reasoning_options = Some(vec!["high".into(), "low".into(), "medium".into()]);
        config.reasoning_effort = Some("high".into());
        let provider = NativeProvider::new(config, ProviderCredential::None).unwrap();
        let effort = |reasoning| {
            let body = request::body(&provider, &[Message::user("q")], &[], 64, reasoning).unwrap();
            if family == ApiFamily::OpenAiResponses {
                body["reasoning"]["effort"].clone()
            } else {
                body["reasoning_effort"].clone()
            }
        };
        assert_eq!(effort(crate::model::Reasoning::Configured), "high");
        assert_eq!(effort(crate::model::Reasoning::Lowest), "low");
    }
}

/// Finish reasons that mean "out of room" keep the partial reply as a
/// `Length` finish, and a plain end-of-sequence is a stop.
#[tokio::test]
async fn output_room_finish_reasons_map_to_length_and_eos_to_stop() {
    for (reason, expected) in [
        ("model_length", FinishReason::Length),
        ("eos_token", FinishReason::Stop),
        ("eos", FinishReason::Stop),
    ] {
        let mut payload = sse(&[
            json!({"choices":[{"delta":{"content":"Partial"},"finish_reason":null}]}),
            json!({"choices":[{"delta":{},"finish_reason":reason}]}),
        ]);
        payload.extend_from_slice(b"data: [DONE]\r\n\r\n");
        let (endpoint, server) = fixture(payload, 200, "text/event-stream").await;
        let response = collect(
            &provider(ApiFamily::OpenAiChat, endpoint),
            &[Message::user("q")],
        )
        .await
        .unwrap();
        assert_eq!(response.finish, expected, "{reason}");
        assert_eq!(response.message.content, "Partial");
        server.await.unwrap();
    }
    let events = [
        json!({"type":"message_start","message":{"usage":{"input_tokens":3}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Cut"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"model_context_window_exceeded"},"usage":{"output_tokens":9}}),
        json!({"type":"message_stop"}),
    ];
    let (endpoint, server) = fixture(sse(&events), 200, "text/event-stream").await;
    let response = collect(
        &provider(ApiFamily::Anthropic, endpoint),
        &[Message::user("q")],
    )
    .await
    .unwrap();
    assert_eq!(response.finish, FinishReason::Length);
    assert_eq!(response.message.content, "Cut");
    server.await.unwrap();
}

/// Gemini's malformed function call is a recoverable finish, not a failure.
#[tokio::test]
async fn gemini_malformed_function_call_is_a_recoverable_finish() {
    let payload = sse(&[
        json!({"candidates":[{"content":{"parts":[{"text":"Calling"}]},"finishReason":"MALFORMED_FUNCTION_CALL"}]}),
    ]);
    let (endpoint, server) = fixture(payload, 200, "text/event-stream").await;
    let response = collect(
        &provider(ApiFamily::Gemini, endpoint),
        &[Message::user("q")],
    )
    .await
    .unwrap();
    assert_eq!(response.finish, FinishReason::MalformedToolCall);
    assert!(response.message.tool_calls.is_empty());
    assert_eq!(response.message.content, "Calling");
    server.await.unwrap();
}

fn body_for(provider: &NativeProvider, messages: &[Message], tools: &[ToolSchema]) -> Value {
    request::body(
        provider,
        messages,
        tools,
        100,
        crate::model::Reasoning::Configured,
    )
    .unwrap()
}

#[test]
fn anthropic_keeps_the_static_system_prompt_cached_beside_a_summary() {
    let native = provider(ApiFamily::Anthropic, "http://127.0.0.1:1".into());
    let mut summary = Message::system("Summary of earlier work");
    summary.summary = true;
    let body = body_for(
        &native,
        &[Message::system("Policy"), summary, Message::user("Next")],
        &[tool_schema()],
    );
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(body["system"][1]["cache_control"]["type"], "ephemeral");
    assert_eq!(body["tools"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(
        body["messages"][0]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
    assert_eq!(body.to_string().matches("cache_control").count(), 4);
}

#[test]
fn openai_cache_key_is_stable_for_a_conversation_and_omitted_for_other_servers() {
    let responses = provider(ApiFamily::OpenAiResponses, "http://127.0.0.1:1".into());
    let first = [Message::system("Policy"), Message::user("Fix the build")];
    let mut later = first.to_vec();
    later.push(Message::assistant("Done"));
    later.push(Message::user("Now run the tests"));
    let key = body_for(&responses, &first, &[])["prompt_cache_key"].clone();
    assert!(key.as_str().unwrap().starts_with("xylo-"));
    assert_eq!(body_for(&responses, &later, &[])["prompt_cache_key"], key);
    let other = [Message::system("Policy"), Message::user("Another task")];
    assert_ne!(body_for(&responses, &other, &[])["prompt_cache_key"], key);
    assert!(!key.as_str().unwrap().contains("Policy"));
    let local = provider(ApiFamily::OpenAiChat, "http://127.0.0.1:1".into());
    assert!(
        body_for(&local, &first, &[])
            .get("prompt_cache_key")
            .is_none()
    );
    let openai = provider(ApiFamily::OpenAiChat, "https://api.openai.com/v1".into());
    assert!(body_for(&openai, &first, &[])["prompt_cache_key"].is_string());
    let anthropic = provider(ApiFamily::Anthropic, "http://127.0.0.1:1".into());
    assert!(
        body_for(&anthropic, &first, &[])
            .get("prompt_cache_key")
            .is_none()
    );
}

#[test]
fn openai_functions_are_strict_only_when_the_schema_already_complies() {
    let native = provider(ApiFamily::OpenAiResponses, "http://127.0.0.1:1".into());
    let schema = |parameters: Value| ToolSchema {
        name: "tool".into(),
        description: "A tool.".into(),
        parameters,
    };
    let strict = [
        tool_schema(),
        schema(json!({"type":"object","properties":{},"required":[],"additionalProperties":false})),
        schema(
            json!({"type":"object","properties":{"todos":{"type":"array","items":{"type":"object","properties":{"status":{"type":"string","enum":["a","b"]}},"required":["status"],"additionalProperties":false}}},"required":["todos"],"additionalProperties":false}),
        ),
    ];
    let loose = [
        schema(
            json!({"type":"object","properties":{"q":{"type":"string"},"limit":{"type":"integer"}},"required":["q"],"additionalProperties":false}),
        ),
        schema(json!({"type":"object","properties":{"q":{"type":"string"}},"required":["q"]})),
        schema(
            json!({"type":"object","properties":{"n":{"type":"integer","minimum":1}},"required":["n"],"additionalProperties":false}),
        ),
        schema(
            json!({"type":"object","properties":{"o":{"type":"object","properties":{"x":{"type":"string"}},"required":["x"]}},"required":["o"],"additionalProperties":false}),
        ),
        schema(json!({})),
    ];
    for (tools, expected) in [(&strict[..], true), (&loose[..], false)] {
        let body = body_for(&native, &[Message::user("q")], tools);
        for (index, function) in body["tools"].as_array().unwrap().iter().enumerate() {
            assert_eq!(function["strict"], expected, "{index}: {function}");
        }
    }
}

/// The request prefix a provider caches (tools, then system) must be
/// byte-identical between two turns of one conversation, whatever the new
/// messages say.
#[test]
fn cached_prefix_is_byte_identical_across_turns() {
    for family in [ApiFamily::Anthropic, ApiFamily::OpenAiResponses] {
        let native = provider(family.clone(), "http://127.0.0.1:1".into());
        let tools = [tool_schema()];
        let turn_one = [Message::system("Policy"), Message::user("First prompt")];
        let mut turn_two = turn_one.to_vec();
        turn_two.push(Message::assistant("First answer"));
        turn_two.push(Message::user("A different second prompt"));
        let one = body_for(&native, &turn_one, &tools);
        let two = body_for(&native, &turn_two, &tools);
        for field in ["tools", "system", "instructions", "prompt_cache_key"] {
            assert_eq!(
                serde_json::to_string(&one[field]).unwrap(),
                serde_json::to_string(&two[field]).unwrap(),
                "{family:?} {field}"
            );
        }
    }
}

#[test]
fn context_overflow_is_recognised_from_codes_and_messages() {
    let native = provider(ApiFamily::OpenAiChat, "http://127.0.0.1:1".into());
    let overflow = |status, code, reason: Option<&str>| {
        matches!(
            native.http_error(status, code, reason, None, &json!({})),
            ModelError::ContextOverflow(_)
        )
    };
    assert!(overflow(400, "context_length_exceeded", None));
    // llama.cpp reports a numeric code and says it in the message.
    assert!(overflow(
        400,
        "request_failed",
        Some("the request exceeds the available context size, try increasing it")
    ));
    assert!(overflow(
        400,
        "invalid_request_error",
        Some("prompt is too long: 210000 tokens > 200000 maximum")
    ));
    assert!(overflow(
        400,
        "request_failed",
        Some(
            "This model's maximum context length is 8192 tokens. However, you requested 9000 tokens."
        )
    ));
    assert!(!overflow(
        400,
        "invalid_request_error",
        Some("messages: field required")
    ));
    assert!(!overflow(500, "request_failed", Some("prompt is too long")));
}
