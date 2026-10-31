use super::*;
use crate::model::ToolCall;
use parking_lot::Mutex;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;
use xylo_core::{ApiFamily, Conversation, PermissionMode, Usage, now_ms};
use xylo_services::ActionIntent;

fn fixture() -> (tempfile::TempDir, AgentEngine, TurnRequest) {
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::at(directory.path().join("profile")).unwrap();
    let store = Store::open(&paths.database).unwrap();
    let session = Uuid::new_v4();
    store
        .upsert_conversation(&Conversation {
            id: session,
            title: "Resume fixture".into(),
            workspace: directory.path().into(),
            updated_at: now_ms(),
            title_source: TitleSource::Prompt,
            pinned: false,
            archived: false,
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
        approved_rules: Vec::new(),
    };
    let engine = AgentEngine::new(
        store,
        paths,
        ToolSession::for_tests(PermissionMode::ReadOnly),
    );
    (directory, engine, request)
}

fn assistant_call(name: &str, args: Value) -> Message {
    Message::assistant_with_tool_calls(
        "",
        vec![ToolCall::new("provider-call-1", name, args.to_string())],
    )
}

fn journal(request: &TurnRequest, turn: Uuid, name: &str, args: &Value) -> ActionIntent {
    ActionIntent {
        id: Uuid::new_v4(),
        session_id: request.session_id,
        turn_id: turn,
        name: name.into(),
        arguments: args.clone(),
        writes: true,
        idempotency_key: ToolBroker::journal_key(None, "provider-call-1"),
    }
}

async fn restore(engine: &AgentEngine, request: &TurnRequest) -> Result<Vec<Message>> {
    let (events, _receiver) = mpsc::channel(256);
    engine
        .restored_messages(request, events, CancellationToken::new())
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn image_checkpoint_resume_preserves_refs_without_inline_image_data() {
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
    engine
        .store
        .save_checkpoint(
            request.session_id,
            request.turn_id,
            &serde_json::to_value(vec![user_message(&request)]).unwrap(),
        )
        .unwrap();
    let restored = restore(&engine, &request).await.unwrap();
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].attachments, request.attachments);
    let value = serde_json::to_value(&restored).unwrap();
    assert!(!value.to_string().contains("base64"));
    assert!(serde_json::to_vec(&value).unwrap().len() < 2048);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_restores_success_using_original_turn_before_new_prompt() {
    let (_directory, engine, mut request) = fixture();
    let old_turn = request.turn_id;
    let args = json!({"path":"already-written.txt","content":"original"});
    let intent = journal(&request, old_turn, "file_write", &args);
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
    let restored = restore(&engine, &request).await.unwrap();
    assert_eq!(restored.len(), 3);
    assert_eq!(restored[2].content, "Continue");
    let result: Value = serde_json::from_str(&restored[1].content).unwrap();
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
    let intent = journal(&request, request.turn_id, "file_write", &args);
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
    let restored = restore(&engine, &request).await.unwrap();
    assert_eq!(restored.len(), 2);
    assert_eq!(
        serde_json::from_str::<Value>(&restored[1].content).unwrap()["status"],
        "error"
    );
    assert!(!request.workspace.join("denied.txt").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calls_that_never_ran_wait_for_the_resuming_turn_and_are_not_run_for_a_new_one() {
    let (_directory, engine, mut request) = fixture();
    let args = json!({"path":"pending.txt"});
    engine
        .store
        .save_checkpoint(
            request.session_id,
            request.turn_id,
            &serde_json::to_value(vec![
                Message::user("Continue"),
                assistant_call("file_read", args),
            ])
            .unwrap(),
        )
        .unwrap();
    let resumed = restore(&engine, &request).await.unwrap();
    assert_eq!(resumed.len(), 2, "the pending call is left for the loop");
    request.turn_id = Uuid::new_v4();
    let fresh = restore(&engine, &request).await.unwrap();
    assert_eq!(fresh.len(), 4);
    assert!(fresh[2].content.contains("Not run"));
    assert_eq!(fresh[3].content, "Continue");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_completed_event_replays_receipt_without_model_or_duplicate_history() {
    let (_directory, engine, request) = fixture();
    let usage = Usage {
        input_tokens: 7,
        output_tokens: 11,
        elapsed_ms: 13,
        context_tokens: 5,
        cache_read_tokens: Some(3),
        cache_write_tokens: None,
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
    // The default provider has no model and would fail validation: receipt
    // delivery precedes every other run action.
    let (events, mut received) = mpsc::channel(32);
    engine
        .run(request.clone(), events, CancellationToken::new())
        .await
        .unwrap();
    match received.recv().await.unwrap() {
        EngineEvent::Completed { text, usage } => {
            assert_eq!(text, "already completed");
            assert_eq!(usage.output_tokens, 11);
            assert_eq!(
                usage.cache_read_tokens,
                Some(3),
                "cache counts survive the receipt"
            );
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
    let resumed = restore(&engine, &request).await.unwrap();
    assert_eq!(resumed.len(), 2);
    request.turn_id = Uuid::new_v4();
    let new_turn = restore(&engine, &request).await.unwrap();
    assert_eq!(new_turn.len(), 3);
    assert_eq!(new_turn.iter().filter(|m| m.role == Role::User).count(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_error_after_admission_is_classified_and_queues_learning_evidence() {
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
    let (events, mut receiver) = mpsc::channel(256);
    engine
        .run(request.clone(), events, CancellationToken::new())
        .await
        .unwrap();
    let mut terminal = None;
    while let Ok(event) = receiver.try_recv() {
        terminal = Some(event);
    }
    match terminal.unwrap() {
        EngineEvent::Error {
            message,
            retryable,
            kind,
        } => {
            assert!(message.contains("outcome-unknown"));
            assert!(!retryable);
            assert_eq!(kind, ErrorKind::Internal);
        }
        event => panic!("Unexpected terminal event: {event:?}"),
    }
    let learning = LearningService::new(engine.store.clone());
    assert_eq!(
        learning
            .status(&request.workspace.to_string_lossy())
            .unwrap()
            .pending_jobs,
        1
    );
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
async fn cancelled_admission_stops_before_provider_or_checkpoint_setup() {
    let (_directory, engine, request) = fixture();
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
}

#[test]
fn titles_are_reduced_to_one_plain_line() {
    assert_eq!(
        clean_title("\"Fix the parser.\"\nExtra").as_deref(),
        Some("Fix the parser")
    );
    assert_eq!(clean_title("  \n  "), None);
    assert!(clean_title(&"word ".repeat(100)).unwrap().chars().count() <= TITLE_CHARS);
}

/// How the fixture server answers one Responses request.
enum Reply {
    Output(Value),
    /// The text, stopped by the output limit (`response.incomplete`).
    Truncated(&'static str),
    /// Streams one text delta and then holds the connection open.
    Partial(&'static str),
    Status(u16, Value),
}

/// A Responses API server answering concurrent requests with `answer`.
async fn serve(
    answer: impl Fn(&Value) -> Reply + Send + Sync + 'static,
) -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = seen.clone();
    let answer = Arc::new(answer);
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let (answer, recorded) = (answer.clone(), recorded.clone());
            tokio::spawn(async move {
                let mut socket = BufReader::new(socket);
                let mut length = 0;
                let mut get = None;
                loop {
                    let mut line = String::new();
                    if socket.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    get.get_or_insert(line.starts_with("GET "));
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                if get == Some(true) {
                    // The served-context probe: this server reports nothing.
                    let _ = socket
                        .get_mut()
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                    return;
                }
                let mut bytes = vec![0; length];
                socket.read_exact(&mut bytes).await.unwrap();
                let body: Value = serde_json::from_slice(&bytes).unwrap();
                recorded.lock().push(body.clone());
                let (status, content_type, payload, hold) = match answer(&body) {
                    Reply::Output(output) => {
                        let response = json!({"status":"completed","model":body["model"],"output":output,"usage":{"input_tokens":10,"output_tokens":5}});
                        (
                            200,
                            "text/event-stream",
                            format!(
                                "data: {}\n\n",
                                json!({"type":"response.completed","response":response})
                            ),
                            false,
                        )
                    }
                    Reply::Truncated(text) => {
                        let response = json!({"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"model":body["model"],"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}],"usage":{"input_tokens":10,"output_tokens":64}});
                        (
                            200,
                            "text/event-stream",
                            format!(
                                "data: {}\n\n",
                                json!({"type":"response.incomplete","response":response})
                            ),
                            false,
                        )
                    }
                    Reply::Partial(text) => (
                        200,
                        "text/event-stream",
                        format!(
                            "data: {}\n\n",
                            json!({"type":"response.output_text.delta","delta":text})
                        ),
                        true,
                    ),
                    Reply::Status(status, body) => {
                        (status, "application/json", body.to_string(), false)
                    }
                };
                let head = if hold {
                    format!("HTTP/1.1 {status} Fixture\r\nContent-Type: {content_type}\r\n\r\n")
                } else {
                    format!(
                        "HTTP/1.1 {status} Fixture\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        payload.len()
                    )
                };
                let _ = socket.get_mut().write_all(head.as_bytes()).await;
                let _ = socket.get_mut().write_all(payload.as_bytes()).await;
                if hold {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            });
        }
    });
    (endpoint, seen)
}

fn message(text: &str) -> Reply {
    Reply::Output(
        json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}]),
    )
}

fn function_call(id: &str, name: &str, arguments: Value) -> Reply {
    Reply::Output(
        json!([{"type":"function_call","id":format!("fc-{id}"),"call_id":id,"name":name,"arguments":arguments.to_string()}]),
    )
}

fn is_title(body: &Value) -> bool {
    body["instructions"] == crate::prompts::title()
}

fn answered(body: &Value) -> bool {
    body["input"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["type"] == "function_call_output")
}

fn responses(endpoint: String) -> ProviderConfig {
    ProviderConfig {
        family: ApiFamily::OpenAiResponses,
        endpoint,
        model: "gpt-6.1-sol".into(),
        context_tokens: 65536,
        output_tokens: 1024,
        timeout_seconds: 10,
        reasoning_options: Some(vec!["low".into(), "high".into()]),
        reasoning_effort: Some("high".into()),
        ..Default::default()
    }
}

fn drain(receiver: &mut mpsc::Receiver<EngineEvent>) -> Vec<EngineEvent> {
    let mut events = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        events.push(event);
    }
    events
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delegation_returns_the_child_answer_with_parent_ids_and_the_first_turn_gets_a_title() {
    let (directory, engine, mut request) = fixture();
    let engine = AgentEngine::new(
        engine.store.clone(),
        engine.paths.clone(),
        ToolSession::for_tests(PermissionMode::ApproveForMe),
    );
    std::fs::write(directory.path().join("fixture.txt"), "CHILD_EVIDENCE").unwrap();
    let helper = crate::prompts::subagent(
        &crate::prompts::PromptContext::detect(
            directory.path(),
            PermissionMode::ApproveForMe,
            now_ms(),
            crate::prompts::PromptBudget::LARGEST,
        ),
        "x",
    );
    let helper_prefix = helper[..40].to_owned();
    let (endpoint, seen) = serve(move |body| {
        if is_title(body) {
            // Reasoning models may stop at the output limit; the text counts.
            return Reply::Truncated("\"Investigate the fixture.\"");
        }
        let child = body["instructions"]
            .as_str()
            .is_some_and(|text| text.starts_with(&helper_prefix));
        match (child, answered(body)) {
            (false, false) => function_call(
                "delegate-call",
                "agent_delegate",
                json!({"task":"Read fixture.txt and report"}),
            ),
            (true, false) => {
                function_call("child-read", "file_read", json!({"path":"fixture.txt"}))
            }
            (true, true) => message("Child complete"),
            (false, true) => message("Parent complete"),
        }
    })
    .await;
    request.provider = responses(endpoint);
    request.prompt = "Investigate fixture.txt".into();
    let (events, mut receiver) = mpsc::channel(1024);
    tokio::time::timeout(
        Duration::from_secs(20),
        engine.run(request.clone(), events, CancellationToken::new()),
    )
    .await
    .unwrap()
    .unwrap();
    let events = drain(&mut receiver);
    // The generated title follows the terminal event.
    let terminal = events
        .iter()
        .rev()
        .find(|event| !matches!(event, EngineEvent::TitleUpdated { .. }));
    assert!(
        matches!(terminal, Some(EngineEvent::Completed { text, .. }) if text == "Parent complete"),
        "{terminal:?}"
    );
    let delegate = events
        .iter()
        .find_map(|event| match event {
            EngineEvent::ToolStarted {
                action_id,
                name,
                parent_id: None,
                ..
            } if name == "agent_delegate" => Some(*action_id),
            _ => None,
        })
        .unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::ToolStarted { name, parent_id: Some(parent), .. } if name == "file_read" && *parent == delegate
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::Phase {
            phase: TurnPhase::Delegating
        }
    )));
    let bodies = seen.lock().clone();
    for body in &bodies {
        assert_eq!(body["model"], "gpt-6.1-sol");
        if is_title(body) {
            // Titles ask for the lowest effort and a small but usable limit.
            assert_eq!(body["reasoning"]["effort"], "low");
            assert_eq!(body["max_output_tokens"], 64);
        } else {
            assert_eq!(body["reasoning"]["effort"], "high");
        }
    }
    assert_eq!(bodies.iter().filter(|body| is_title(body)).count(), 1);
    let parent_final = bodies
        .iter()
        .find(|body| {
            !is_title(body)
                && answered(body)
                && body["instructions"]
                    .as_str()
                    .is_some_and(|text| !text.starts_with(&helper[..40]))
        })
        .unwrap();
    let output = parent_final["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .unwrap();
    assert!(
        output["output"]
            .as_str()
            .unwrap()
            .contains("Child complete")
    );
    // The system prompt is Xylo's, identical across the parent's calls.
    let parent_instructions = bodies
        .iter()
        .filter(|body| {
            !is_title(body)
                && !body["instructions"]
                    .as_str()
                    .unwrap()
                    .starts_with(&helper[..40])
        })
        .map(|body| body["instructions"].clone())
        .collect::<Vec<_>>();
    assert_eq!(parent_instructions.len(), 2);
    assert_eq!(parent_instructions[0], parent_instructions[1]);
    assert!(!parent_instructions[0].to_string().contains("Hermes"));
    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::TitleUpdated { title } if title == "Investigate the fixture"
    )));
    let conversation = engine
        .store
        .conversation(request.session_id)
        .unwrap()
        .unwrap();
    assert_eq!(conversation.title, "Investigate the fixture");
    assert_eq!(conversation.title_source, TitleSource::Generated);
    let completed = engine
        .store
        .completed_turn(request.session_id, request.turn_id)
        .unwrap()
        .unwrap();
    assert!(completed.usage.context_tokens > 0);
    assert!(completed.usage.input_tokens >= completed.usage.context_tokens);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_user_title_is_never_replaced() {
    let (_directory, engine, mut request) = fixture();
    engine
        .store
        .rename_conversation(request.session_id, "Mine", TitleSource::User)
        .unwrap();
    let (endpoint, seen) = serve(|body| {
        assert!(!is_title(body), "no title request for a user-named chat");
        message("Hello")
    })
    .await;
    request.provider = responses(endpoint);
    let (events, mut receiver) = mpsc::channel(256);
    engine
        .run(request.clone(), events, CancellationToken::new())
        .await
        .unwrap();
    assert!(
        !drain(&mut receiver)
            .iter()
            .any(|event| matches!(event, EngineEvent::TitleUpdated { .. }))
    );
    assert_eq!(seen.lock().len(), 1);
    assert_eq!(
        engine
            .store
            .conversation(request.session_id)
            .unwrap()
            .unwrap()
            .title,
        "Mine"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_title_never_fails_a_completed_turn() {
    let (_directory, engine, mut request) = fixture();
    let (endpoint, _) = serve(|body| {
        if is_title(body) {
            // Answer only after the consumer is gone, so announcing fails.
            std::thread::sleep(Duration::from_millis(300));
            message("A title")
        } else {
            message("Hello")
        }
    })
    .await;
    request.provider = responses(endpoint);
    let (events, mut receiver) = mpsc::channel(256);
    let consumer = tokio::spawn(async move {
        while let Some(event) = receiver.recv().await {
            if matches!(event, EngineEvent::Completed { .. }) {
                return true;
            }
        }
        false
    });
    tokio::time::timeout(
        Duration::from_secs(20),
        engine.run(request.clone(), events, CancellationToken::new()),
    )
    .await
    .unwrap()
    .expect("the committed turn stays completed");
    assert!(consumer.await.unwrap(), "Completed was delivered first");
    assert!(
        engine
            .store
            .completed_turn(request.session_id, request.turn_id)
            .unwrap()
            .is_some()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_keeps_the_streamed_text_as_an_interrupted_reply() {
    let (_directory, engine, mut request) = fixture();
    let (endpoint, _) = serve(|body| {
        if is_title(body) {
            message("A title")
        } else {
            Reply::Partial("Half an answer")
        }
    })
    .await;
    request.provider = responses(endpoint);
    engine.store.register_user_turn(&request).unwrap();
    let (events, mut receiver) = mpsc::channel(256);
    let cancel = CancellationToken::new();
    let run = engine.run(request.clone(), events, cancel.clone());
    let watcher = async {
        while let Some(event) = receiver.recv().await {
            if matches!(event, EngineEvent::Delta { .. }) {
                cancel.cancel();
            }
            if matches!(event, EngineEvent::Cancelled) {
                return true;
            }
        }
        false
    };
    let (result, cancelled) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(run, watcher)
    })
    .await
    .unwrap();
    result.unwrap();
    assert!(cancelled);
    let messages = engine.store.messages(request.session_id, 10).unwrap();
    let reply = messages.iter().find(|m| m.role == "assistant").unwrap();
    assert_eq!(reply.content, "Half an answer");
    assert_eq!(reply.status, MessageStatus::Interrupted);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejected_credentials_end_with_a_non_retryable_authentication_error() {
    let (_directory, engine, mut request) = fixture();
    let (endpoint, seen) =
        serve(|_| Reply::Status(401, json!({"error":{"code":"invalid_api_key"}}))).await;
    request.provider = responses(endpoint);
    let (events, mut receiver) = mpsc::channel(256);
    engine
        .run(request.clone(), events, CancellationToken::new())
        .await
        .unwrap();
    let terminal = drain(&mut receiver).pop().unwrap();
    assert!(matches!(
        terminal,
        EngineEvent::Error {
            kind: ErrorKind::Authentication,
            retryable: false,
            ..
        }
    ));
    // The main request is not retried; only it and the title request ran.
    assert!(seen.lock().iter().filter(|body| !is_title(body)).count() == 1);
}

/// A provider whose every response asks for a long-running shell command.
struct StalledChild;
impl ModelProvider for StalledChild {
    fn stream(&self, _: ModelRequest, _: CancellationToken) -> crate::model::EventStream {
        use crate::model::{FinishReason, StreamEvent};
        Box::pin(futures_util::stream::iter([
            Ok(StreamEvent::ToolCall {
                index: 0,
                id: Some("child-call".into()),
                name: Some("shell_command".into()),
                arguments: Some(
                    json!({"command":"printf started > child-started; sleep 30","timeout_seconds":60})
                        .to_string(),
                ),
            }),
            Ok(StreamEvent::Finished(FinishReason::ToolCalls)),
        ]))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn child_timeout_cancels_its_stalled_process_without_cancelling_the_parent() {
    let (_directory, engine, request) = fixture();
    let session = ToolSession::for_tests(PermissionMode::FullAccess);
    let engine = AgentEngine::new(engine.store.clone(), engine.paths.clone(), session);
    let (events, mut receiver) = mpsc::channel(256);
    let parent_cancel = CancellationToken::new();
    let broker = Arc::new(
        engine
            .broker(&request, events.clone(), parent_cancel.clone())
            .unwrap(),
    );
    let delegation = Arc::new(Delegation {
        provider: Arc::new(StalledChild),
        budget: ContextBudget::new(&ProviderConfig {
            context_tokens: 65536,
            ..Default::default()
        }),
        summary_deadline: Duration::from_secs(5),
        max_tokens: 256,
        parent: Arc::downgrade(&broker),
        events,
        slots: Arc::new(tokio::sync::Semaphore::new(2)),
        cancel: parent_cancel.clone(),
        timeout: Duration::from_millis(1500),
        runtime: tokio::runtime::Handle::current(),
        workspace: request.workspace.clone(),
        permissions: crate::tools::PermissionCell::new(PermissionMode::FullAccess),
    });
    let handler = delegation.handler();
    let call = crate::tools::DelegateCall {
        parent_key: "root:parent-call".into(),
        parent_action_id: Uuid::new_v4(),
        arguments: json!({"task":"Run the fixture tool"}),
    };
    let start = std::time::Instant::now();
    let result = tokio::task::spawn_blocking(move || handler(&call))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result["status"], "timeout");
    assert!(start.elapsed() < Duration::from_secs(10));
    assert!(!parent_cancel.is_cancelled());
    assert!(
        drain(&mut receiver)
            .iter()
            .any(|event| matches!(event, EngineEvent::ToolStarted { .. })),
        "the timeout must exercise a real host tool"
    );
    assert!(request.workspace.join("child-started").is_file());
    broker.shutdown().await.unwrap();
    let actions = engine.store.unresolved_actions(request.session_id).unwrap();
    assert!(actions.iter().all(|action| action.state != "running"));
}

/// A Chat Completions server answering connections strictly in order, as the
/// worker protocol fixtures do.
async fn sequential_chat(replies: Vec<Value>) -> (String, tokio::task::JoinHandle<Vec<Value>>) {
    sequential_chat_serving(None, replies).await
}

/// [`sequential_chat`] that also answers llama.cpp's `GET /props` with
/// `props`, as a local server reporting the context it serves.
async fn sequential_chat_serving(
    props: Option<Value>,
    replies: Vec<Value>,
) -> (String, tokio::task::JoinHandle<Vec<Value>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut bodies = Vec::new();
        for (index, delta) in replies.into_iter().enumerate() {
            // The served-context probe's GETs get 404: this server does not
            // report its context.
            let (mut socket, length) = loop {
                let (socket, _) = listener.accept().await.unwrap();
                let mut socket = BufReader::new(socket);
                let mut length = 0;
                let mut first = String::new();
                socket.read_line(&mut first).await.unwrap();
                loop {
                    let mut line = String::new();
                    socket.read_line(&mut line).await.unwrap();
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                if !first.starts_with("GET ") {
                    break (socket, length);
                }
                let response = match &props {
                    Some(props) if first.starts_with("GET /props ") => {
                        let props = props.to_string();
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{props}",
                            props.len()
                        )
                    }
                    _ => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_owned(),
                };
                let _ = socket.get_mut().write_all(response.as_bytes()).await;
            };
            let mut bytes = vec![0; length];
            socket.read_exact(&mut bytes).await.unwrap();
            bodies.push(serde_json::from_slice::<Value>(&bytes).unwrap());
            let tool = delta.get("tool_calls").is_some();
            let first = json!({"id":"fixture","object":"chat.completion.chunk","model":"fixture","choices":[{"index":0,"delta":delta,"finish_reason":null}]});
            let last = json!({"id":"fixture","object":"chat.completion.chunk","model":"fixture","choices":[{"index":0,"delta":{},"finish_reason":if tool {"tool_calls"} else {"stop"}}],"usage":{"prompt_tokens":100 + index,"completion_tokens":20,"total_tokens":120}});
            let body = format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n");
            socket
                .get_mut()
                .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes())
                .await
                .unwrap();
        }
        bodies
    });
    (endpoint, server)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streamed_chat_tool_call_runs_before_the_answer_and_the_title_follows_completion() {
    let (directory, engine, mut request) = fixture();
    std::fs::write(
        directory.path().join("sample.txt"),
        "durable fixture contents",
    )
    .unwrap();
    let (endpoint, server) = sequential_chat(vec![
        json!({"tool_calls":[{"index":0,"id":"fixture-call-1","type":"function","function":{"name":"file_read","arguments":json!({"path":"sample.txt"}).to_string()}}]}),
        json!({"content":"The contents of sample.txt are: durable fixture contents."}),
        json!({"content":"Read the sample file"}),
    ])
    .await;
    request.provider = ProviderConfig {
        endpoint,
        model: "fixture".into(),
        context_tokens: 65536,
        output_tokens: 1024,
        timeout_seconds: 10,
        ..Default::default()
    };
    request.prompt = "Read sample.txt using the file_read tool, then report its content.".into();
    engine.store.register_user_turn(&request).unwrap();
    let (events, mut receiver) = mpsc::channel(1024);
    tokio::time::timeout(
        Duration::from_secs(20),
        engine.run(request.clone(), events, CancellationToken::new()),
    )
    .await
    .unwrap()
    .unwrap();
    let events = drain(&mut receiver);
    let position = |check: &dyn Fn(&EngineEvent) -> bool| events.iter().position(check).unwrap();
    let started =
        position(&|e| matches!(e, EngineEvent::ToolStarted { name, .. } if name == "file_read"));
    let finished = position(&|e| matches!(e, EngineEvent::ToolFinished { success: true, .. }));
    let answer = position(
        &|e| matches!(e, EngineEvent::Delta { text } if text.contains("durable fixture contents")),
    );
    let completed = position(
        &|e| matches!(e, EngineEvent::Completed { text, usage } if text.contains("durable fixture contents") && usage.context_tokens == 101),
    );
    let title = position(
        &|e| matches!(e, EngineEvent::TitleUpdated { title } if title == "Read the sample file"),
    );
    // The title never delays the terminal event.
    assert!(started < finished && finished < answer && answer < completed && completed < title);
    assert_eq!(title, events.len() - 1);
    let bodies = server.await.unwrap();
    assert_eq!(bodies[2]["messages"][0]["content"], crate::prompts::title());
    let checkpoint: Vec<Message> = serde_json::from_value(
        engine
            .store
            .checkpoint(request.session_id)
            .unwrap()
            .unwrap()
            .messages,
    )
    .unwrap();
    assert!(
        checkpoint
            .iter()
            .any(|m| m.tool_call_id.as_deref() == Some("fixture-call-1"))
    );
}

/// Problems the Responses API reports for a function's parameter schema.
fn responses_schema_problems(path: &str, schema: &Value, problems: &mut Vec<String>) {
    let Some(fields) = schema.as_object() else {
        problems.push(format!("{path}: schema is not an object"));
        return;
    };
    for key in ["$schema", "$id", "$ref", "definitions", "$defs"] {
        if fields.contains_key(key) {
            problems.push(format!("{path}: unsupported keyword {key}"));
        }
    }
    let types: Vec<&str> = match &fields.get("type") {
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    if types.contains(&"array") && !fields.contains_key("items") {
        problems.push(format!("{path}: array schema missing items"));
    }
    if let Some(Value::Array(values)) = fields.get("enum")
        && values.is_empty()
    {
        problems.push(format!("{path}: empty enum"));
    }
    if let Some(properties) = fields.get("properties") {
        match properties.as_object() {
            Some(properties) => {
                for (name, property) in properties {
                    responses_schema_problems(&format!("{path}.{name}"), property, problems);
                }
            }
            None => problems.push(format!("{path}: properties is not an object")),
        }
    }
    if let Some(Value::Array(required)) = fields.get("required") {
        for name in required.iter().filter_map(Value::as_str) {
            if !fields
                .get("properties")
                .and_then(Value::as_object)
                .is_some_and(|properties| properties.contains_key(name))
            {
                problems.push(format!("{path}: required {name} is not a property"));
            }
        }
    }
    for key in ["items", "additionalProperties", "not"] {
        if let Some(nested) = fields.get(key).filter(|nested| nested.is_object()) {
            responses_schema_problems(&format!("{path}.{key}"), nested, problems);
        }
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        for (index, nested) in fields
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            responses_schema_problems(&format!("{path}.{key}[{index}]"), nested, problems);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_offered_tool_schema_is_valid_for_the_responses_api() {
    let (_directory, engine, request) = fixture();
    for mode in PermissionMode::ALL {
        let engine = AgentEngine::new(
            engine.store.clone(),
            engine.paths.clone(),
            ToolSession::for_tests(mode),
        );
        let (events, _receiver) = mpsc::channel(256);
        let broker = engine
            .broker(&request, events, CancellationToken::new())
            .unwrap();
        let mut problems = Vec::new();
        let mut names = HashSet::new();
        for definition in broker.initial_definitions() {
            let name = &definition.name;
            if !names.insert(name.clone()) {
                problems.push(format!("{name}: duplicate tool name"));
            }
            if name.is_empty()
                || name.len() > 64
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
            {
                problems.push(format!("{name}: invalid function name"));
            }
            if definition.description.trim().is_empty() || definition.description.len() > 1024 {
                problems.push(format!(
                    "{name}: description must be 1 to 1024 bytes, is {}",
                    definition.description.len()
                ));
            }
            if definition.parameters["type"] != "object" {
                problems.push(format!("{name}: top-level parameters must be an object"));
            }
            responses_schema_problems(name, &definition.parameters, &mut problems);
        }
        assert!(problems.is_empty(), "{mode:?}: {problems:#?}");
        broker.shutdown().await.unwrap();
    }
}

/// The system prompt holds nothing derived from the user's prompt, so it is
/// byte-identical across turns; what memory recalls for a prompt travels in
/// a quoted note after that prompt instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn system_prompt_is_identical_across_turns_and_recall_follows_each_prompt() {
    use xylo_core::{MemoryOrigin, MemoryRecord};
    let (_directory, engine, mut request) = fixture();
    let memory = xylo_services::SqliteMemory::new(engine.store.clone());
    for (content, origin) in [
        ("Tests run with cargo nextest", MemoryOrigin::UserTyped),
        (
            "Tests run with a script from a web page",
            MemoryOrigin::WebContent,
        ),
    ] {
        memory
            .remember_sync(MemoryRecord {
                id: Uuid::new_v4(),
                scope: request.workspace.to_string_lossy().into_owned(),
                content: content.into(),
                source: "fixture".into(),
                created_at: now_ms(),
                supersedes: None,
                origin,
            })
            .unwrap();
    }
    // One skill the model may use itself, one only the user may start.
    let extensions = xylo_services::extensions::ExtensionService::new(
        engine.paths.artifacts.parent().unwrap().join("extensions"),
    );
    for (name, manual) in [("tidy", false), ("release", true)] {
        let source = tempfile::tempdir().unwrap();
        std::fs::write(
            source.path().join("SKILL.md"),
            format!(
                "---\nname: {name}\ndescription: The {name} fixture skill\ndisable-model-invocation: {manual}\n---\nSteps."
            ),
        )
        .unwrap();
        let package = extensions.install_local(source.path()).unwrap();
        extensions.set_enabled(&package.id, true).unwrap();
    }
    let (events, _receiver) = mpsc::channel(256);
    let mut prompts = Vec::new();
    for prompt in [
        "How do the tests run?",
        "Rename the deploy script",
        "/release version 2",
    ] {
        request.turn_id = Uuid::new_v4();
        request.prompt = prompt.into();
        let broker = engine
            .broker(&request, events.clone(), CancellationToken::new())
            .unwrap();
        let system = AgentEngine::system_prompt(&request, &broker, PermissionMode::ReadOnly);
        let recalled = AgentEngine::recalled_context(&broker);
        broker.shutdown().await.unwrap();
        prompts.push((system, recalled));
    }
    assert_eq!(prompts[0].0, prompts[1].0);
    assert_eq!(
        prompts[0].0, prompts[2].0,
        "invoking a skill keeps the prefix"
    );
    assert!(!prompts[0].0.contains("nextest"));
    assert!(prompts[0].0.contains("The tidy fixture skill"));
    assert!(!prompts[0].0.contains("The release fixture skill"));
    let invoked = &prompts[2].1.as_ref().unwrap().content;
    assert!(invoked.contains("## Skills the user invoked"), "{invoked}");
    assert!(invoked.contains("The release fixture skill"), "{invoked}");
    let note = prompts[0].1.as_ref().unwrap();
    assert_eq!(note.role, Role::User);
    assert!(note.content.starts_with("<application_note>"));
    assert!(
        note.content
            .contains("stated by the user: \"Tests run with cargo nextest\"")
    );
    assert!(
        !note.content.contains("web page"),
        "untrusted facts are not recalled"
    );
    assert!(prompts[1].1.is_none(), "nothing matched the second prompt");
}

/// Across two real turns, the system prompt and tools sent to the provider
/// are byte-identical, and the first turn's recalled-context note is sent
/// right after its prompt and replayed unchanged in the second turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turns_share_a_cached_prefix_and_replay_their_recalled_context() {
    use xylo_core::{MemoryOrigin, MemoryRecord};
    let (_directory, engine, mut request) = fixture();
    xylo_services::SqliteMemory::new(engine.store.clone())
        .remember_sync(MemoryRecord {
            id: Uuid::new_v4(),
            scope: request.workspace.to_string_lossy().into_owned(),
            content: "Tests run with cargo nextest".into(),
            source: "fixture".into(),
            created_at: now_ms(),
            supersedes: None,
            origin: MemoryOrigin::UserTyped,
        })
        .unwrap();
    let (endpoint, server) = sequential_chat(vec![
        json!({"content":"Use cargo nextest."}),
        json!({"content":"Test runner"}),
        json!({"content":"Done."}),
    ])
    .await;
    request.provider = ProviderConfig {
        endpoint,
        model: "fixture".into(),
        context_tokens: 65536,
        output_tokens: 1024,
        timeout_seconds: 10,
        ..Default::default()
    };
    for prompt in ["How do the tests run?", "Thanks, that is all"] {
        request.turn_id = Uuid::new_v4();
        request.prompt = prompt.into();
        engine.store.register_user_turn(&request).unwrap();
        let (events, _receiver) = mpsc::channel(1024);
        tokio::time::timeout(
            Duration::from_secs(20),
            engine.run(request.clone(), events, CancellationToken::new()),
        )
        .await
        .unwrap()
        .unwrap();
    }
    let bodies = server.await.unwrap();
    let (first, second) = (&bodies[0], &bodies[2]);
    assert_eq!(first["tools"], second["tools"]);
    assert_eq!(first["messages"][0], second["messages"][0]);
    let sent = first["messages"].as_array().unwrap();
    assert_eq!(sent[1]["content"], "How do the tests run?");
    let note = sent[2]["content"].as_str().unwrap();
    assert!(note.starts_with("<application_note>") && note.contains("cargo nextest"));
    assert_eq!(sent.len(), 3);
    let replayed = second["messages"].as_array().unwrap();
    assert_eq!(replayed[..3], sent[..]);
    assert_eq!(replayed.last().unwrap()["content"], "Thanks, that is all");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_is_budgeted_to_the_context_its_local_server_serves() {
    let (_directory, engine, mut request) = fixture();
    let (endpoint, server) = sequential_chat_serving(
        Some(json!({"default_generation_settings":{"n_ctx":8192}})),
        vec![json!({"content":"Done."}), json!({"content":"Short title"})],
    )
    .await;
    request.provider = ProviderConfig {
        endpoint,
        model: "fixture".into(),
        context_tokens: 65536,
        output_tokens: 1024,
        timeout_seconds: 10,
        ..Default::default()
    };
    engine.store.register_user_turn(&request).unwrap();
    let (events, mut receiver) = mpsc::channel(1024);
    tokio::time::timeout(
        Duration::from_secs(20),
        engine.run(request.clone(), events, CancellationToken::new()),
    )
    .await
    .unwrap()
    .unwrap();
    let mut capacities = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        if let EngineEvent::ContextUsage { usage } = event {
            capacities.push(usage.capacity);
        }
    }
    assert!(!capacities.is_empty());
    assert!(
        capacities.iter().all(|capacity| *capacity == 8192),
        "{capacities:?}"
    );
    // The served window is small, so only the small resident tool set is sent.
    let bodies = server.await.unwrap();
    assert!(bodies[0]["tools"].as_array().unwrap().len() <= 8);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engines_sharing_a_served_context_cache_keep_the_window_of_earlier_turns() {
    let (_directory, engine, mut request) = fixture();
    let cache = xylo_services::providers::ServedContextCache::default();
    let engine = engine.with_served_contexts(cache.clone());
    let (endpoint, server) = sequential_chat_serving(
        Some(json!({"default_generation_settings":{"n_ctx":8192}})),
        vec![json!({"content":"Done."}), json!({"content":"Short title"})],
    )
    .await;
    request.provider = ProviderConfig {
        endpoint,
        model: "fixture".into(),
        context_tokens: 65536,
        output_tokens: 1024,
        timeout_seconds: 10,
        ..Default::default()
    };
    engine.store.register_user_turn(&request).unwrap();
    let (events, _receiver) = mpsc::channel(1024);
    tokio::time::timeout(
        Duration::from_secs(20),
        engine.run(request.clone(), events, CancellationToken::new()),
    )
    .await
    .unwrap()
    .unwrap();
    // The server is gone, so only the shared cache can still answer: the
    // worker's next engine for this session sees the same window.
    server.await.unwrap();
    assert_eq!(
        cache
            .served_context_tokens(&request.provider, &CancellationToken::new())
            .await,
        Some(8192)
    );
}
