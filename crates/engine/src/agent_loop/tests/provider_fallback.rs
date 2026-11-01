//! Availability recovery must preserve tool history and never replay observed calls.
use super::*;
use crate::providers::fallback::WithFallback;

fn unavailable() -> ModelError {
    ModelError::Unavailable {
        status: 529,
        retry_after: Some(Duration::ZERO),
        message: "Provider returned HTTP 529".into(),
    }
}

#[tokio::test]
async fn scripted_529_continues_on_fallback_with_a_durable_harness_note() {
    let mut harness = harness(vec![vec![Step::Fail(unavailable())]; 6]);
    let alternative = Scripted::new(vec![
        vec![
            call(0, "read", "read_a", "{}"),
            finish(FinishReason::ToolCalls),
        ],
        vec![text("Recovered"), finish(FinishReason::Stop)],
    ]);
    harness.agent.provider = Arc::new(WithFallback {
        primary: harness.provider.clone(),
        alternative: alternative.clone(),
        name: "alternative-model".into(),
    });
    let outcome = harness
        .agent
        .run(vec![Message::user("Work")])
        .await
        .unwrap();
    assert_eq!(outcome.text, "Recovered");
    assert_eq!(harness.provider.requests.lock().len(), 6);
    let requests = alternative.requests.lock();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0]
            .messages
            .iter()
            .any(|message| message.content.contains("alternative-model"))
    );
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.role == Role::Tool)
    );
    assert!(
        outcome
            .messages
            .iter()
            .any(|message| message.content.contains("configured fallback"))
    );
    assert!(drain(&mut harness.events).iter().any(|event| matches!(
        event,
        EngineEvent::HarnessNote {
            kind: NoteKind::Fallback,
            ..
        }
    )));
}

#[tokio::test]
async fn an_observed_tool_call_never_switches_models_or_replays() {
    let mut harness = harness(vec![vec![
        call(0, "read", "read_a", "{}"),
        Step::Fail(unavailable()),
    ]]);
    let alternative = Scripted::new(vec![]);
    harness.agent.provider = Arc::new(WithFallback {
        primary: harness.provider.clone(),
        alternative: alternative.clone(),
        name: "alternative-model".into(),
    });
    assert!(
        harness
            .agent
            .run(vec![Message::user("Work")])
            .await
            .is_err()
    );
    assert_eq!(harness.provider.requests.lock().len(), 1);
    assert!(alternative.requests.lock().is_empty());
}

#[tokio::test]
async fn thinking_is_streamed_as_its_own_event_and_retained_in_history() {
    let mut harness = harness(vec![vec![
        Step::Event(StreamEvent::Reasoning("Consider".into())),
        text("Answer"),
        finish(FinishReason::Stop),
    ]]);
    let outcome = harness
        .agent
        .run(vec![Message::user("Work")])
        .await
        .unwrap();
    assert_eq!(
        outcome.messages.last().unwrap().reasoning.as_deref(),
        Some("Consider")
    );
    assert!(
        drain(&mut harness.events).iter().any(
            |event| matches!(event, EngineEvent::ThinkingDelta { text } if text == "Consider")
        )
    );
}

#[tokio::test]
async fn a_checkpoint_that_already_selected_fallback_resumes_there_directly() {
    let mut harness = harness(vec![]);
    let alternative = Scripted::new(vec![vec![text("Resumed"), finish(FinishReason::Stop)]]);
    harness.agent.provider = Arc::new(WithFallback {
        primary: harness.provider.clone(),
        alternative: alternative.clone(),
        name: "alternative-model".into(),
    });
    let history = vec![
        Message::user("Work"),
        application_note(&retry::fallback_note("alternative-model")),
    ];
    let outcome = harness.agent.run(history).await.unwrap();
    assert_eq!(outcome.text, "Resumed");
    assert!(harness.provider.requests.lock().is_empty());
    assert_eq!(alternative.requests.lock().len(), 1);
}

#[tokio::test]
async fn authentication_failure_never_selects_a_fallback() {
    let mut harness = harness(vec![vec![Step::Fail(ModelError::Authentication(
        "Denied".into(),
    ))]]);
    let alternative = Scripted::new(vec![]);
    harness.agent.provider = Arc::new(WithFallback {
        primary: harness.provider.clone(),
        alternative: alternative.clone(),
        name: "alternative-model".into(),
    });
    assert!(
        harness
            .agent
            .run(vec![Message::user("Work")])
            .await
            .is_err()
    );
    assert!(alternative.requests.lock().is_empty());
    assert!(!drain(&mut harness.events).iter().any(|event| matches!(
        event,
        EngineEvent::HarnessNote {
            kind: NoteKind::Fallback,
            ..
        }
    )));
}
