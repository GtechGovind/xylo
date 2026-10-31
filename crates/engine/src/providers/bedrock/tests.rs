use super::*;
use crate::model::{Assembler, InlineImage, ToolCall};

pub(crate) fn frame(event_type: &str, payload: Value) -> Vec<u8> {
    let payload = serde_json::to_vec(&payload).unwrap();
    let mut headers = Vec::new();
    for (name, value) in [
        (":message-type", "event"),
        (":event-type", event_type),
        (":content-type", "application/json"),
    ] {
        headers.push(name.len() as u8);
        headers.extend_from_slice(name.as_bytes());
        headers.push(7);
        headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
        headers.extend_from_slice(value.as_bytes());
    }
    let total = 16 + headers.len() + payload.len();
    let mut frame = Vec::with_capacity(total);
    frame.extend_from_slice(&(total as u32).to_be_bytes());
    frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    frame.extend_from_slice(&crc32(&frame[..8]).to_be_bytes());
    frame.extend_from_slice(&headers);
    frame.extend_from_slice(&payload);
    frame.extend_from_slice(&crc32(&frame).to_be_bytes());
    frame
}

fn decode_all(bytes: &[u8]) -> Result<Vec<StreamEvent>, ModelError> {
    let mut buffer = bytes.to_vec();
    let mut decoder = StreamDecoder::default();
    let mut events = Vec::new();
    while let Some(frame) = take_frame(&mut buffer)? {
        if let Some(event) = decode_frame(&frame)? {
            events.extend(decoder.event(&event)?);
        }
    }
    decoder.finish()?;
    Ok(events)
}

#[test]
fn crc32_matches_the_standard_check_value() {
    assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
}

#[test]
fn partial_frames_wait_and_whole_frames_decode_text_reasoning_tools_and_usage() {
    let frames = [
        frame(
            "contentBlockDelta",
            json!({"contentBlockDelta":{"contentBlockIndex":0,"delta":{"text":"Hello"}}}),
        ),
        frame(
            "contentBlockDelta",
            json!({"contentBlockDelta":{"contentBlockIndex":1,"delta":{"reasoningContent":{"text":"thinking","signature":"sig"}}}}),
        ),
        frame(
            "contentBlockStart",
            json!({"contentBlockStart":{"contentBlockIndex":2,"start":{"toolUse":{"toolUseId":"tool_1","name":"file_read"}}}}),
        ),
        frame(
            "contentBlockDelta",
            json!({"contentBlockDelta":{"contentBlockIndex":2,"delta":{"toolUse":{"input":"{\"path\":\"f\"}"}}}}),
        ),
        frame(
            "messageStop",
            json!({"messageStop":{"stopReason":"tool_use"}}),
        ),
        frame(
            "metadata",
            json!({"metadata":{"usage":{"inputTokens":5,"outputTokens":3}}}),
        ),
    ];
    let mut buffer = frames[0][..frames[0].len() / 2].to_vec();
    assert!(take_frame(&mut buffer).unwrap().is_none());
    let events = decode_all(&frames.concat()).unwrap();
    let mut assembler = Assembler::default();
    for event in events {
        assembler.push(event);
    }
    let response = assembler.finish().unwrap();
    assert_eq!(response.message.content, "Hello");
    assert_eq!(response.message.reasoning.as_deref(), Some("thinking"));
    assert_eq!(
        response.message.tool_calls,
        vec![ToolCall::new("tool_1", "file_read", "{\"path\":\"f\"}")]
    );
    assert_eq!(response.finish, FinishReason::ToolCalls);
    assert_eq!(response.usage.unwrap().input_tokens, 5);
    let Some(NativeContent::Bedrock(blocks)) = response.message.native else {
        panic!("native blocks missing");
    };
    assert_eq!(
        blocks[1]["reasoningContent"]["reasoningText"]["signature"],
        "sig"
    );
    assert_eq!(blocks[2]["toolUse"]["input"]["path"], "f");
}

#[test]
fn corrupt_truncated_unfinished_and_filtered_streams_fail() {
    let mut corrupt = frame("metadata", json!({"metadata":{"usage":{}}}));
    let last = corrupt.len() - 1;
    corrupt[last] ^= 0xff;
    assert!(
        decode_all(&corrupt)
            .unwrap_err()
            .to_string()
            .contains("checksum")
    );
    let text = frame(
        "contentBlockDelta",
        json!({"contentBlockDelta":{"contentBlockIndex":0,"delta":{"text":"partial"}}}),
    );
    let unfinished = decode_all(&text).unwrap_err();
    assert!(unfinished.retryable());
    let filtered = frame(
        "messageStop",
        json!({"messageStop":{"stopReason":"guardrail_intervened"}}),
    );
    assert!(
        decode_all(&filtered)
            .unwrap_err()
            .to_string()
            .contains("filtered")
    );
    let missing = frame("messageStop", json!({"messageStop":{}}));
    assert!(decode_all(&missing).is_err());
}

#[test]
fn stream_exceptions_map_to_typed_errors() {
    let throttled = frame(
        "throttlingException",
        json!({"throttlingException":{"message":"slow down"}}),
    );
    assert!(matches!(
        decode_all(&throttled),
        Err(ModelError::RateLimited { .. })
    ));
    let invalid = frame(
        "validationException",
        json!({"validationException":{"message":"bad input"}}),
    );
    assert!(!decode_all(&invalid).unwrap_err().retryable());
}

