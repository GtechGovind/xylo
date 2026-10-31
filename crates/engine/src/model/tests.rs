use super::*;
use serde_json::json;

#[test]
fn checkpoint_messages_keep_the_store_guidance_shape_and_drop_hydrated_images() {
    let mut message = Message::user("Steer here");
    message.name = Some("guidance-1".into());
    message.images.push(InlineImage {
        media_type: "image/png".into(),
        data: "aGVsbG8=".into(),
    });
    let value = serde_json::to_value(&message).unwrap();
    assert_eq!(
        value,
        json!({"role":"user","content":"Steer here","name":"guidance-1"})
    );
    let decoded: Message = serde_json::from_value(value).unwrap();
    assert!(decoded.images.is_empty());
}

#[test]
fn renaming_a_call_updates_native_replay_content() {
    let mut message =
        Message::assistant_with_tool_calls("", vec![ToolCall::new("c1", "read", "{}")]);
    message.native = Some(NativeContent::Anthropic(vec![
        json!({"type":"tool_use","id":"c1","name":"read","input":{}}),
    ]));
    message.rename_tool_call("c1", "file_read");
    assert_eq!(message.tool_calls[0].name, "file_read");
    let Some(NativeContent::Anthropic(items)) = &message.native else {
        panic!("native content lost");
    };
    assert_eq!(items[0]["name"], "file_read");
}

#[test]
fn assembler_joins_fragments_and_reports_tool_calls() {
    let mut assembler = Assembler::default();
    for event in [
        StreamEvent::Text("Hi".into()),
        StreamEvent::ToolCall {
            index: 0,
            id: Some("c".into()),
            name: Some("file_read".into()),
            arguments: Some("{\"pa".into()),
        },
        StreamEvent::ToolCall {
            index: 0,
            id: None,
            name: None,
            arguments: Some("th\":1}".into()),
        },
        StreamEvent::Finished(FinishReason::Stop),
    ] {
        assembler.push(event);
    }
    let response = assembler.finish().unwrap();
    assert_eq!(response.finish, FinishReason::ToolCalls);
    assert_eq!(response.message.tool_calls[0].arguments, "{\"path\":1}");
    assert_eq!(response.message.content, "Hi");
}

#[test]
fn unfinished_stream_is_a_retryable_failure() {
    let mut assembler = Assembler::default();
    assembler.push(StreamEvent::Text("partial".into()));
    assert!(assembler.finish().unwrap_err().retryable());
}

fn assemble(events: Vec<StreamEvent>) -> ModelResponse {
    let mut assembler = Assembler::default();
    for event in events {
        assembler.push(event);
    }
    assembler.finish().unwrap()
}

fn fragment(index: u32, id: &str, arguments: &str) -> StreamEvent {
    StreamEvent::ToolCall {
        index,
        id: Some(id.into()),
        name: Some("file_write".into()),
        arguments: Some(arguments.into()),
    }
}

