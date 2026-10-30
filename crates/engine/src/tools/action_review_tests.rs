//! Real engine continuation after an explicit observation; no effect is replayed.
use super::*;
use hermes_core::{FunctionCall, Message, ToolCall};
use xylo_core::{Conversation, EngineBackend, ProviderConfig, now_ms};
use xylo_services::{ActionResolutionOutcome, ActionResolutionRequest, ActionReviewScope};

struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    paths: crate::AppPaths,
    request: TurnRequest,
    id: Uuid,
    args: Value,
}
impl Fixture {
    fn new(name: &str, qualified: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let paths = crate::AppPaths::at(root.clone()).unwrap();
        let store = Store::open(&paths.database).unwrap();
        let session = Uuid::new_v4();
        store
            .upsert_conversation(&Conversation {
                id: session,
                title: "Explicit review".into(),
                workspace: root.clone(),
                updated_at: now_ms(),
            })
            .unwrap();
        let request = TurnRequest {
            session_id: session,
            turn_id: Uuid::new_v4(),
            prompt: "Continue from the observed outcome.".into(),
            workspace: root.clone(),
            provider: ProviderConfig::default(),
            permissions: PermissionMode::AskForWrites,
            attachments: vec![],
        };
        let id = Uuid::new_v4();
        let guard = if qualified {
            Some(store.action_execution_lease(id, true).unwrap())
        } else {
            None
        };
        let args = if name == "file_write" {
            json!({"path":"partial.txt","content":"must never be replayed"})
        } else {
            json!({"operation_id":Uuid::new_v4(),"environment_id":Uuid::new_v4(),"expected_revision":1,"command":["never-dispatch"]})
        };
        store
            .begin_action(&ActionIntent {
                id,
                session_id: session,
                turn_id: request.turn_id,
                name: name.into(),
                arguments: args.clone(),
                writes: true,
                idempotency_key: ToolBroker::journal_key(None, "reviewed-call"),
            })
            .unwrap();
        store.mark_action_running(id).unwrap();
        std::fs::write(root.join("partial.txt"), "observed partial effect").unwrap();
        store.mark_action_unknown_with_result(id,&json!({"status":"failed","success":false,"summary":"Completion was not confirmed","artifacts":[root.join("partial.txt")]}),"Interrupted before acknowledgement").unwrap();
        drop(guard);
        let checkpoint = vec![
            Message::user("Perform the approved action."),
            Message::assistant_with_tool_calls(
                None,
                vec![ToolCall {
                    id: "reviewed-call".into(),
                    function: FunctionCall {
                        name: name.into(),
                        arguments: args.to_string(),
                    },
                    extra_content: None,
                }],
            ),
        ];
        store
            .save_checkpoint(
                session,
                request.turn_id,
                &serde_json::to_value(checkpoint).unwrap(),
            )
            .unwrap();
        Self {
            _dir: dir,
            store,
            paths,
            request,
            id,
            args,
        }
    }
    fn scope(&self) -> ActionReviewScope {
        ActionReviewScope {
            session_id: self.request.session_id,
            workspace: self.request.workspace.clone(),
        }
    }
    fn resolution(&self, outcome: ActionResolutionOutcome) -> ActionResolutionRequest {
        ActionResolutionRequest {
            operation_id: Uuid::new_v4(),
            scope: self.scope(),
            action_id: self.id,
            expected_revision: self
                .store
                .action_review(self.scope(), self.id)
                .unwrap()
                .action
                .revision,
            outcome,
            evidence: "I inspected the saved file; partial effects are retained.".into(),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_observations_resume_real_engine_without_replay_or_false_native_success() {
    for (outcome, checkpointed) in [
        (ActionResolutionOutcome::Completed, false),
        (ActionResolutionOutcome::DidNotComplete, false),
        (ActionResolutionOutcome::Completed, true),
        (ActionResolutionOutcome::DidNotComplete, true),
    ] {
        let f = Fixture::new("file_write", true);
        let original = f.store.action(f.id).unwrap().unwrap();
        if checkpointed {
            let checkpoint = f.store.checkpoint(f.request.session_id).unwrap().unwrap();
            let mut messages: Vec<Message> = serde_json::from_value(checkpoint.messages).unwrap();
            // Imported metadata must neither shadow the actual assistant call
            // nor become a replacement tool observation merely by carrying IDs.
            let mut imported_note = Message::system("KEEP_IMPORTED_NOTE");
            imported_note.tool_call_id = Some("reviewed-call".into());
            imported_note.tool_calls = Some(vec![ToolCall {
                id: "reviewed-call".into(),
                function: FunctionCall {
                    name: "file_write".into(),
                    arguments: json!({"path":"wrong.txt","content":"wrong"}).to_string(),
                },
                extra_content: None,
            }]);
            messages.push(imported_note);
            messages.push(Message::tool_result(
                "reviewed-call",
                original.result.as_ref().unwrap().to_string(),
            ));
            messages.push(Message::system("KEEP_NON_REVIEW_NOTE"));
            messages.push(Message::assistant_with_tool_calls(
                None,
                vec![ToolCall {
                    id: "other-call".into(),
                    function: FunctionCall {
                        name: "file_read".into(),
                        arguments: json!({"path":"partial.txt"}).to_string(),
                    },
                    extra_content: None,
                }],
            ));
            messages.push(Message::tool_result(
                "other-call",
                json!({"status":"success","summary":"KEEP_OTHER_RESULT"}).to_string(),
            ));
            f.store
                .save_checkpoint(
                    f.request.session_id,
                    f.request.turn_id,
                    &serde_json::to_value(messages).unwrap(),
                )
                .unwrap();
        }
        let recorded = f.resolution(outcome);
        let receipt = f
            .store
            .resolve_action(recorded.clone(), &CancellationToken::new())
            .unwrap();
        let (endpoint, provider) = super::environment_recovery_tests::provider_fixture().await;
        let mut request = f.request.clone();
        request.turn_id = Uuid::new_v4();
        request.provider = ProviderConfig {
            endpoint,
            model: "fixture-model".into(),
            context_tokens: 65536,
            output_tokens: 256,
            timeout_seconds: 5,
            ..Default::default()
        };
        let engine =
            crate::HermesEngine::new(f.store.clone(), f.paths.clone(), ApprovalGate::default());
        let (events, mut incoming) = tokio::sync::mpsc::channel(256);
        tokio::time::timeout(
            Duration::from_secs(10),
            engine.run(request, events, CancellationToken::new()),
        )
        .await
        .unwrap()
        .unwrap();
        let body = provider.await.unwrap();
        if checkpointed {
            let text = body.to_string();
            assert!(text.contains("KEEP_NON_REVIEW_NOTE"));
            assert!(text.contains("KEEP_IMPORTED_NOTE"));
            assert!(text.contains("KEEP_OTHER_RESULT"));
        }
        assert_eq!(
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["role"] == "tool" && m["tool_call_id"] == "reviewed-call")
                .count(),
            1,
            "Refresh must replace the exact tool observation without duplication"
        );
        let sent = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "tool")
            .unwrap();
        let result: Value = serde_json::from_str(sent["content"].as_str().unwrap()).unwrap();
        assert_eq!(
            result["status"],
            if outcome == ActionResolutionOutcome::Completed {
                "user_confirmed"
            } else {
                "failed"
            }
        );
        assert_eq!(
            result["operator_observation"]["native_execution_verified"],
            false
        );
        assert_eq!(result["original_observation"], original.result.unwrap());
        assert_eq!(
            std::fs::read_to_string(f.request.workspace.join("partial.txt")).unwrap(),
            "observed partial effect"
        );
        while let Ok(event) = incoming.try_recv() {
            assert!(!matches!(
                event,
                EngineEvent::ToolStarted { .. } | EngineEvent::ApprovalRequired { .. }
            ));
        }
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let broker = ToolBroker::new(
            f.request.clone(),
            f.store.clone(),
            CapabilityConfig {
                workspace: f.request.workspace.clone(),
                artifacts: f.paths.artifacts.clone(),
                management_root: Some(f.paths.data.join("native-management")),
                ..Default::default()
            },
            ApprovalGate::default(),
            tx,
            CancellationToken::new(),
        )
        .unwrap();
        let args = f.args.clone();
        let cached = tokio::task::spawn_blocking(move || {
            broker.execute("reviewed-call", "file_write", args)
        })
        .await
        .unwrap();
        assert_eq!(
            cached.is_ok(),
            outcome == ActionResolutionOutcome::Completed
        );
        assert_eq!(
            f.store
                .action_resolution(f.scope(), recorded.operation_id)
                .unwrap(),
            Some(receipt)
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_native_receipt_remains_unqualified_and_actual_engine_blocks() {
    let f = Fixture::new("environment_execute", false);
    let resolution = f.resolution(ActionResolutionOutcome::Completed);
    assert!(
        f.store
            .resolve_action(resolution.clone(), &CancellationToken::new())
            .is_err()
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut request = f.request.clone();
    request.provider.model = "fixture-model".into();
    request.provider.endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let engine =
        crate::HermesEngine::new(f.store.clone(), f.paths.clone(), ApprovalGate::default());
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    assert!(
        engine
            .run(request, tx, CancellationToken::new())
            .await
            .unwrap_err()
            .to_string()
            .contains("outcome-unknown")
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
    assert!(
        f.store
            .action_resolution(f.scope(), resolution.operation_id)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        f.store.action(f.id).unwrap().unwrap().state,
        "outcome_unknown"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_blocking_work_retains_real_lease_after_async_scope_and_handle_are_cancelled() {
    let f = Fixture::new("file_write", true);
    let lease = f.store.lock_action_execution(f.id).unwrap();
    let file = lease.file();
    let (entered, ready) = std::sync::mpsc::sync_channel(1);
    let (release, wait) = std::sync::mpsc::sync_channel(1);
    let target = f.request.workspace.join("late.txt");
    // Deliberately return the handle without awaiting the actual blocking work:
    // it must retain ownership after this async lease scope is gone.
    let (job,) = xylo_runtime::resources::scope_action(file, async {
        (xylo_runtime::resources::spawn_blocking(move || {
            entered.send(()).unwrap();
            wait.recv().unwrap();
            std::fs::write(target, "actual late work completed").unwrap();
        }),)
    })
    .await;
    ready.recv_timeout(Duration::from_secs(2)).unwrap();
    drop(lease);
    job.abort();
    let request = f.resolution(ActionResolutionOutcome::DidNotComplete);
    assert!(f.store.action_review(f.scope(), f.id).unwrap().action.busy);
    assert!(
        f.store
            .resolve_action(request.clone(), &CancellationToken::new())
            .is_err()
    );
    release.send(()).unwrap();
    job.await.unwrap();
    assert!(!f.store.action_review(f.scope(), f.id).unwrap().action.busy);
    f.store
        .resolve_action(request, &CancellationToken::new())
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(f.request.workspace.join("late.txt")).unwrap(),
        "actual late work completed"
    );
}
