//! Plan approval and question answers continue the original model turn.
use super::*;
use crate::{FinishReason, RunOptions, StreamEvent};
use std::sync::atomic::{AtomicUsize, Ordering};
use xylo_core::{InteractionAnswer, QuestionAnswer};

struct InteractionProbe {
    calls: AtomicUsize,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}
impl ModelProvider for InteractionProbe {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> crate::EventStream {
        if request.reasoning == crate::Reasoning::Lowest {
            return Box::pin(futures_util::stream::iter([
                Ok(StreamEvent::Text("Planning".into())),
                Ok(StreamEvent::Finished(FinishReason::Stop)),
            ]));
        }
        self.requests.lock().push(request);
        let step = self.calls.fetch_add(1, Ordering::SeqCst);
        let call = match step {
            0 => Some((
                "exit_plan_mode",
                json!({"plan":"Ask the scope, then finish."}),
            )),
            1 => Some((
                "ask_user",
                json!({"questions":[{"header":"Scope","question":"Choose scope","multi_select":false,"options":[{"label":"Small","description":"One file"},{"label":"Broad","description":"All files"}]}]}),
            )),
            _ => None,
        };
        match call {
            Some((name, args)) => Box::pin(futures_util::stream::iter([
                Ok(StreamEvent::ToolCall {
                    index: 0,
                    id: Some(format!("interaction-{step}")),
                    name: Some(name.into()),
                    arguments: Some(args.to_string()),
                }),
                Ok(StreamEvent::Finished(FinishReason::ToolCalls)),
            ])),
            None => Box::pin(futures_util::stream::iter([
                Ok(StreamEvent::Text("Selected Small.".into())),
                Ok(StreamEvent::Finished(FinishReason::Stop)),
            ])),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interaction_plan_and_question_continue_the_same_turn_to_the_model() {
    let (_root, engine, mut request) = fixture();
    request.permissions = PermissionMode::Plan;
    request.provider = ProviderConfig {
        model: "fixture".into(),
        endpoint: "https://scripted.invalid/v1".into(),
        context_tokens: 32768,
        ..Default::default()
    };
    engine.session.permissions().set(PermissionMode::Plan);
    let tools = engine.session.clone();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let probe: Arc<dyn ModelProvider> = Arc::new(InteractionProbe {
        calls: AtomicUsize::new(0),
        requests: requests.clone(),
    });
    let engine = engine
        .with_provider_credential(WorkerCredential(Some("offline".into())))
        .with_provider_layer(Arc::new(move |_| probe.clone()))
        .with_run_options(RunOptions {
            interactive: true,
            ..Default::default()
        });
    engine.store.register_user_turn(&request).unwrap();
    let (events, mut receiver) = mpsc::channel(256);
    let run =
        tokio::spawn(async move { engine.run(request, events, CancellationToken::new()).await });
    let mut completed = false;
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = receiver.recv().await {
            match event {
                EngineEvent::PlanRequired { action_id, .. } => tools
                    .answer(
                        action_id,
                        InteractionAnswer::ApprovePlan {
                            mode: PermissionMode::AskForApproval,
                        },
                    )
                    .unwrap(),
                EngineEvent::QuestionRequired { action_id, .. } => tools
                    .answer(
                        action_id,
                        InteractionAnswer::Questions {
                            answers: vec![QuestionAnswer {
                                selected: vec![0],
                                other: None,
                            }],
                        },
                    )
                    .unwrap(),
                EngineEvent::Completed { .. } => completed = true,
                EngineEvent::Error { message, .. } => panic!("{message}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    run.await.unwrap().unwrap();
    assert!(completed);
    let seen = requests.lock();
    assert_eq!(seen.len(), 3);
    assert!(
        seen[1]
            .messages
            .iter()
            .any(|message| message.content.contains("approve_plan"))
    );
    assert!(
        seen[2]
            .messages
            .iter()
            .any(|message| message.content.contains("resolved_answers")
                && message.content.contains("Small"))
    );
}
