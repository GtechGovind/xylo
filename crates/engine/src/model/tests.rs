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
