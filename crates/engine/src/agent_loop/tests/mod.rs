//! The loop against a scripted provider and an in-memory tool executor.
use super::*;
use crate::model::{EventStream, FinishReason, StreamEvent, ToolCall};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use xylo_core::{ContextUsage, ProviderConfig, SteeringRequest, SteeringState};
use xylo_services::{DraftKey, DraftSave, DraftSubmission, DraftWrite};

/// One scripted provider step.
#[derive(Clone)]
enum Step {
    Event(StreamEvent),
    Fail(ModelError),
    Hang,
    /// Runs a side effect while the response streams.
    Effect(Arc<dyn Fn() + Send + Sync>),
}

fn text(text: &str) -> Step {
    Step::Event(StreamEvent::Text(text.into()))
}
fn call(index: u32, id: &str, name: &str, arguments: &str) -> Step {
    Step::Event(StreamEvent::ToolCall {
        index,
        id: Some(id.into()),
        name: Some(name.into()),
        arguments: Some(arguments.into()),
    })
}
fn finish(reason: FinishReason) -> Step {
    Step::Event(StreamEvent::Finished(reason))
}
fn usage(input: u64) -> Step {
    Step::Event(StreamEvent::Usage(crate::model::ModelUsage {
        input_tokens: input,
        output_tokens: 1,
    }))
}

/// Replays one script per request and records every request.
#[derive(Default)]
struct Scripted {
    scripts: Mutex<VecDeque<Vec<Step>>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl Scripted {
    fn new(scripts: Vec<Vec<Step>>) -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(scripts.into()),
            requests: Mutex::default(),
        })
    }
}

impl ModelProvider for Scripted {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> EventStream {
        self.requests.lock().push(request);
        let script = self
            .scripts
            .lock()
            .pop_front()
            .expect("unexpected model request");
        Box::pin(async_stream::stream! {
            for step in script {
                match step {
                    Step::Event(event) => yield Ok(event),
                    Step::Fail(error) => { yield Err(error); return; }
                    Step::Effect(effect) => effect(),
                    Step::Hang => {
                        cancel.cancelled().await;
                        yield Err(ModelError::Interrupted);
                        return;
                    }
                }
            }
        })
    }
}

/// In-memory tools: `read_*` tools are read-only and sleep briefly, the
/// rest write. Records start/finish order and the peak concurrency.
#[derive(Default)]
struct Tools {
    log: Mutex<Vec<String>>,
    running: Mutex<usize>,
    peak: Mutex<usize>,
    on_execute: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl ToolExecutor for Tools {
    fn schemas(&self) -> Vec<ToolSchema> {
        ["read_a", "write_a", "file_read"]
            .iter()
            .map(|name| ToolSchema {
                name: (*name).into(),
                description: "fixture".into(),
                parameters: json!({"type":"object"}),
            })
            .collect()
    }
    fn deferred_count(&self) -> usize {
        3
    }
    fn access(&self, name: &str, _: &Value) -> Option<Access> {
        match name {
            "read_a" | "file_read" => Some(Access::ReadOnly),
            "write_a" => Some(Access::Writes),
            _ => None,
        }
    }
    fn execute(&self, call_id: &str, name: &str, arguments: Value) -> anyhow::Result<String> {
        if let Some(hook) = &*self.on_execute.lock() {
            hook();
        }
        {
            let mut running = self.running.lock();
            *running += 1;
            let mut peak = self.peak.lock();
            *peak = (*peak).max(*running);
        }
        self.log.lock().push(format!("start {call_id}"));
        std::thread::sleep(Duration::from_millis(if name == "write_a" {
            5
        } else {
            60
        }));
        self.log.lock().push(format!("end {call_id}"));
        *self.running.lock() -= 1;
        Ok(format!("{name} ok {arguments}"))
    }
}

struct Harness {
    _directory: tempfile::TempDir,
    store: Store,
    session: Uuid,
    turn: Uuid,
    events: mpsc::Receiver<EngineEvent>,
    agent: AgentLoop,
    tools: Arc<Tools>,
    provider: Arc<Scripted>,
}

fn harness(scripts: Vec<Vec<Step>>) -> Harness {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("xylo.db")).unwrap();
    let (sender, events) = mpsc::channel(1024);
    let tools = Arc::new(Tools::default());
    let provider = Scripted::new(scripts);
    let session = Uuid::new_v4();
    let turn = Uuid::new_v4();
    store
        .upsert_conversation(&xylo_core::Conversation {
            id: session,
            title: "Loop fixture".into(),
            workspace: directory.path().into(),
            updated_at: xylo_core::now_ms(),
            title_source: Default::default(),
            pinned: false,
            archived: false,
        })
        .unwrap();
    let config = ProviderConfig {
        model: "fixture".into(),
        context_tokens: 65536,
        output_tokens: 256,
        ..Default::default()
    };
    let agent = AgentLoop {
        provider: provider.clone(),
        budget: ContextBudget::new(&config),
        summary_deadline: Duration::from_secs(5),
        tools: tools.clone(),
        system: "Fixture system prompt".into(),
        max_tokens: 256,
        max_model_calls: MAX_MODEL_CALLS,
        events: sender,
        journal: Some(Journal {
            store: store.clone(),
            session,
            turn,
            steering: None,
        }),
        permissions: None,
        cancel: CancellationToken::new(),
    };
    Harness {
        _directory: directory,
        store,
        session,
        turn,
        events,
        agent,
        tools,
        provider,
    }
}

