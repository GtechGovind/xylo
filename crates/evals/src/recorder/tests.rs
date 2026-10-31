use super::*;
use crate::{
    scripted::{Script, ScriptedProvider, Step},
    trace::ErrorKind,
};
use serde_json::json;
use xylo_engine::{Message, Reasoning, ToolSchema};

fn request() -> ModelRequest {
    ModelRequest {
        messages: vec![Message::system("system"), Message::user("go")],
        tools: vec![ToolSchema {
            name: "file_write".into(),
            description: "Write".into(),
            parameters: json!({"type": "object"}),
        }],
        max_tokens: 128,
        reasoning: xylo_engine::Reasoning::Configured,
    }
}

fn recorder(script: Script) -> (RecordingProvider, ExchangeLog) {
    let log = ExchangeLog::default();
    let provider = RecordingProvider::new(Arc::new(ScriptedProvider::new(script)), log.clone());
    (provider, log)
}

#[tokio::test]
async fn a_cut_off_call_is_recorded_exactly_as_sent() {
    let cut = ModelReply {
        tool_calls: vec![ToolCall::new(
            "c1",
            "file_write",
            r#"{"path":"a","content":"par"#,
        )],
        finish: FinishReason::Length,
        ..ModelReply::default()
    };
    let (provider, log) = recorder(Script::replies([cut.clone()]));
    let events: Vec<_> = provider
        .stream(request(), CancellationToken::new())
        .collect()
        .await;
    assert!(events.iter().all(Result::is_ok));
    let log = log.lock();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].kind, RequestKind::Agent);
    assert_eq!(log[0].request.tools, ["file_write"]);
    let Outcome::Reply(reply) = &log[0].outcome else {
        panic!("expected a reply, got {:?}", log[0].outcome);
    };
    assert_eq!(reply.tool_calls, cut.tool_calls);
    assert_eq!(reply.finish, FinishReason::Length);
    assert!(
        reply.usage.as_ref().unwrap()["input_tokens"]
            .as_u64()
            .unwrap()
            > 0
    );
}

#[tokio::test]
async fn failures_are_recorded_and_still_reach_the_caller() {
    let (provider, log) = recorder(Script {
        steps: vec![Step::Fail(RecordedError::context_overflow("too long"))],
        ..Script::default()
    });
    let error = provider
        .complete(request(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(error, ModelError::ContextOverflow(_)));
    let log = log.lock();
    assert!(
        matches!(&log[0].outcome, Outcome::Error(error) if error.kind == ErrorKind::ContextOverflow)
    );
}

#[tokio::test]
async fn a_stream_dropped_midway_is_recorded_as_abandoned() {
    let (provider, log) = recorder(Script::replies([ModelReply::text("hello")]));
    let mut stream = provider.stream(request(), CancellationToken::new());
    assert!(stream.next().await.is_some());
    drop(stream);
    assert!(matches!(log.lock()[0].outcome, Outcome::Abandoned));
}

#[tokio::test]
async fn titles_are_told_apart_from_agent_steps() {
    let (provider, log) = recorder(Script::default());
    let mut title = request();
    title.tools.clear();
    title.reasoning = Reasoning::Lowest;
    provider
        .complete(title, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(log.lock()[0].kind, RequestKind::Title);
}