#[test]
fn calls_cut_off_by_the_output_limit_are_reported_and_cleared() {
    let response = assemble(vec![
        fragment(0, "done", r#"{"path":"a"}"#),
        fragment(1, "cut", r#"{"path":"b","content":"ab"#),
        fragment(2, "unstarted", ""),
        StreamEvent::Native(NativeContent::Responses(vec![
            json!({"type":"function_call","call_id":"done","name":"file_write","arguments":"{\"path\":\"a\"}"}),
            json!({"type":"function_call","call_id":"cut","name":"file_write","arguments":"{\"path\":\"b\",\"content\":\"ab"}),
        ])),
        StreamEvent::Finished(FinishReason::Length),
    ]);
    assert_eq!(response.finish, FinishReason::Length);
    assert_eq!(
        response.truncated_calls,
        vec![
            TruncatedCall {
                position: 1,
                received: 25
            },
            TruncatedCall {
                position: 2,
                received: 0
            }
        ]
    );
    let arguments = response
        .message
        .tool_calls
        .iter()
        .map(|call| call.arguments.as_str())
        .collect::<Vec<_>>();
    assert_eq!(arguments, [r#"{"path":"a"}"#, "{}", "{}"]);
    let Some(NativeContent::Responses(items)) = &response.message.native else {
        panic!("native content lost");
    };
    assert_eq!(items[0]["arguments"], r#"{"path":"a"}"#);
    assert_eq!(items[1]["arguments"], "{}");
}

#[test]
fn without_the_output_limit_empty_arguments_mean_no_parameters() {
    let response = assemble(vec![
        fragment(0, "c", ""),
        StreamEvent::Finished(FinishReason::ToolCalls),
    ]);
    assert!(response.truncated_calls.is_empty());
    assert_eq!(response.message.tool_calls[0].arguments, "{}");
}

#[test]
fn cleared_arguments_reach_every_native_protocol_by_occurrence() {
    let calls = vec![
        ToolCall::new("same", "file_write", r#"{"a":1}"#),
        ToolCall::new("same", "file_write", r#"{"a":"#),
    ];
    for (native, pointer, empty) in [
        (
            NativeContent::Anthropic(vec![
                json!({"type":"tool_use","id":"same","input":{"a":1}}),
                json!({"type":"tool_use","id":"same","input":{"a":2}}),
            ]),
            "/input",
            json!({}),
        ),
        (
            NativeContent::Gemini(vec![
                json!({"functionCall":{"id":"same","args":{"a":1}}}),
                json!({"functionCall":{"id":"same","args":{"a":2}}}),
            ]),
            "/functionCall/args",
            json!({}),
        ),
        (
            NativeContent::Bedrock(vec![
                json!({"toolUse":{"toolUseId":"same","input":{"a":1}}}),
                json!({"toolUse":{"toolUseId":"same","input":{"a":2}}}),
            ]),
            "/toolUse/input",
            json!({}),
        ),
    ] {
        let mut message = Message::assistant_with_tool_calls("", calls.clone());
        message.native = Some(native);
        message.set_tool_call_arguments(1, &json!({}));
        assert_eq!(message.tool_calls[0].arguments, r#"{"a":1}"#);
        assert_eq!(message.tool_calls[1].arguments, "{}");
        let items = match message.native.as_ref().unwrap() {
            NativeContent::Anthropic(items)
            | NativeContent::Gemini(items)
            | NativeContent::Bedrock(items)
            | NativeContent::Responses(items) => items,
        };
        assert_eq!(items[0].pointer(pointer).unwrap(), &json!({"a":1}));
        assert_eq!(items[1].pointer(pointer).unwrap(), &empty);
    }
}

#[test]
fn harness_stops_classify_by_cause() {
    let stalled = ModelError::Stalled("loop".into());
    assert_eq!(stalled.kind(), ErrorKind::Stalled);
    assert!(!stalled.retryable());
    let empty = ModelError::EmptyResponse("empty".into());
    assert_eq!(empty.kind(), ErrorKind::EmptyResponse);
    assert!(empty.retryable());
    assert_eq!(empty.to_string(), "empty");
}

#[test]
fn repaired_arguments_replay_canonically_in_every_protocol() {
    let repaired = json!({"path":"a.rs","limit":5});
    for (native, pointer, replayed) in [
        (
            NativeContent::Anthropic(vec![json!({"type":"tool_use","id":"c","input":{}})]),
            "/input",
            repaired.clone(),
        ),
        (
            NativeContent::Responses(vec![
                json!({"type":"function_call","call_id":"c","arguments":"{'path':'a.rs'"}),
            ]),
            "/arguments",
            json!(repaired.to_string()),
        ),
        (
            NativeContent::Gemini(vec![json!({"functionCall":{"id":"c","args":{}}})]),
            "/functionCall/args",
            repaired.clone(),
        ),
        (
            NativeContent::Bedrock(vec![json!({"toolUse":{"toolUseId":"c","input":{}}})]),
            "/toolUse/input",
            repaired.clone(),
        ),
    ] {
        let mut message = Message::assistant_with_tool_calls(
            "",
            vec![ToolCall::new(
                "c",
                "file_read",
                "{'path':'a.rs', limit: '5'}",
            )],
        );
        message.native = Some(native);
        message.set_tool_call_arguments(0, &repaired);
        assert_eq!(message.tool_calls[0].arguments, repaired.to_string());
        let (NativeContent::Anthropic(items)
        | NativeContent::Responses(items)
        | NativeContent::Gemini(items)
        | NativeContent::Bedrock(items)) = message.native.as_ref().unwrap();
        assert_eq!(items[0].pointer(pointer).unwrap(), &replayed);
    }
    // Without native content only the call changes.
    let mut message =
        Message::assistant_with_tool_calls("", vec![ToolCall::new("c", "file_read", "x")]);
    message.set_tool_call_arguments(0, &repaired);
    assert_eq!(message.tool_calls[0].arguments, repaired.to_string());
    message.set_tool_call_arguments(7, &json!({}));
}