#[test]
fn http_errors_classify_without_reflecting_bodies() {
    let auth = map_error(403, r#"{"message":"secret-echo"}"#);
    assert!(matches!(auth, ModelError::Authentication(_)));
    assert!(!auth.to_string().contains("secret-echo"));
    assert!(matches!(
        map_error(
            400,
            r#"{"message":"Input is too long for requested model"}"#
        ),
        ModelError::ContextOverflow(_)
    ));
    let invalid = map_error(400, r#"{"message":"rate limit connection reset"}"#);
    assert!(!invalid.retryable());
    let unavailable = with_retry_after(map_error(503, "{}"), Some(Duration::from_secs(12)));
    assert!(unavailable.retryable());
    assert_eq!(unavailable.retry_after(), Some(Duration::from_secs(12)));
}

#[test]
fn converse_body_replays_native_blocks_and_maps_images_tools_and_boundaries() {
    let mut user = Message::user("What is shown?");
    user.images.push(InlineImage {
        media_type: "image/png".into(),
        data: "aGVsbG8=".into(),
    });
    let mut native =
        Message::assistant_with_tool_calls("", vec![ToolCall::new("t1", "lookup", "{}")]);
    native.native = Some(NativeContent::Bedrock(vec![
        json!({"reasoningContent":{"reasoningText":{"text":"r","signature":"s"}}}),
        json!({"toolUse":{"toolUseId":"t1","name":"lookup","input":{}}}),
    ]));
    let body = converse_body(
        "us.anthropic.claude-sonnet",
        &[
            Message::system("Policy"),
            user,
            native,
            Message::tool_result("t1", "done"),
            Message::assistant("Finished"),
        ],
        &[ToolSchema {
            name: "lookup".into(),
            description: "Look up".into(),
            parameters: json!({}),
        }],
        64,
    )
    .unwrap();
    assert_eq!(body["system"][0]["text"], "Policy");
    assert_eq!(body["messages"][0]["content"][1]["image"]["format"], "png");
    assert_eq!(
        body["messages"][1]["content"][0]["reasoningContent"]["reasoningText"]["signature"],
        "s"
    );
    assert_eq!(
        body["messages"][2]["content"][0]["toolResult"]["toolUseId"],
        "t1"
    );
    assert_eq!(
        body["messages"].as_array().unwrap().last().unwrap()["role"],
        "user"
    );
    assert_eq!(
        body["toolConfig"]["tools"][0]["toolSpec"]["inputSchema"]["json"]["type"],
        "object"
    );
    assert_eq!(
        body["additionalModelRequestFields"]["anthropic_beta"][0],
        ANTHROPIC_BETAS[0]
    );
}

#[test]
fn sigv4_headers_sign_the_double_encoded_path_with_the_session_token() {
    let auth = BedrockAuth::SigV4(AwsCredentials {
        access_key_id: "AKIDEXAMPLE".into(),
        secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
        session_token: Some("session".into()),
    });
    let request = HeaderRequest {
        method: "POST",
        url: "https://bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude%3A0/converse-stream",
        region: "us-east-1",
        body: br#"{"messages":[]}"#,
        anthropic_beta: Some(ANTHROPIC_BETAS[0]),
        now: DateTime::parse_from_rfc3339("2026-05-30T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
    };
    let headers = request_headers(request, &auth).unwrap();
    assert_eq!(headers["x-amz-date"], "20260530T000000Z");
    assert_eq!(headers["x-amz-security-token"], "session");
    assert!(headers["authorization"].starts_with(
        "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20260530/us-east-1/bedrock/aws4_request, SignedHeaders=accept;anthropic-beta;content-type;host;x-amz-content-sha256;x-amz-date;x-amz-security-token, Signature="
    ));
    assert!(!format!("{auth:?}").contains("AKID"));
    let bearer = request_headers(request, &BedrockAuth::Bearer("token".into())).unwrap();
    assert_eq!(bearer["authorization"], "Bearer token");
}

#[test]
fn caching_models_get_checkpoints_after_tools_system_and_the_newest_message() {
    let messages = [
        Message::system("Policy"),
        Message::user("First"),
        Message::assistant("Reply"),
        Message::user("Second"),
    ];
    let tools = [ToolSchema {
        name: "lookup".into(),
        description: "Look up".into(),
        parameters: json!({"type":"object","properties":{}}),
    }];
    let checkpoint = json!({"cachePoint":{"type":"default"}});
    let claude = converse_body("us.anthropic.claude-sonnet", &messages, &tools, 64).unwrap();
    assert_eq!(
        claude["system"].as_array().unwrap().last().unwrap(),
        &checkpoint
    );
    assert_eq!(
        claude["toolConfig"]["tools"]
            .as_array()
            .unwrap()
            .last()
            .unwrap(),
        &checkpoint
    );
    let last = claude["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(last["content"][0]["text"], "Second");
    assert_eq!(
        last["content"].as_array().unwrap().last().unwrap(),
        &checkpoint
    );
    assert_eq!(claude.to_string().matches("cachePoint").count(), 3);
    let nova = converse_body("amazon.nova-pro-v1:0", &messages, &tools, 64).unwrap();
    assert_eq!(nova.to_string().matches("cachePoint").count(), 2);
    assert!(!nova["toolConfig"].to_string().contains("cachePoint"));
    let other = converse_body("meta.llama3-70b", &messages, &tools, 64).unwrap();
    assert!(!other.to_string().contains("cachePoint"));
}

#[test]
fn running_out_of_room_finishes_with_length() {
    for reason in ["max_tokens", "model_context_window_exceeded"] {
        assert_eq!(terminal_reason(Some(reason)).unwrap(), FinishReason::Length);
        let stop = frame("messageStop", json!({"messageStop":{"stopReason":reason}}));
        let events = decode_all(&stop).unwrap();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, StreamEvent::Finished(FinishReason::Length))),
            "{reason}"
        );
    }
}
