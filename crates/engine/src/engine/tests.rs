use super::*;
use hermes_core::{FunctionCall, ToolCall};
use uuid::Uuid;
use xylo_core::{Conversation, PermissionMode, now_ms};
use xylo_services::ActionIntent;

fn fixture() -> (tempfile::TempDir, HermesEngine, TurnRequest) {
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::at(directory.path().to_owned()).unwrap();
    let store = Store::open(&paths.database).unwrap();
    let session = Uuid::new_v4();
    store
        .upsert_conversation(&Conversation {
            id: session,
            title: "Resume fixture".into(),
            workspace: directory.path().into(),
            updated_at: now_ms(),
        })
        .unwrap();
    let request = TurnRequest {
        session_id: session,
        turn_id: Uuid::new_v4(),
        prompt: "Continue".into(),
        workspace: directory.path().into(),
        provider: ProviderConfig::default(),
        permissions: PermissionMode::ReadOnly,
        attachments: Vec::new(),
    };
    let engine = HermesEngine::new(store, paths, ApprovalGate::default());
    (directory, engine, request)
}
fn assistant_call(name: &str, args: Value) -> Message {
    Message::assistant_with_tool_calls(
        None,
        vec![ToolCall {
            id: "provider-call-1".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: args.to_string(),
            },
            extra_content: None,
        }],
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn image_checkpoint_resume_preserves_refs_without_duplicate_or_inline_image_data() {
    use xylo_core::AttachmentSource;
    let (_directory, engine, mut request) = fixture();
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(16, 16)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    let assets =
        xylo_services::attachments::AttachmentStore::new(engine.paths.artifacts.clone()).unwrap();
    request.attachments = vec![
        assets
            .import_bytes(
                request.session_id,
                "resume.png",
                png.get_ref(),
                AttachmentSource::Picker,
                &CancellationToken::new(),
            )
            .unwrap(),
    ];
    engine.store.register_user_turn(&request).unwrap();
    let checkpoint = vec![crate::attachments::user_message(&request).unwrap()];
    engine
        .store
        .save_checkpoint(
            request.session_id,
            request.turn_id,
            &serde_json::to_value(&checkpoint).unwrap(),
        )
        .unwrap();
    let (events, _receiver) = mpsc::channel(16);
    let restored = engine
        .restored_messages(&request, events, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(restored.len(), 1);
    let value = serde_json::to_value(&restored).unwrap();
    assert_eq!(
        value[0]["anthropic_content_blocks"][0]["item"]["id"],
        request.attachments[0].id.to_string()
    );
    assert!(!value.to_string().contains("base64"));
    assert!(serde_json::to_vec(&value).unwrap().len() < 2048);
    assert_eq!(
        engine.store.messages(request.session_id, 10).unwrap()[0].attachments,
        request.attachments
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_restores_success_using_original_turn_before_new_prompt() {
    let (_directory, engine, mut request) = fixture();
    let old_turn = request.turn_id;
    let args = json!({"path":"already-written.txt","content":"original"});
    let intent = ActionIntent {
        id: Uuid::new_v4(),
        session_id: request.session_id,
        turn_id: old_turn,
        name: "file_write".into(),
        arguments: args.clone(),
        writes: true,
        idempotency_key: ToolBroker::journal_key(None, "provider-call-1"),
    };
    engine.store.begin_action(&intent).unwrap();
    engine.store.mark_action_running(intent.id).unwrap();
    engine
        .store
        .finish_action(
            intent.id,
            &json!({"status":"success","summary":{"remote_id":"once-only"}}),
        )
        .unwrap();
    engine
        .store
        .save_checkpoint(
            request.session_id,
            old_turn,
            &serde_json::to_value(vec![assistant_call("file_write", args)]).unwrap(),
        )
        .unwrap();
    request.turn_id = Uuid::new_v4();
    let (events, _rx) = mpsc::channel(256);
    let restored = engine
        .restored_messages(&request, events, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(restored.len(), 3);
    assert_eq!(restored[2].content.as_deref(), Some("Continue"));
    let result: Value = serde_json::from_str(restored[1].content.as_deref().unwrap()).unwrap();
    assert_eq!(result["summary"]["remote_id"], "once-only");
    assert_eq!(
        engine
            .store
            .checkpoint(request.session_id)
            .unwrap()
            .unwrap()
            .turn_id,
        old_turn
    );
    assert!(
        engine
            .store
            .action_for_key(
                request.session_id,
                request.turn_id,
                &ToolBroker::journal_key(None, "provider-call-1")
            )
            .unwrap()
            .is_none()
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn denied_pending_result_is_restored_as_error_without_redispatch() {
    let (_directory, engine, request) = fixture();
    let args = json!({"path":"denied.txt","content":"never"});
    let intent = ActionIntent {
        id: Uuid::new_v4(),
        session_id: request.session_id,
        turn_id: request.turn_id,
        name: "file_write".into(),
        arguments: args.clone(),
        writes: true,
        idempotency_key: ToolBroker::journal_key(None, "provider-call-1"),
    };
    engine.store.begin_action(&intent).unwrap();
    engine.store.deny_action(intent.id).unwrap();
    engine
        .store
        .save_checkpoint(
            request.session_id,
            request.turn_id,
            &serde_json::to_value(vec![assistant_call("file_write", args)]).unwrap(),
        )
        .unwrap();
    let (events, _rx) = mpsc::channel(256);
    let restored = engine
        .restored_messages(&request, events, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(restored.len(), 2);
    assert_eq!(
        serde_json::from_str::<Value>(restored[1].content.as_deref().unwrap()).unwrap()["status"],
        "error"
    );
    assert!(!request.workspace.join("denied.txt").exists());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_registry_cannot_expose_delegation() {
    let (_directory, engine, request) = fixture();
    let (events, _rx) = mpsc::channel(256);
    let broker = Arc::new(
        ToolBroker::new(
            request.clone(),
            engine.store.clone(),
            engine.capability_config(&request).unwrap(),
            ApprovalGate::default(),
            events,
            CancellationToken::new(),
        )
        .unwrap(),
    );
    let registry = build_registry(broker, Some("parent-delegate-call".into())).unwrap();
    assert!(registry.get("agent_delegate").is_none());
    assert!(registry.get("call_tool").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_completed_event_replays_receipt_without_model_or_duplicate_history() {
    let (_directory, engine, request) = fixture();
    let usage = Usage {
        input_tokens: 7,
        output_tokens: 11,
        elapsed_ms: 13,
    };
    engine
        .store
        .complete_turn(
            request.session_id,
            request.turn_id,
            &json!([]),
            "already completed",
            &usage,
        )
        .unwrap();
    // Default provider has no chosen model and would fail before a model request.
    // Receipt delivery must precede provider validation and every other run action.
    let (events, mut received) = mpsc::channel(32);
    engine
        .run(request.clone(), events, CancellationToken::new())
        .await
        .unwrap();
    match received.recv().await.unwrap() {
        EngineEvent::Completed { text, usage } => {
            assert_eq!(text, "already completed");
            assert_eq!(usage.output_tokens, 11)
        }
        event => panic!("Unexpected event: {event:?}"),
    };
    assert_eq!(
        engine.store.messages(request.session_id, 10).unwrap().len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_turn_retry_resumes_without_reinjecting_the_original_user_instruction() {
    let (_directory, engine, mut request) = fixture();
    engine
        .store
        .save_checkpoint(
            request.session_id,
            request.turn_id,
            &serde_json::to_value(vec![
                Message::user("Continue"),
                Message::assistant("Intermediate saved reasoning"),
            ])
            .unwrap(),
        )
        .unwrap();
    let (events, _rx) = mpsc::channel(32);
    let resumed = engine
        .restored_messages(&request, events, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(resumed.len(), 2);
    assert_eq!(
        resumed
            .iter()
            .filter(|m| m.role == MessageRole::User)
            .count(),
        1
    );
    request.turn_id = Uuid::new_v4();
    let (events, _rx) = mpsc::channel(32);
    let new_turn = engine
        .restored_messages(&request, events, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(new_turn.len(), 3);
    assert_eq!(
        new_turn
            .iter()
            .filter(|m| m.role == MessageRole::User)
            .count(),
        2
    );
}

struct NaturalAnswerProvider {
    calls: std::sync::atomic::AtomicUsize,
    seen: std::sync::Mutex<Vec<String>>,
}
impl NaturalAnswerProvider {
    fn next(&self, messages: &[Message]) -> usize {
        self.seen
            .lock()
            .unwrap()
            .push(serde_json::to_string(messages).unwrap());
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    }
}
#[async_trait]
impl LlmProvider for NaturalAnswerProvider {
    async fn chat_completion(
        &self,
        messages: &[Message],
        _: &[ToolSchema],
        _: Option<u32>,
        _: Option<f64>,
        _: Option<&str>,
        _: Option<&Value>,
    ) -> Result<hermes_core::LlmResponse, AgentError> {
        let message = match self.next(messages) {
            0 => assistant_call("file_read", json!({"path":"fixture.txt"})),
            1 => Message::assistant("The file contains HELLO_NATIVE_TEST."),
            _ => {
                return Err(AgentError::Config(
                    "Unexpected third provider call: formatting retry escaped product mode".into(),
                ));
            }
        };
        Ok(hermes_core::LlmResponse {
            message,
            usage: None,
            model: "product-fixture".into(),
            finish_reason: Some("stop".into()),
        })
    }
    fn chat_completion_stream(
        &self,
        messages: &[Message],
        _: &[ToolSchema],
        _: Option<u32>,
        _: Option<f64>,
        _: Option<&str>,
        _: Option<&Value>,
    ) -> futures_util::stream::BoxStream<
        'static,
        std::result::Result<hermes_core::StreamChunk, AgentError>,
    > {
        use hermes_core::{FunctionCallDelta, StreamChunk, StreamDelta, ToolCallDelta};
        let chunk = match self.next(messages) {
            0 => Ok(StreamChunk {
                delta: Some(StreamDelta {
                    content: None,
                    tool_calls: Some(vec![ToolCallDelta {
                        index: 0,
                        id: Some("provider-call-1".into()),
                        function: Some(FunctionCallDelta {
                            name: Some("file_read".into()),
                            arguments: Some(json!({"path":"fixture.txt"}).to_string()),
                        }),
                    }]),
                    extra: None,
                }),
                finish_reason: Some("tool_calls".into()),
                usage: None,
            }),
            1 => Ok(StreamChunk {
                delta: Some(StreamDelta {
                    content: Some("The file contains HELLO_NATIVE_TEST.".into()),
                    tool_calls: None,
                    extra: None,
                }),
                finish_reason: Some("stop".into()),
                usage: None,
            }),
            _ => Err(AgentError::Config(
                "Unexpected third provider call: formatting retry escaped product mode".into(),
            )),
        };
        Box::pin(futures_util::stream::once(async move { chunk }))
    }
}
async fn verified_tool_and_natural_answer(streaming: bool) {
    let (directory, engine, request) = fixture();
    std::fs::write(directory.path().join("fixture.txt"), "HELLO_NATIVE_TEST").unwrap();
    let (events, _rx) = mpsc::channel(256);
    let broker = Arc::new(
        ToolBroker::new(
            request.clone(),
            engine.store.clone(),
            engine.capability_config(&request).unwrap(),
            ApprovalGate::default(),
            events,
            CancellationToken::new(),
        )
        .unwrap(),
    );
    let registry = build_registry(broker, None).unwrap();
    let schemas = vec![registry.get("file_read").unwrap().schema.clone()];
    let recorded = Arc::new(NaturalAnswerProvider {
        calls: std::sync::atomic::AtomicUsize::new(0),
        seen: std::sync::Mutex::new(Vec::new()),
    });
    let provider_config = ProviderConfig {
        model: "product-fixture".into(),
        context_tokens: 8192,
        output_tokens: 256,
        ..Default::default()
    };
    let provider = Arc::new(crate::provider_guard::GuardedProvider::new(
        recorded.clone(),
        &provider_config,
        directory.path().join("local-inference.lock"),
        CancellationToken::new(),
        false,
    ));
    let mut config = AgentConfig {
        native_product_finalization: true,
        model: "product-fixture".into(),
        max_turns: 4,
        max_tokens: Some(256),
        system_prompt: Some(
            "Use tools to verify file contents; answer naturally and concisely.".into(),
        ),
        skip_memory: true,
        skip_context_files: true,
        code_index_enabled: false,
        lsp_context_enabled: false,
        hermes_home: Some(directory.path().to_string_lossy().into_owned()),
        ..Default::default()
    };
    config.retry.max_retries = 0;
    let agent = AgentLoop::new(config, registry, provider);
    let messages = vec![Message::user("Read fixture.txt and tell me its contents.")];
    let result = if streaming {
        agent
            .run_stream(messages, Some(schemas), Some(Box::new(|_| {})))
            .await
    } else {
        agent.run(messages, Some(schemas)).await
    }
    .unwrap();
    assert!(result.finished_naturally);
    assert_eq!(recorded.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(
        result.messages.last().unwrap().content.as_deref(),
        Some("The file contains HELLO_NATIVE_TEST.")
    );
    for request in recorded.seen.lock().unwrap().iter() {
        assert!(!request.contains("Finalizer evidence contract"));
        assert!(!request.contains("confidence=<"));
        assert!(!request.contains("objective_state="));
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_streaming_finalization_finishes_after_one_tool_and_natural_answer() {
    verified_tool_and_natural_answer(true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_nonstream_finalization_finishes_after_one_tool_and_natural_answer() {
    verified_tool_and_natural_answer(false).await;
}
#[test]
fn product_mode_is_opt_in_and_statuses_do_not_expose_engine_contracts() {
    assert!(!AgentConfig::default().native_product_finalization);
    assert_eq!(
        product_status("Retrying model request after HTTP 429"),
        "Retrying the model request…"
    );
    assert_eq!(
        product_status("Rate limit reached; waiting"),
        "Waiting for the provider's rate limit…"
    );
    assert_eq!(
        product_status("Context compaction removed 14000 tokens"),
        "Condensing earlier context…"
    );
    assert_eq!(
        product_status("Context pressure at 85%"),
        "Preparing conversation…"
    );
    assert_eq!(product_status("Calling internal_tool_name"), "Working…");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_error_after_admission_queues_terminal_learning_evidence_without_model_dispatch() {
    let (_directory, engine, mut request) = fixture();
    request.provider.model = "fixture-model".into();
    engine.store.register_user_turn(&request).unwrap();
    let action = ActionIntent {
        id: Uuid::new_v4(),
        session_id: request.session_id,
        turn_id: request.turn_id,
        name: "run_command".into(),
        arguments: json!({"program":"cargo","args":["test","--offline"]}),
        writes: true,
        idempotency_key: "interrupted-verifier".into(),
    };
    engine.store.begin_action(&action).unwrap();
    engine.store.mark_action_running(action.id).unwrap();
    engine
        .store
        .mark_action_unknown(action.id, "Worker interrupted")
        .unwrap();
    let (events, _receiver) = mpsc::channel(256);
    assert!(
        engine
            .run(request.clone(), events, CancellationToken::new())
            .await
            .is_err()
    );
    let learning = LearningService::new(engine.store.clone());
    assert_eq!(
        learning
            .status(&request.workspace.to_string_lossy())
            .unwrap()
            .pending_jobs,
        1
    );
    let batch = learning
        .process_pending(4, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(batch.processed, 1);
    assert_eq!(batch.promoted, 0);
}
#[tokio::test]
async fn dropped_engine_future_guard_persists_the_terminal_evidence_hook() {
    let (_directory, engine, request) = fixture();
    let learning = LearningService::new(engine.store.clone());
    let service = learning.clone();
    let session = request.session_id;
    let turn = request.turn_id;
    let (ready, started) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let _guard = LearningTurnGuard {
            service,
            session,
            turn,
            completed: false,
        };
        ready.send(()).unwrap();
        futures_util::future::pending::<()>().await;
    });
    started.await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(
        learning
            .status(&request.workspace.to_string_lossy())
            .unwrap()
            .pending_jobs,
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_rebuilds_only_leading_policy_and_retains_later_system_and_native_messages() {
    let (_directory, engine, request) = fixture();
    let messages = vec![
        Message::system("Old application policy"),
        Message::system("Old generated hint"),
        Message::user("Original task"),
        Message::system("Preserve this later safety note"),
        Message::assistant("Partial answer"),
    ];
    engine
        .store
        .save_checkpoint(
            request.session_id,
            request.turn_id,
            &serde_json::to_value(&messages).unwrap(),
        )
        .unwrap();
    let (events, _receiver) = mpsc::channel(16);
    let restored = engine
        .restored_messages(&request, events, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(restored).unwrap(),
        serde_json::to_value(&messages[2..]).unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_generation_requires_durable_context_and_retains_unknown_writes() {
    let (_directory, engine, request) = fixture();
    let context = json!([{"role":"user","content":"Retained original task"}]);
    engine
        .store
        .save_checkpoint(request.session_id, request.turn_id, &context)
        .unwrap();
    let action = ActionIntent {
        id: Uuid::new_v4(),
        session_id: request.session_id,
        turn_id: request.turn_id,
        name: "external_write".into(),
        arguments: json!({"value":"original"}),
        writes: true,
        idempotency_key: "already-dispatched".into(),
    };
    engine.store.begin_action(&action).unwrap();
    engine.store.mark_action_running(action.id).unwrap();
    engine
        .store
        .mark_action_unknown(action.id, "Transport ended after dispatch")
        .unwrap();
    let (events, mut received) = mpsc::channel(8);
    engine
        .finish_interrupted_generation(&request, &events, Ok(()), Ok(()))
        .await
        .unwrap();
    assert!(matches!(
        received.recv().await,
        Some(EngineEvent::Cancelled)
    ));
    assert!(received.try_recv().is_err());
    assert_eq!(
        engine
            .store
            .checkpoint(request.session_id)
            .unwrap()
            .unwrap()
            .messages,
        context
    );
    assert_eq!(
        engine.store.unresolved_actions(request.session_id).unwrap()[0].state,
        "outcome_unknown"
    );
    assert!(
        engine
            .store
            .completed_turn(request.session_id, request.turn_id)
            .unwrap()
            .is_none()
    );
    // A cleanup fault is an error even though the caller requested cancellation.
    assert!(
        engine
            .finish_interrupted_generation(
                &request,
                &events,
                Err(anyhow::anyhow!("fixture tool shutdown failed")),
                Ok(())
            )
            .await
            .is_err()
    );
    assert!(
        engine
            .finish_interrupted_generation(
                &request,
                &events,
                Ok(()),
                Err(anyhow::anyhow!("fixture delta persistence failed"))
            )
            .await
            .is_err()
    );
    assert!(received.try_recv().is_err());
    // A missing, mismatched or corrupted durable context cannot be called saved.
    let mut absent = request.clone();
    absent.session_id = Uuid::new_v4();
    assert!(
        engine
            .finish_interrupted_generation(&absent, &events, Ok(()), Ok(()))
            .await
            .is_err()
    );
    engine
        .store
        .save_checkpoint(request.session_id, Uuid::new_v4(), &context)
        .unwrap();
    assert!(
        engine
            .finish_interrupted_generation(&request, &events, Ok(()), Ok(()))
            .await
            .is_err()
    );
    engine
        .store
        .save_checkpoint(
            request.session_id,
            request.turn_id,
            &json!({"broken":"context"}),
        )
        .unwrap();
    assert!(
        engine
            .finish_interrupted_generation(&request, &events, Ok(()), Ok(()))
            .await
            .is_err()
    );
    assert!(received.try_recv().is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_admission_stops_before_provider_or_checkpoint_setup() {
    let (_directory, engine, request) = fixture();
    // Even an unconfigured provider must not turn an already stopped admission
    // into a setup failure, nor create new checkpoint/tool effects.
    let cancel = CancellationToken::new();
    cancel.cancel();
    let (events, mut receiver) = mpsc::channel(8);
    engine.run(request.clone(), events, cancel).await.unwrap();
    assert!(matches!(
        receiver.recv().await,
        Some(EngineEvent::Cancelled)
    ));
    assert!(receiver.try_recv().is_err());
    assert!(
        engine
            .store
            .checkpoint(request.session_id)
            .unwrap()
            .is_none()
    );
    assert!(
        engine
            .store
            .unresolved_actions(request.session_id)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn actual_delegation_inherits_parent_model_and_reasoning_on_wire() {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    let (_directory, engine, mut request) = fixture();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    request.provider = ProviderConfig {
        family: xylo_core::ApiFamily::OpenAiResponses,
        endpoint: format!("http://{}/v1", listener.local_addr().unwrap()),
        model: "gpt-6.1-sol".into(),
        context_tokens: 65536,
        reasoning_options: Some(vec!["low".into(), "high".into()]),
        reasoning_effort: Some("high".into()),
        ..Default::default()
    };
    let server = tokio::spawn(async move {
        let mut seen = Vec::new();
        for index in 0..3 {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            let mut length = 0;
            let mut first = String::new();
            socket.read_line(&mut first).await.unwrap();
            assert!(first.starts_with("POST /v1/responses "));
            loop {
                let mut line = String::new();
                socket.read_line(&mut line).await.unwrap();
                if let Some((name, value)) = line.split_once(':') {
                    if name.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                }
                if line == "\r\n" {
                    break;
                }
            }
            let mut bytes = vec![0; length];
            socket.read_exact(&mut bytes).await.unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            seen.push(body.clone());
            let output = if index == 0 {
                json!([{"type":"function_call","id":"fc-delegate","call_id":"delegate-call","name":"agent_delegate","arguments":"{\"task\":\"Investigate independently and report\"}"}])
            } else {
                json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":if index==1 {"Child complete"} else {"Parent complete"}}]}])
            };
            let response = json!({"id":format!("response-{index}"),"status":"completed","model":"gpt-6.1-sol","output":output,"usage":{"input_tokens":10,"output_tokens":5}});
            let (content_type, payload) = if body["stream"] == true {
                (
                    "text/event-stream",
                    format!(
                        "data: {}\n\n",
                        json!({"type":"response.completed","response":response})
                    ),
                )
            } else {
                ("application/json", response.to_string())
            };
            socket.get_mut().write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",payload.len()).as_bytes()).await.unwrap();
        }
        seen
    });
    let (events, mut receiver) = mpsc::channel(1024);
    tokio::time::timeout(
        Duration::from_secs(15),
        engine.run(request, events, CancellationToken::new()),
    )
    .await
    .unwrap()
    .unwrap();
    let seen = tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(seen.len(), 3);
    assert_eq!(seen[1]["stream"], false);
    for body in &seen {
        assert_eq!(body["model"], "gpt-6.1-sol");
        assert_eq!(body["reasoning"]["effort"], "high");
    }
    let mut completed = false;
    while let Ok(event) = receiver.try_recv() {
        if let EngineEvent::Completed { text, .. } = event {
            assert_eq!(text, "Parent complete");
            completed = true;
        }
    }
    assert!(completed);
    let child_result = seen[2]["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .unwrap();
    let result: Value = serde_json::from_str(child_result["output"].as_str().unwrap()).unwrap();
    assert_eq!(result["status"], "completed");
    assert_eq!(result["result"], "Child complete");
}