fn drain(events: &mut mpsc::Receiver<EngineEvent>) -> Vec<EngineEvent> {
    let mut drained = Vec::new();
    while let Ok(event) = events.try_recv() {
        drained.push(event);
    }
    drained
}

fn deltas(events: &[EngineEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::Delta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn checkpoint(h: &Harness) -> Vec<Message> {
    serde_json::from_value(h.store.checkpoint(h.session).unwrap().unwrap().messages).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_only_answer_finishes_in_one_call_with_ordered_events() {
    let mut h = harness(vec![vec![
        text("Hello "),
        text("there"),
        usage(42),
        finish(FinishReason::Stop),
    ]]);
    let outcome = h.agent.run(vec![Message::user("Hi")]).await.unwrap();
    assert_eq!(outcome.text, "Hello there");
    assert!(!outcome.stopped_early);
    assert_eq!(outcome.usage.context_tokens, 42);
    assert_eq!(outcome.messages.len(), 2);
    let events = drain(&mut h.events);
    assert_eq!(deltas(&events), "Hello there");
    let phases = events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::Phase { phase } => Some(phase.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(phases, vec![TurnPhase::Thinking, TurnPhase::Responding]);
    let usages = events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::ContextUsage { usage } => Some(usage.clone()),
            _ => None,
        })
        .collect::<Vec<ContextUsage>>();
    assert_eq!(usages.len(), 2);
    assert_eq!(usages[0].provider_input, None);
    assert_eq!(usages[1].provider_input, Some(42));
    assert_eq!(usages[1].deferred_tool_count, 3);
    let request = &h.provider.requests.lock()[0];
    assert_eq!(request.messages[0].content, "Fixture system prompt");
    assert_eq!(request.messages[0].role, Role::System);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_tool_round_checkpoints_before_and_after_and_keeps_the_system_prompt_stable() {
    let mut h = harness(vec![
        vec![
            text("Looking."),
            call(0, "c1", "read_a", "{\"path\":\"x\"}"),
            finish(FinishReason::ToolCalls),
        ],
        vec![text("Done."), finish(FinishReason::Stop)],
    ]);
    let checkpoints = Arc::new(Mutex::new(Vec::new()));
    let (store, session, seen) = (h.store.clone(), h.session, checkpoints.clone());
    *h.tools.on_execute.lock() = Some(Box::new(move || {
        let saved: Vec<Message> =
            serde_json::from_value(store.checkpoint(session).unwrap().unwrap().messages).unwrap();
        seen.lock().push(saved);
    }));
    let outcome = h.agent.run(vec![Message::user("Read x")]).await.unwrap();
    assert_eq!(outcome.text, "Done.");
    // Before dispatch the assistant call was durable without a result.
    let before = &checkpoints.lock()[0];
    assert_eq!(before.last().unwrap().tool_calls[0].id, "c1");
    // After the batch the result is durable too.
    let after = checkpoint(&h);
    assert_eq!(after.last().unwrap().tool_call_id.as_deref(), Some("c1"));
    assert!(after.last().unwrap().content.starts_with("read_a ok"));
    assert!(after.iter().all(|m| m.content != "Fixture system prompt"));
    let requests = h.provider.requests.lock();
    assert_eq!(requests[0].messages[0], requests[1].messages[0]);
    let events = drain(&mut h.events);
    let delta = events
        .iter()
        .position(|e| matches!(e, EngineEvent::Delta { .. }))
        .unwrap();
    let tools = events
        .iter()
        .position(|e| {
            matches!(
                e,
                EngineEvent::Phase {
                    phase: TurnPhase::RunningTools
                }
            )
        })
        .unwrap();
    assert!(delta < tools, "text must be flushed before the tool batch");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reads_run_in_parallel_and_writes_run_alone_in_order() {
    let mut h = harness(vec![
        vec![
            call(0, "r1", "read_a", "{}"),
            call(1, "r2", "read_a", "{}"),
            call(2, "w1", "write_a", "{\"n\":1}"),
            call(3, "w2", "write_a", "{\"n\":2}"),
            call(4, "r3", "read_a", "{}"),
            finish(FinishReason::ToolCalls),
        ],
        vec![text("ok"), finish(FinishReason::Stop)],
    ]);
    let outcome = h.agent.run(vec![Message::user("go")]).await.unwrap();
    let log = h.tools.log.lock().clone();
    let position = |entry: &str| log.iter().position(|e| e == entry).unwrap();
    // Both reads started before either finished.
    assert!(position("start r2") < position("end r1"));
    assert_eq!(*h.tools.peak.lock(), 2);
    // Writes waited for the reads and for each other, in call order.
    assert!(position("end r1") < position("start w1"));
    assert!(position("end r2") < position("start w1"));
    assert!(position("end w1") < position("start w2"));
    assert!(position("end w2") < position("start r3"));
    let results = outcome
        .messages
        .iter()
        .filter_map(|m| m.tool_call_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(results, ["r1", "r2", "w1", "w2", "r3"]);
    drain(&mut h.events);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_names_and_invalid_arguments_become_tool_errors_and_aliases_resolve() {
    let h = harness(vec![
        vec![
            call(0, "a1", "read", "{\"path\":\"x\"}"),
            call(1, "a2", "teleport", "{}"),
            call(2, "a3", "read_a", "{not json"),
            call(3, "a4", "read_a", "[1]"),
            finish(FinishReason::ToolCalls),
        ],
        vec![text("recovered"), finish(FinishReason::Stop)],
    ]);
    let outcome = h.agent.run(vec![Message::user("go")]).await.unwrap();
    assert_eq!(outcome.text, "recovered");
    assert_eq!(h.tools.log.lock().len(), 2, "only the alias call ran");
    let result = |id: &str| {
        outcome
            .messages
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some(id))
            .unwrap()
            .content
            .clone()
    };
    assert!(result("a1").starts_with("file_read ok"));
    assert!(result("a2").contains("search_tools"));
    assert!(result("a3").contains("not valid JSON"));
    assert!(result("a4").contains("must be a JSON object"));
    let assistant = &outcome.messages[1];
    assert_eq!(assistant.tool_calls[0].name, "file_read");
    assert_eq!(assistant.tool_calls[1].name, "teleport");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_mid_stream_reports_the_streamed_text() {
    let h = harness(vec![vec![text("partial answer"), Step::Hang]]);
    let cancel = h.agent.cancel.clone();
    let run = h.agent.run(vec![Message::user("go")]);
    let stopper = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
    };
    let (failure, ()) = tokio::join!(run, stopper);
    let failure = failure.unwrap_err();
    assert!(failure.cancelled());
    assert_eq!(failure.streamed, "partial answer");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_after_is_honoured_and_a_retried_partial_reply_is_not_repeated() {
    let mut h = harness(vec![
        vec![Step::Fail(ModelError::RateLimited {
            retry_after: Some(Duration::from_millis(40)),
            message: "slow down".into(),
        })],
        vec![text("Hel"), Step::Fail(ModelError::transient("reset"))],
        vec![text("Hello"), text(" world"), finish(FinishReason::Stop)],
    ]);
    let outcome = h.agent.run(vec![Message::user("go")]).await.unwrap();
    assert_eq!(outcome.text, "Hello world");
    let events = drain(&mut h.events);
    assert_eq!(deltas(&events), "Hello world");
    let retries = events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::Phase {
                phase: TurnPhase::Retrying { attempt, delay_ms },
            } => Some((*attempt, *delay_ms)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(retries.len(), 2);
    assert_eq!(retries[0], (1, 40));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_diverging_retry_is_marked_and_permanent_errors_are_not_retried() {
    let mut h = harness(vec![
        vec![
            text("First try"),
            Step::Fail(ModelError::transient("reset")),
        ],
        vec![text("Second"), finish(FinishReason::Stop)],
    ]);
    h.agent.run(vec![Message::user("go")]).await.unwrap();
    assert_eq!(deltas(&drain(&mut h.events)), "First try\n\nSecond");
    let h = harness(vec![vec![Step::Fail(ModelError::Authentication(
        "401".into(),
    ))]]);
    let failure = h.agent.run(vec![Message::user("go")]).await.unwrap_err();
    assert!(matches!(
        failure.error.downcast_ref::<ModelError>(),
        Some(ModelError::Authentication(_))
    ));
    assert_eq!(h.provider.requests.lock().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failure_after_a_tool_call_fragment_is_never_replayed() {
    let h = harness(vec![vec![
        call(0, "c1", "write_a", "{"),
        Step::Fail(ModelError::transient("reset")),
    ]]);
    assert!(h.agent.run(vec![Message::user("go")]).await.is_err());
    assert_eq!(h.provider.requests.lock().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_step_limit_surfaces_the_models_own_summary() {
    let mut h = harness(vec![
        vec![
            call(0, "c1", "read_a", "{}"),
            finish(FinishReason::ToolCalls),
        ],
        vec![
            text("Summary: read the file; tests remain."),
            call(0, "c2", "read_a", "{}"),
            finish(FinishReason::ToolCalls),
        ],
    ]);
    h.agent.max_model_calls = 2;
    let outcome = h.agent.run(vec![Message::user("go")]).await.unwrap();
    assert!(outcome.stopped_early);
    assert_eq!(outcome.text, "Summary: read the file; tests remain.");
    assert_eq!(
        h.tools.log.lock().len(),
        2,
        "the final call's tools never ran"
    );
    let last = outcome.messages.last().unwrap();
    assert_eq!(last.tool_call_id.as_deref(), Some("c2"));
    assert!(last.content.contains("step limit"));
    let requests = h.provider.requests.lock();
    let note = application_note(crate::prompts::step_limit());
    assert_eq!(requests[1].messages.last().unwrap(), &note);
    assert_eq!(note.role, Role::User);
    assert!(requests[0].messages.iter().all(|m| *m != note));
    assert_eq!(
        requests[1]
            .messages
            .iter()
            .filter(|m| m.role == Role::System)
            .count(),
        1,
        "only the system prompt is a system message"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resumed_turn_runs_only_the_calls_without_results() {
    let h = harness(vec![vec![text("resumed"), finish(FinishReason::Stop)]]);
    let messages = vec![
        Message::user("go"),
        Message::assistant_with_tool_calls(
            "",
            vec![
                ToolCall::new("done", "read_a", "{}"),
                ToolCall::new("pending", "read_a", "{}"),
            ],
        ),
        Message::tool_result("done", "earlier result"),
    ];
    let outcome = h.agent.run(messages).await.unwrap();
    assert_eq!(*h.tools.log.lock(), ["start pending", "end pending"]);
    assert_eq!(outcome.text, "resumed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guidance_is_applied_after_a_complete_tool_round() {
    let mut h = harness(vec![
        vec![
            call(0, "c1", "read_a", "{}"),
            finish(FinishReason::ToolCalls),
        ],
        vec![text("adjusted"), finish(FinishReason::Stop)],
    ]);
    let steering = SteeringControl::new(h.store.clone());
    if let Some(journal) = &mut h.agent.journal {
        journal.steering = Some(steering.clone());
    }
    let workspace = h._directory.path().to_path_buf();
    let (store, session, turn) = (h.store.clone(), h.session, h.turn);
    let queued = Arc::new(Mutex::new(None));
    let guidance_id = queued.clone();
    *h.tools.on_execute.lock() = Some(Box::new(move || {
        if guidance_id.lock().is_none() {
            *guidance_id.lock() = Some(queue_guidance(&store, session, turn, &workspace));
            steering.notify();
        }
    }));
    h.store
        .save_checkpoint(h.session, h.turn, &json!([{"role":"user","content":"go"}]))
        .unwrap();
    let outcome = h.agent.run(vec![Message::user("go")]).await.unwrap();
    assert_eq!(outcome.text, "adjusted");
    let requests = h.provider.requests.lock();
    let first: HashMap<_, _> = requests[0]
        .messages
        .iter()
        .map(|m| (m.content.clone(), m.role))
        .collect();
    assert!(!first.contains_key("Prefer the smaller fix"));
    let second = &requests[1].messages;
    let guidance = second
        .iter()
        .position(|m| m.content == "Prefer the smaller fix")
        .unwrap();
    assert_eq!(second[guidance - 1].tool_call_id.as_deref(), Some("c1"));
    let events = drain(&mut h.events);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::Steering { .. }))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_loops_report_nothing_but_their_answer() {
    let mut h = harness(vec![vec![text("child answer"), finish(FinishReason::Stop)]]);
    h.agent.journal = None;
    let outcome = h.agent.run(vec![Message::user("task")]).await.unwrap();
    assert_eq!(outcome.text, "child answer");
    assert!(drain(&mut h.events).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_permission_change_between_calls_is_announced_exactly_once() {
    use crate::tools::PermissionCell;
    use xylo_core::PermissionMode;
    let mut h = harness(vec![
        vec![
            call(0, "c1", "read_a", "{}"),
            finish(FinishReason::ToolCalls),
        ],
        vec![
            call(0, "c2", "read_a", "{}"),
            finish(FinishReason::ToolCalls),
        ],
        vec![text("done"), finish(FinishReason::Stop)],
    ]);
    let cell = PermissionCell::new(PermissionMode::AskForApproval);
    h.agent.permissions = Some((cell.clone(), PermissionMode::AskForApproval));
    let changed = Mutex::new(false);
    *h.tools.on_execute.lock() = Some(Box::new(move || {
        let mut changed = changed.lock();
        if !*changed {
            *changed = true;
            cell.set(PermissionMode::ReadOnly);
        }
    }));
    h.agent.run(vec![Message::user("go")]).await.unwrap();
    let note =
        application_note(&crate::prompts::permission_change(PermissionMode::ReadOnly)).content;
    let requests = h.provider.requests.lock();
    let count = |request: &ModelRequest| {
        request
            .messages
            .iter()
            .filter(|m| m.role == Role::User && m.content == note)
            .count()
    };
    assert_eq!(count(&requests[0]), 0);
    assert_eq!(count(&requests[1]), 1);
    assert_eq!(
        count(&requests[2]),
        1,
        "the note is announced once and kept"
    );
    assert_eq!(requests[0].messages[0], requests[2].messages[0]);
    let note_at = requests[1]
        .messages
        .iter()
        .position(|m| m.content == note)
        .unwrap();
    assert_eq!(
        requests[1].messages[note_at - 1].tool_call_id.as_deref(),
        Some("c1")
    );
}

/// Queues "Prefer the smaller fix" for the turn the way the host does.
fn queue_guidance(store: &Store, session: Uuid, turn: Uuid, workspace: &std::path::Path) -> Uuid {
    let key = DraftKey {
        session_id: session,
        workspace: Some(workspace.to_path_buf()),
    };
    let DraftSave::Saved(saved) = store
        .save_draft(&DraftWrite {
            key: key.clone(),
            expected_revision: 0,
            mutation_id: Uuid::new_v4(),
            text: "Prefer the smaller fix".into(),
            attachments: Vec::new(),
        })
        .unwrap()
    else {
        panic!("draft conflict");
    };
    let request = SteeringRequest {
        id: Uuid::new_v4(),
        session_id: session,
        turn_id: turn,
        text: saved.text.clone(),
    };
    let outcome = store
        .queue_steering(
            &request,
            &DraftSubmission {
                key,
                expected_revision: saved.revision,
                text: saved.text,
                attachments: Vec::new(),
            },
        )
        .unwrap();
    assert_eq!(outcome.receipt.state, SteeringState::Queued);
    request.id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guidance_during_the_final_answer_continues_the_task() {
    type Target = (Store, Uuid, Uuid, SteeringControl, std::path::PathBuf);
    let queued: Arc<Mutex<Option<Target>>> = Arc::default();
    let hook = queued.clone();
    let mut h = harness(vec![
        vec![
            text("First answer."),
            Step::Effect(Arc::new(move || {
                let (store, session, turn, steering, workspace) = hook.lock().clone().unwrap();
                queue_guidance(&store, session, turn, &workspace);
                steering.notify();
            })),
            finish(FinishReason::Stop),
        ],
        vec![text("Smaller fix applied."), finish(FinishReason::Stop)],
    ]);
    let steering = SteeringControl::new(h.store.clone());
    *queued.lock() = Some((
        h.store.clone(),
        h.session,
        h.turn,
        steering.clone(),
        h._directory.path().to_path_buf(),
    ));
    if let Some(journal) = &mut h.agent.journal {
        journal.steering = Some(steering);
    }
    h.store
        .save_checkpoint(h.session, h.turn, &json!([{"role":"user","content":"go"}]))
        .unwrap();
    let outcome = h.agent.run(vec![Message::user("go")]).await.unwrap();
    assert_eq!(outcome.text, "Smaller fix applied.");
    let requests = h.provider.requests.lock();
    assert_eq!(requests.len(), 2);
    let second = &requests[1].messages;
    assert_eq!(second[second.len() - 2].content, "First answer.");
    assert_eq!(second.last().unwrap().content, "Prefer the smaller fix");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reused_provider_call_ids_are_made_unique_before_dispatch() {
    let h = harness(vec![
        vec![
            call(0, "call_0", "read_a", "{\"n\":1}"),
            call(1, "call_0", "read_a", "{\"n\":2}"),
            finish(FinishReason::ToolCalls),
        ],
        vec![
            call(0, "call_0", "read_a", "{\"n\":3}"),
            finish(FinishReason::ToolCalls),
        ],
        vec![text("done"), finish(FinishReason::Stop)],
    ]);
    let outcome = h.agent.run(vec![Message::user("go")]).await.unwrap();
    let mut started = h
        .tools
        .log
        .lock()
        .iter()
        .filter_map(|entry| entry.strip_prefix("start ").map(str::to_owned))
        .collect::<Vec<_>>();
    started.sort();
    assert_eq!(started, ["call_0", "call_0#2", "call_0#3"]);
    let calls = outcome
        .messages
        .iter()
        .flat_map(|m| m.tool_calls.iter().map(|c| c.id.clone()))
        .collect::<Vec<_>>();
    assert_eq!(calls, ["call_0", "call_0#2", "call_0#3"]);
    let results = outcome
        .messages
        .iter()
        .filter_map(|m| m.tool_call_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(results, calls);
    assert!(
        outcome
            .messages
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("call_0#3"))
            .unwrap()
            .content
            .contains("\"n\":3")
    );
}

/// Text that tokenizes roughly as written (unlike repeated characters).
fn noise(words: usize) -> String {
    (0..words).map(|i| format!("w{i:x} ")).collect()
}

fn started_calls(h: &Harness) -> Vec<String> {
    h.tools
        .log
        .lock()
        .iter()
        .filter_map(|entry| entry.strip_prefix("start ").map(str::to_owned))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_id_compacted_out_of_the_conversation_is_never_reused() {
    let mut h = harness(vec![
        // The context summary that drops the earlier rounds.
        vec![
            text("User requests: read the files. Work done: read every file. Next: answer."),
            finish(FinishReason::Stop),
        ],
        vec![
            call(0, "call-0", "read_a", "{\"n\":1}"),
            finish(FinishReason::ToolCalls),
        ],
        vec![text("done"), finish(FinishReason::Stop)],
    ]);
    h.agent.budget = ContextBudget::new(&ProviderConfig {
        model: "unrecognized-custom-model".into(),
        context_tokens: 8192,
        output_tokens: 512,
        ..Default::default()
    });
    let mut history = vec![Message::user("Read the files")];
    for index in 0..12 {
        let id = format!("call-{index}");
        history.push(Message::assistant_with_tool_calls(
            "",
            vec![ToolCall::new(&id, "read_a", "{}")],
        ));
        history.push(Message::tool_result(id, noise(300)));
    }
    let outcome = h.agent.run(history).await.unwrap();
    assert!(outcome.messages[0].summary, "the history was compacted");
    assert!(
        !outcome
            .messages
            .iter()
            .any(|m| m.tool_calls.iter().any(|c| c.id == "call-0")),
        "the compacted call is gone from the conversation"
    );
    assert_eq!(started_calls(&h), ["call-0#2"]);
    assert!(
        checkpoint(&h)
            .iter()
            .any(|m| m.tool_call_id.as_deref() == Some("call-0#2"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resumed_turn_never_reuses_an_id_only_its_journal_remembers() {
    let h = harness(vec![
        vec![
            call(0, "call_0", "read_a", "{\"n\":2}"),
            finish(FinishReason::ToolCalls),
        ],
        vec![text("done"), finish(FinishReason::Stop)],
    ]);
    // An earlier step of this turn ran call_0, then a compaction dropped it
    // from the checkpoint the turn resumes from.
    for key in ["root:call_0", "root:call_0#2"] {
        h.store
            .begin_action(&xylo_services::ActionIntent {
                id: Uuid::new_v4(),
                session_id: h.session,
                turn_id: h.turn,
                name: "read_a".into(),
                arguments: json!({"n": 1}),
                writes: false,
                idempotency_key: key.into(),
            })
            .unwrap();
    }
    h.agent.run(vec![Message::user("go")]).await.unwrap();
    assert_eq!(started_calls(&h), ["call_0#3"]);
}
