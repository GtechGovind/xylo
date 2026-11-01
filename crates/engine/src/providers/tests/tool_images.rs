use super::*;
#[test]
fn all_http_protocols_preserve_images_from_tool_results_and_call_identity() {
    for family in [
        ApiFamily::OpenAiChat,
        ApiFamily::OpenAiResponses,
        ApiFamily::Anthropic,
        ApiFamily::Gemini,
    ] {
        let provider = provider(family.clone(), "https://example.com".into());
        let mut result = Message::tool_result("image-call", "image result");
        result.images = image_message().images;
        let messages = vec![
            Message::user("Read this image"),
            Message::assistant_with_tool_calls(
                "",
                vec![ToolCall::new(
                    "image-call",
                    "file_read",
                    r#"{"path":"a.png"}"#,
                )],
            ),
            result,
        ];
        let body = request::body(
            &provider,
            &messages,
            &[],
            256,
            crate::model::Reasoning::Configured,
        )
        .unwrap();
        let encoded = body.to_string();
        assert!(encoded.contains("aGVsbG8="), "{family:?}: {encoded}");
        assert!(encoded.contains("image-call"), "{family:?}: {encoded}");
        match family {
            ApiFamily::OpenAiChat => assert_eq!(body["messages"][3]["role"], "user"),
            ApiFamily::OpenAiResponses => {
                assert_eq!(body["input"][2]["output"][1]["type"], "input_image")
            }
            ApiFamily::Anthropic => assert_eq!(
                body["messages"][2]["content"][0]["content"][1]["type"],
                "image"
            ),
            ApiFamily::Gemini => assert_eq!(
                body["contents"][2]["parts"][1]["inlineData"]["mimeType"],
                "image/png"
            ),
            _ => unreachable!(),
        }
    }
}
#[test]
fn chat_keeps_all_parallel_tool_responses_before_image_followup() {
    let provider = provider(ApiFamily::OpenAiChat, "https://example.com".into());
    let mut image = Message::tool_result("a", "image");
    image.images = image_message().images;
    let messages = vec![
        Message::assistant_with_tool_calls(
            "",
            vec![
                ToolCall::new("a", "file_read", "{}"),
                ToolCall::new("b", "file_read", "{}"),
            ],
        ),
        image,
        Message::tool_result("b", "text"),
    ];
    let body = request::body(
        &provider,
        &messages,
        &[],
        256,
        crate::model::Reasoning::Configured,
    )
    .unwrap();
    assert_eq!(body["messages"][1]["role"], "tool");
    assert_eq!(body["messages"][2]["role"], "tool");
    assert_eq!(body["messages"][3]["role"], "user");
}

#[tokio::test]
async fn managed_tool_images_rehydrate_and_stream_for_every_api_family() {
    use crate::attachments::AttachmentProvider;
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
        };
        let message = Message::tool_result(
            "tool-image",
            json!({"images":turn.attachments,"path":"fixture.png"}).to_string(),
        );
        let messages = vec![
            Message::user("Read image"),
            Message::assistant_with_tool_calls(
                "",
                vec![ToolCall::new(
                    "tool-image",
                    "file_read",
                    r#"{"path":"fixture.png"}"#,
                )],
            ),
            message.clone(),
        ];
        let durable = serde_json::to_string(&message).unwrap();
        assert!(!durable.contains("base64"));
        assert!(!durable.contains("data:image"));
        let response = collect(&wrapped, &messages).await.unwrap();
        assert_eq!(response.message.content, "Seen fixture image");
        assert_eq!(serde_json::to_string(&message).unwrap(), durable);
        let body = server.await.unwrap().body;
        assert!(
            body.to_string().contains(&STANDARD.encode(&expected)),
            "Image bytes missing for {family:?}: {body}"
        );
        assert!(body.to_string().contains("tool-image"));
    }
}
