use super::*;
use crate::trace::ErrorKind;
use futures_util::StreamExt;
use serde_json::json;
use xylo_engine::{FinishReason, Message, ModelResponse, Reasoning, ToolSchema};

fn agent(text: &str) -> ModelRequest {
    ModelRequest {
        messages: vec![Message::system("system"), Message::user(text)],
        tools: vec![ToolSchema {
            name: "file_read".into(),
            description: "Read a file".into(),
            parameters: json!({"type": "object"}),
        }],
        max_tokens: 256,
        reasoning: Reasoning::Configured,
    }
}

fn auxiliary(reasoning: Reasoning) -> ModelRequest {
    ModelRequest {
        tools: Vec::new(),
        reasoning,
        ..agent("summarize")
    }
}

async fn ask(
    provider: &ScriptedProvider,
    request: ModelRequest,
) -> Result<ModelResponse, ModelError> {
    provider.complete(request, CancellationToken::new()).await
}

#[tokio::test]
async fn agent_steps_play_in_order_then_fail_loudly() {
    let provider = ScriptedProvider::new(Script::replies([
        ModelReply::calls(1, [("file_read", json!({"path": "a"}))]),
        ModelReply::text("done"),
    ]));
    let first = ask(&provider, agent("go")).await.unwrap();
    assert_eq!(first.finish, FinishReason::ToolCalls);
    assert_eq!(first.message.tool_calls[0].id, "call-1-0");
    assert_eq!(
        ask(&provider, agent("go")).await.unwrap().message.content,
        "done"
    );
    let error = ask(&provider, agent("go")).await.unwrap_err();
    assert!(!error.retryable());
    assert!(error.to_string().contains("no step 3"), "{error}");
}

#[tokio::test]
async fn summaries_and_titles_do_not_consume_agent_steps() {
    let provider = ScriptedProvider::new(Script {
        steps: vec![Step::Reply(ModelReply::text("answer"))],
        summaries: vec!["first summary".into()],
        ..Script::default()
    });
    let summary = ask(&provider, auxiliary(Reasoning::Configured))
        .await
        .unwrap();
    assert_eq!(summary.message.content, "first summary");
    let again = ask(&provider, auxiliary(Reasoning::Configured))
        .await
        .unwrap();
    assert_eq!(again.message.content, DEFAULT_SUMMARY);
    let title = ask(&provider, auxiliary(Reasoning::Lowest)).await.unwrap();
    assert_eq!(title.message.content, DEFAULT_TITLE);
    assert_eq!(
        ask(&provider, agent("go")).await.unwrap().message.content,
        "answer"
    );
}

#[tokio::test]
async fn requests_over_the_served_window_overflow_without_consuming_a_step() {
    let small = agent("short");
    let window = estimate_tokens(&small) + 10;
    let provider = ScriptedProvider::new(Script {
        steps: vec![Step::Reply(ModelReply::text("fits"))],
        served_window: Some(window),
        ..Script::default()
    });
    let large = agent(&"x".repeat(200));
    assert!(matches!(
        ask(&provider, large).await,
        Err(ModelError::ContextOverflow(_))
    ));
    assert_eq!(ask(&provider, small).await.unwrap().message.content, "fits");
}

#[tokio::test]
async fn scripted_failures_and_truncation_reach_the_engine_types() {
    let provider = ScriptedProvider::new(Script {
        steps: vec![
            Step::Fail(RecordedError {
                kind: ErrorKind::Transient,
                message: "overloaded".into(),
            }),
            Step::Reply(ModelReply::text("partial").truncated()),
        ],
        ..Script::default()
    });
    let error = ask(&provider, agent("go")).await.unwrap_err();
    assert!(error.retryable());
    assert_eq!(
        ask(&provider, agent("go")).await.unwrap().finish,
        FinishReason::Length
    );
}

#[tokio::test]
async fn cancelled_requests_are_interrupted() {
    let provider = ScriptedProvider::new(Script::replies([ModelReply::text("never")]));
    let cancel = CancellationToken::new();
    cancel.cancel();
    let mut stream = provider.stream(agent("go"), cancel);
    assert_eq!(stream.next().await, Some(Err(ModelError::Interrupted)));
}

#[tokio::test]
async fn usage_is_deterministic_unless_the_reply_records_its_own() {
    let provider = ScriptedProvider::new(Script::replies([
        ModelReply::text("same"),
        ModelReply::text("same"),
        ModelReply {
            usage: Some(json!({"input_tokens": 7, "output_tokens": 3, "unknown_field": 1})),
            ..ModelReply::text("recorded")
        },
    ]));
    let first = ask(&provider, agent("go")).await.unwrap().usage.unwrap();
    let second = ask(&provider, agent("go")).await.unwrap().usage.unwrap();
    assert_eq!(first, second);
    assert_eq!(first.input_tokens, estimate_tokens(&agent("go")));
    let recorded = ask(&provider, agent("go")).await.unwrap().usage.unwrap();
    assert_eq!((recorded.input_tokens, recorded.output_tokens), (7, 3));
}

fn child(task: &str) -> ModelRequest {
    let mut request = agent(task);
    request.messages[0] = Message::system(format!(
        "{} More helper instructions.",
        crate::trace::child_opening()
    ));
    request
}

#[tokio::test]
async fn children_are_routed_by_task_whatever_order_they_ask_in() {
    let provider = ScriptedProvider::new(Script {
        children: vec![
            ChildScript {
                task: "Read a".into(),
                steps: vec![
                    Step::Reply(ModelReply::text("a1")),
                    Step::Reply(ModelReply::text("a2")),
                ],
            },
            ChildScript {
                task: "Read b".into(),
                steps: vec![Step::Reply(ModelReply::text("b1"))],
            },
            ChildScript {
                task: "Read b, then summarize".into(),
                steps: vec![Step::Reply(ModelReply::text("exact"))],
            },
        ],
        ..Script::default()
    });
    let text = |response: ModelResponse| response.message.content;
    assert_eq!(text(ask(&provider, child("Read b")).await.unwrap()), "b1");
    assert_eq!(text(ask(&provider, child("Read a")).await.unwrap()), "a1");
    assert_eq!(text(ask(&provider, child("Read a")).await.unwrap()), "a2");
    assert_eq!(
        text(
            ask(&provider, child("Read b, then summarize"))
                .await
                .unwrap()
        ),
        "exact"
    );
    assert!(ask(&provider, child("Read c")).await.is_err());
    // Root steps are untouched by children.
    assert!(
        ask(&provider, agent("go"))
            .await
            .unwrap_err()
            .to_string()
            .contains("no step 1")
    );
}
