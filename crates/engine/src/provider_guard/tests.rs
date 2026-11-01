use super::*;
use crate::model::{FinishReason, StreamEvent, ToolCall};
use record::Record;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Text that tokenizes roughly as written (unlike repeated characters).
fn noise(words: usize) -> String {
    (0..words).map(|i| format!("w{i:x} ")).collect()
}

fn config(timeout_seconds: u64) -> ProviderConfig {
    ProviderConfig {
        model: "unrecognized-custom-model".into(),
        context_tokens: 8192,
        output_tokens: 512,
        timeout_seconds,
        ..Default::default()
    }
}

fn round(messages: &mut Vec<Message>, index: usize, size: usize) {
    let id = format!("call-{index}");
    messages.push(Message::assistant_with_tool_calls(
        "",
        vec![ToolCall::new(&id, "file_read", "{}")],
    ));
    messages.push(Message::tool_result(id, noise(size)));
}

/// Answers every request with fixed text; counts requests.
struct Fixture {
    calls: Arc<AtomicUsize>,
    reply: &'static str,
    open: bool,
}

impl ModelProvider for Fixture {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> EventStream {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(request.tools.is_empty());
        let (reply, open) = (self.reply, self.open);
        Box::pin(async_stream::stream! {
            yield Ok(StreamEvent::Text(reply.into()));
            if open {
                std::future::pending::<()>().await;
            }
            yield Ok(StreamEvent::Finished(FinishReason::Stop));
        })
    }
}

fn fixture(reply: &'static str, open: bool) -> (Arc<Fixture>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    (
        Arc::new(Fixture {
            calls: calls.clone(),
            reply,
            open,
        }),
        calls,
    )
}

#[tokio::test]
async fn long_history_is_replaced_by_a_durable_model_summary() {
    let budget = ContextBudget::new(&config(5));
    let (summarizer, calls) = fixture(
        "User requests: refactor the parser. Work done: read src/parser.rs. Next steps: run tests.",
        false,
    );
    let system = vec![Message::system("Policy")];
    let mut history = vec![Message::user("Refactor the parser")];
    for index in 0..12 {
        round(&mut history, index, 300);
    }
    let mut view = system.clone();
    view.extend_from_slice(&history);
    assert!(budget.over_threshold(&view, &[], 512));
    let plan = budget
        .summary_plan(&system, &history, &[], 512, Pressure::Normal)
        .unwrap();
    let condensed = budget
        .summarize(
            summarizer.as_ref(),
            &history,
            &plan,
            Duration::from_secs(5),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(condensed[0].summary);
    assert!(condensed[0].content.contains("refactor the parser"));
    assert_eq!(condensed[1].content, "Refactor the parser");
    assert!(condensed.len() < history.len());
    for (index, message) in condensed.iter().enumerate() {
        if let Some(id) = &message.tool_call_id {
            assert!(
                condensed[..index]
                    .iter()
                    .any(|m| m.tool_calls.iter().any(|c| &c.id == id))
            );
        }
    }
    let mut fitted = system.clone();
    fitted.extend(condensed.clone());
    assert!(!budget.over_threshold(&fitted, &[], 512));
    // The summary survives a checkpoint round trip and is reused, not redone.
    let restored: Vec<Message> =
        serde_json::from_value(serde_json::to_value(&condensed).unwrap()).unwrap();
    assert!(restored[0].summary);
    let usage = budget.usage(&fitted, &[], 512);
    assert!(usage.compacted && usage.summary > 0 && usage.system > 0);
    assert_eq!(usage.capacity, 8192);
}

#[test]
fn a_short_history_is_not_worth_a_summary() {
    let budget = ContextBudget::new(&config(5));
    let history = vec![Message::user("small"), Message::assistant("reply")];
    assert!(
        budget
            .summary_plan(
                &[Message::system("Policy")],
                &history,
                &[],
                512,
                Pressure::Normal
            )
            .is_none()
    );
}

#[tokio::test]
async fn summarizer_has_its_own_deadline() {
    let budget = ContextBudget::new(&config(5));
    let (stalled, _) = fixture("never finishes", true);
    let mut history = vec![Message::user("Refactor")];
    for index in 0..12 {
        round(&mut history, index, 300);
    }
    let plan = budget
        .summary_plan(
            &[Message::system("Policy")],
            &history,
            &[],
            512,
            Pressure::Normal,
        )
        .unwrap();
    let error = budget
        .summarize(
            stalled.as_ref(),
            &history,
            &plan,
            Duration::from_millis(50),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ModelError::Timeout(_)));
}

#[test]
fn compaction_preserves_current_request_and_original_history() {
    let budget = ContextBudget::new(&config(2));
    let messages = vec![
        Message::system("Keep approval policy"),
        Message::user(noise(3200)),
        Message::assistant(noise(1600)),
        Message::user("Continue the current task"),
    ];
    let original = messages[1].content.len();
    let view = budget.fit(messages.clone(), &[], 512).unwrap().0;
    assert_eq!(view.first().unwrap().content, "Keep approval policy");
    assert_eq!(view.last().unwrap().content, "Continue the current task");
    assert!(view[1].content.contains("omitted"));
    assert_eq!(messages[1].content.len(), original);
}

#[test]
fn large_tool_result_is_bounded_without_removing_its_pair() {
    let budget = ContextBudget::new(&config(2));
    let messages = vec![
        Message::user("Study this project"),
        Message::assistant_with_tool_calls("", vec![ToolCall::new("call-1", "files_list", "{}")]),
        Message::tool_result(
            "call-1",
            format!("{} artifact:retained-result", "文件".repeat(12000)),
        ),
    ];
    let view = budget.fit(messages.clone(), &[], 512).unwrap().0;
    assert_eq!(view.len(), 3);
    assert_eq!(view[1].tool_calls[0].id, "call-1");
    assert_eq!(view[2].tool_call_id.as_deref(), Some("call-1"));
    assert!(view[2].content.contains("artifact:retained-result"));
    assert!(messages[2].content.len() > 60000);
}

#[test]
fn completed_rounds_with_large_arguments_fit_without_changing_durable_history() {
    let budget = ContextBudget::new(&config(2));
    let mut messages = vec![
        Message::system("Keep approval policy"),
        Message::user("Study this project"),
    ];
    for index in 0..8 {
        let id = format!("call-{index}");
        messages.push(Message::assistant_with_tool_calls(
            "",
            vec![ToolCall::new(&id, "files_list", noise(500))],
        ));
        messages.push(Message::tool_result(id, noise(800)));
    }
    let original = serde_json::to_value(&messages).unwrap();
    let view = budget.fit(messages.clone(), &[], 512).unwrap().0;
    assert_eq!(view[0].content, messages[0].content);
    assert_eq!(view[1].content, messages[1].content);
    assert!(view.iter().any(|m| {
        m.content
            .contains(crate::prompts::compaction_tool_notes_header())
    }));
    assert_eq!(serde_json::to_value(&messages).unwrap(), original);
}

#[test]
fn condensing_the_final_tool_round_keeps_its_observations_as_input() {
    let budget = ContextBudget::new(&config(2));
    let messages = vec![
        Message::user("Read the project and continue working"),
        Message::assistant_with_tool_calls(
            "Reading the project.",
            vec![ToolCall::new("read", "file_read", noise(6000))],
        ),
        Message::tool_result("read", "Observed project contents\n"),
    ];
    let (view, costs) = budget.fit(messages.clone(), &[], 512).unwrap();
    assert_eq!(view.len(), 2, "the completed round must be condensed");
    assert_eq!(view[0], messages[0]);
    let observation = &view[1];
    assert_eq!(observation.role, Role::User);
    assert!(observation.content.contains("Observed project contents"));
    assert!(observation.tool_calls.is_empty());
    assert!(observation.tool_call_id.is_none());
    assert_eq!(costs, budget.costs(&view));
    assert_eq!(messages[2].content, "Observed project contents\n");
}

#[test]
fn unfinished_tool_round_is_never_discarded_to_fit() {
    let budget = ContextBudget::new(&config(2));
    let call = Message::assistant_with_tool_calls(
        "",
        vec![ToolCall::new("pending", "files_list", noise(5000))],
    );
    assert!(matches!(
        budget.fit(vec![Message::user("Keep working"), call], &[], 512),
        Err(ModelError::ContextOverflow(_))
    ));
}

#[tokio::test]
async fn streaming_lease_is_retained_until_drop_then_released() {
    let directory = tempfile::tempdir().unwrap();
    let mut local = config(2);
    local.endpoint = "http://127.0.0.1:8080/v1".into();
    let (inner, _) = fixture("fixture", true);
    let guard = GuardedProvider::new(
        inner,
        &local,
        directory.path().join("local-inference.lock"),
        true,
    );
    let request = || ModelRequest {
        messages: vec![Message::user("small prompt")],
        tools: Vec::new(),
        max_tokens: 512,
        reasoning: crate::model::Reasoning::Configured,
    };
    let mut stream = guard.stream(request(), CancellationToken::new());
    assert!(stream.next().await.unwrap().is_ok());
    let competing = OpenOptions::new()
        .read(true)
        .write(true)
        .open(directory.path().join("local-inference.lock"))
        .unwrap();
    assert!(fs2::FileExt::try_lock_exclusive(&competing).is_err());
    drop(stream);
    assert!(fs2::FileExt::try_lock_exclusive(&competing).is_ok());
    fs2::FileExt::unlock(&competing).unwrap();
}

#[tokio::test]
async fn cancelled_waiting_generation_never_enters_provider() {
    let directory = tempfile::tempdir().unwrap();
    let mut local = config(2);
    local.endpoint = "http://127.0.0.1:8080/v1".into();
    let (inner, calls) = fixture("fixture", false);
    let guard = GuardedProvider::new(
        inner,
        &local,
        directory.path().join("local-inference.lock"),
        true,
    );
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.path().join("local-inference.lock"))
        .unwrap();
    fs2::FileExt::lock_exclusive(&lock).unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let mut stream = guard.stream(
        ModelRequest {
            messages: vec![Message::user("small")],
            tools: Vec::new(),
            max_tokens: 512,
            reasoning: crate::model::Reasoning::Configured,
        },
        cancel,
    );
    assert_eq!(stream.next().await, Some(Err(ModelError::Interrupted)));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn remote_provider_is_not_serialized_by_local_lock() {
    let directory = tempfile::tempdir().unwrap();
    let (inner, _) = fixture("fixture", false);
    let mut remote = config(2);
    remote.endpoint = "https://api.example.com/v1".into();
    let guard = GuardedProvider::new(
        inner,
        &remote,
        directory.path().join("local-inference.lock"),
        true,
    );
    guard
        .complete(
            ModelRequest {
                messages: vec![Message::user("small")],
                tools: Vec::new(),
                max_tokens: 512,
                reasoning: crate::model::Reasoning::Configured,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!directory.path().join("local-inference.lock").exists());
}

#[tokio::test]
async fn cancelling_a_polled_open_stream_returns_typed_interruption_every_time() {
    let directory = tempfile::tempdir().unwrap();
    for _ in 0..32 {
        let cancel = CancellationToken::new();
        let (inner, _) = fixture("partial", true);
        let guard = GuardedProvider::new(inner, &config(30), directory.path().join("lease"), false);
        let mut stream = guard.stream(
            ModelRequest {
                messages: vec![Message::user("task")],
                tools: Vec::new(),
                max_tokens: 512,
                reasoning: crate::model::Reasoning::Configured,
            },
            cancel.clone(),
        );
        assert!(stream.next().await.unwrap().is_ok());
        cancel.cancel();
        assert_eq!(stream.next().await, Some(Err(ModelError::Interrupted)));
        assert!(stream.next().await.is_none());
    }
}

struct Pulses;
impl ModelProvider for Pulses {
    fn stream(&self, _: ModelRequest, _: CancellationToken) -> EventStream {
        Box::pin(async_stream::stream! {
            for _ in 0..5 {
                tokio::time::sleep(Duration::from_millis(15)).await;
                yield Ok(StreamEvent::Text(String::new()));
            }
            std::future::pending::<()>().await;
        })
    }
}

#[tokio::test]
async fn active_stream_can_outlive_response_deadline_but_idle_stream_times_out() {
    let directory = tempfile::tempdir().unwrap();
    let mut guard = GuardedProvider::new(
        Arc::new(Pulses),
        &ProviderConfig::default(),
        directory.path().join("lock"),
        false,
    );
    guard.timeout = Duration::from_millis(60);
    let mut stream = guard.stream(
        ModelRequest {
            messages: vec![Message::user("small")],
            tools: Vec::new(),
            max_tokens: 32,
            reasoning: crate::model::Reasoning::Configured,
        },
        CancellationToken::new(),
    );
    for _ in 0..5 {
        assert!(stream.next().await.unwrap().is_ok());
    }
    let error = stream.next().await.unwrap().unwrap_err();
    assert!(error.to_string().contains("idle deadline"));
    assert!(error.retryable());
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn first_response_timeout_is_distinct_from_stream_idle_timeout() {
    let directory = tempfile::tempdir().unwrap();
    let mut guard = GuardedProvider::new(
        Arc::new(Pulses),
        &ProviderConfig::default(),
        directory.path().join("lock"),
        false,
    );
    guard.timeout = Duration::from_millis(5);
    let mut stream = guard.stream(
        ModelRequest {
            messages: vec![Message::user("small")],
            tools: Vec::new(),
            max_tokens: 32,
            reasoning: crate::model::Reasoning::Configured,
        },
        CancellationToken::new(),
    );
    assert!(
        stream
            .next()
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("first response deadline")
    );
}

#[test]
fn resource_faults_remain_distinct_from_cancellation() {
    assert!(
        matches!(admission_error(anyhow::anyhow!("resource monitor I/O failed")),ModelError::Io(message) if message.contains("I/O failed"))
    );
    assert_eq!(
        admission_error(xylo_resources::AdmissionCancelled.into()),
        ModelError::Interrupted
    );
}

#[test]
fn only_local_or_managed_endpoints_reserve_local_memory() {
    let directory = tempfile::tempdir().unwrap();
    let (inner, _) = fixture("fixture", false);
    let guard = |endpoint: &str, managed: bool| {
        let mut config = config(2);
        config.endpoint = endpoint.into();
        GuardedProvider::new(
            inner.clone(),
            &config,
            directory.path().join("lock"),
            managed,
        )
    };
    assert!(
        guard("https://api.example.com/v1", false)
            .resources
            .is_none()
    );
    assert!(guard("http://127.0.0.1:8080/v1", false).resources.is_some());
    assert!(
        guard("https://models.example.com/v1", true)
            .resources
            .is_some()
    );
}

#[tokio::test]
async fn summaries_carry_a_harness_record_that_survives_the_next_compaction() {
    let budget = ContextBudget::new(&config(5));
    let (summarizer, _) = fixture(
        "Work done: refactored the parser and ran the tests; next, update the docs.",
        false,
    );
    let system = vec![Message::system("Policy")];
    let mut history = vec![Message::user(
        "Refactor the parser.\nNever touch \"vendor/\".",
    )];
    history.push(Message::assistant_with_tool_calls(
        "",
        vec![
            ToolCall::new("read-1", "file_read", r#"{"path":"src/parser.rs"}"#),
            ToolCall::new("edit-1", "file_edit", r#"{"path":"src/parser.rs"}"#),
            ToolCall::new("test-1", "shell_command", r#"{"command":"cargo test"}"#),
        ],
    ));
    history.push(Message::tool_result("read-1", noise(200)));
    history.push(Message::tool_result("edit-1", r#"{"status":"success"}"#));
    history.push(Message::tool_result(
        "test-1",
        r#"{"exit_code":101,"stdout":"1 failed"}"#,
    ));
    history.push(Message::user("Also keep the public API stable."));
    for index in 0..12 {
        round(&mut history, index, 300);
    }
    let plan = budget
        .summary_plan(&system, &history, &[], 512, Pressure::Normal)
        .unwrap();
    let condensed = budget
        .summarize(
            summarizer.as_ref(),
            &history,
            &plan,
            Duration::from_secs(5),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let summary = &condensed[0].content;
    let record = Record::parse(summary).unwrap();
    assert_eq!(
        record.user_messages,
        vec!["Refactor the parser.\nNever touch \"vendor/\".".to_owned()]
    );
    assert_eq!(record.files_read, vec!["src/parser.rs".to_owned()]);
    assert_eq!(record.files_changed, vec!["src/parser.rs".to_owned()]);
    assert_eq!(record.commands, vec!["cargo test → exit 101".to_owned()]);
    // The current request stays a message of its own and is not duplicated.
    assert_eq!(condensed[1].content, "Also keep the public API stable.");
    // A later compaction updates the summary text only; the record carries
    // forward and grows deterministically.
    let mut later = condensed.clone();
    later.push(Message::user("Now update the docs."));
    for index in 20..34 {
        round(&mut later, index, 300);
    }
    let plan = budget
        .summary_plan(&system, &later, &[], 512, Pressure::Normal)
        .unwrap();
    let (updater, _) = fixture(
        "Work done: parser refactored, docs being updated; next, review the docs.",
        false,
    );
    let again = budget
        .summarize(
            updater.as_ref(),
            &later,
            &plan,
            Duration::from_secs(5),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let record = Record::parse(&again[0].content).unwrap();
    assert_eq!(
        record.user_messages[0],
        "Refactor the parser.\nNever touch \"vendor/\"."
    );
    assert!(
        record
            .user_messages
            .iter()
            .any(|m| m == "Also keep the public API stable.")
    );
    assert_eq!(record.commands, vec!["cargo test → exit 101".to_owned()]);
    assert_eq!(again[0].content.matches("<harness_record>").count(), 1);
}

#[test]
fn an_overflow_summarises_to_a_lower_target() {
    // Only large windows lower the target; small ones rely on the raised
    // calibration.
    let mut large = config(5);
    large.context_tokens = 32_768;
    let budget = ContextBudget::new(&large);
    let system = vec![Message::system("Policy")];
    let mut history = vec![Message::user("Refactor the parser")];
    for index in 0..240 {
        round(&mut history, index, 40);
    }
    let normal = budget
        .summary_plan(&system, &history, &[], 512, Pressure::Normal)
        .unwrap();
    let overflow = budget
        .summary_plan(&system, &history, &[], 512, Pressure::Overflow)
        .unwrap();
    assert!(overflow.cut > normal.cut, "{overflow:?} vs {normal:?}");
}

#[test]
fn provider_usage_calibrates_estimates_and_an_overflow_raises_them() {
    let budget = ContextBudget::new(&config(5));
    let messages = vec![Message::system("Policy"), Message::user(noise(1500))];
    let estimate = budget.usage(&messages, &[], 0).used();
    // The provider counted twice as many tokens: estimates follow at once.
    budget.observe(&messages, &[], estimate * 2);
    let raised = budget.usage(&messages, &[], 0).used();
    assert!(raised >= estimate * 19 / 10, "{raised} vs {estimate}");
    // Overcounts are corrected slowly, never below the floor.
    budget.observe(&messages, &[], raised / 4);
    let lowered = budget.usage(&messages, &[], 0).used();
    assert!(lowered < raised && lowered > raised / 2);
    // Tiny reports (fixtures, cached prefixes) are ignored.
    budget.observe(&messages, &[], 10);
    assert_eq!(budget.usage(&messages, &[], 0).used(), lowered);
    // Helper agents share the calibration; an overflow raises it.
    let child = budget.clone();
    child.penalize_overflow();
    assert!(budget.usage(&messages, &[], 0).used() > lowered);
}

#[test]
fn the_budget_fits_the_served_window() {
    let mut served = config(5);
    served.context_tokens = 32_768;
    served.served_context_tokens = Some(8192);
    let budget = ContextBudget::new(&served);
    let usage = budget.usage(&[Message::user("hi")], &[], 512);
    assert_eq!(usage.capacity, 8192);
    // A small window summarises one summary's size below the hard limit.
    let summary = (budget.window().tokens() / 8).min(2048);
    assert_eq!(usage.autocompact_threshold, u64::from(8192 - summary));
}

#[test]
fn calibration_never_learns_to_undercount_and_ignores_reasoning() {
    let budget = ContextBudget::new(&config(5));
    let messages = vec![Message::system("Policy"), Message::user(noise(1500))];
    let baseline = budget.usage(&messages, &[], 0).used();
    for _ in 0..40 {
        budget.observe(&messages, &[], 1100);
    }
    let floored = budget.usage(&messages, &[], 0).used();
    assert!(floored >= baseline * 84 / 100, "{floored} vs {baseline}");
    // Reasoning the provider does not count is not compared against usage.
    let fresh = ContextBudget::new(&config(5));
    let mut with_reasoning = messages.clone();
    let mut answer = Message::assistant("ok");
    answer.reasoning = Some(noise(3000));
    with_reasoning.push(answer);
    let text_only = fresh.usage(&messages, &[], 0).used();
    fresh.observe(&with_reasoning, &[], text_only + 64);
    assert!(fresh.usage(&messages, &[], 0).used() >= text_only);
}

/// A local server serving 4K of a configured 32K window, with the default
/// output limit: the tiny-window profile.
fn tiny_config() -> ProviderConfig {
    ProviderConfig {
        model: "unrecognized-custom-model".into(),
        context_tokens: 32_768,
        served_context_tokens: Some(4096),
        output_tokens: 2048,
        timeout_seconds: 5,
        ..Default::default()
    }
}

/// Five schemas about the size of the tiny window's compact resident tools.
fn tiny_tools() -> Vec<ToolSchema> {
    ["file_read", "file_edit", "shell_command", "search_tools", "call_tool"]
        .into_iter()
        .map(|name| ToolSchema {
            name: name.into(),
            description: format!("{name}: {}", noise(14)),
            parameters: serde_json::json!({"type":"object","properties":{"path":{"type":"string"},"text":{"type":"string"}},"required":["path"],"additionalProperties":false}),
        })
        .collect()
}

/// A file_read round whose result is as large as a tiny window lets a
/// result be.
fn tiny_round(messages: &mut Vec<Message>, index: usize, budget: &ContextBudget) {
    let id = format!("call-{index}");
    messages.push(Message::assistant_with_tool_calls(
        "",
        vec![ToolCall::new(
            &id,
            "file_read",
            format!(r#"{{"path":"src/file{index}.rs"}}"#),
        )],
    ));
    let mut result = noise(400);
    result.truncate(budget.window().tool_result_bytes() - 64);
    messages.push(Message::tool_result(id, result));
}

/// Records the summary request it answers.
struct Capture {
    requests: Arc<parking_lot::Mutex<Vec<ModelRequest>>>,
}

impl ModelProvider for Capture {
    fn stream(&self, request: ModelRequest, _: CancellationToken) -> EventStream {
        self.requests.lock().push(request);
        Box::pin(async_stream::stream! {
            yield Ok(StreamEvent::Text("Requirements: fix the parser. Work done: read src/file0.rs [call-0]. Next: run the tests.".into()));
            yield Ok(StreamEvent::Finished(FinishReason::Stop));
        })
    }
}

/// In a 4K window with the compact prompt and tools, masking frees older
/// results, a summary is planned below its threshold, its request fits the
/// window with the tiny summary prompt and output limit, and the condensed
/// history leaves room for the next steps.
#[tokio::test]
async fn a_tiny_window_masks_and_summarises_within_the_window() {
    let budget = ContextBudget::new(&tiny_config());
    let window = budget.window();
    assert!(window.tiny());
    let tools = tiny_tools();
    let max_tokens = 2048;
    // About the estimated size of the compact system prompt (500 tokens).
    let system = vec![Message::system(noise(190))];
    let mut history = vec![Message::user("Fix the parser in src/")];
    for index in 0..3 {
        tiny_round(&mut history, index, &budget);
    }
    // Masking: the older reads go, the newest round stays.
    let plan = budget
        .mask_plan(&system, &history, &tools, max_tokens, Pressure::Normal)
        .expect("masking frees older results");
    let newest = history.len() - 1;
    assert!(!plan.stubs.is_empty());
    assert!(plan.stubs.iter().all(|(index, _)| *index != newest));
    let before = budget.costs(&history).iter().sum::<usize>();
    plan.apply(&mut history);
    assert!(budget.costs(&history).iter().sum::<usize>() < before);
    // Summary: once the threshold passes, a plan exists under both pressures.
    let mut index = 3;
    loop {
        let mut view = system.clone();
        view.extend_from_slice(&history);
        if budget.over_threshold(&view, &tools, max_tokens) {
            break;
        }
        tiny_round(&mut history, index, &budget);
        index += 1;
    }
    for _ in 0..2 {
        tiny_round(&mut history, index, &budget);
        index += 1;
    }
    assert!(
        budget
            .summary_plan(&system, &history, &tools, max_tokens, Pressure::Overflow)
            .is_some()
    );
    let plan = budget
        .summary_plan(&system, &history, &tools, max_tokens, Pressure::Normal)
        .expect("a tiny window can summarise");
    let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let capture = Capture {
        requests: requests.clone(),
    };
    let condensed = budget
        .summarize(
            &capture,
            &history,
            &plan,
            Duration::from_secs(5),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let request = requests.lock().pop().unwrap();
    assert_eq!(request.max_tokens, window.tokens() / 8);
    assert!(request.messages[0].content.contains("Stay under"));
    let sent = budget.usage(&request.messages, &[], request.max_tokens);
    assert!(
        sent.used() + u64::from(request.max_tokens) <= u64::from(window.context()),
        "{sent:?}"
    );
    // The summary keeps the record and the newest round, and the condensed
    // request fits the hard limit with room to spare.
    assert!(Record::parse(&condensed[0].content).is_some());
    assert_eq!(condensed.last(), history.last());
    let mut view = system.clone();
    view.extend_from_slice(&condensed);
    let (_, costs) = budget.fit(view.clone(), &tools, max_tokens).unwrap();
    let used = costs.iter().sum::<usize>();
    let hard = budget.allowance(&tools, max_tokens, window.context());
    assert!(used < hard, "{used} of {hard}");
}

/// `count` tool schemas whose description has `words` words.
fn schemas(count: usize, words: usize) -> Vec<ToolSchema> {
    (0..count)
        .map(|index| ToolSchema {
            name: format!("tool_{index}"),
            description: noise(words),
            parameters: serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"Workspace-relative or absolute path"},"limit":{"type":"integer","minimum":1}},"required":["path"],"additionalProperties":false}),
        })
        .collect()
}

/// A tool round whose result is `bytes` long.
fn sized_round(messages: &mut Vec<Message>, index: usize, bytes: usize) {
    let id = format!("call-{index}");
    messages.push(Message::assistant_with_tool_calls(
        "",
        vec![ToolCall::new(
            &id,
            "file_read",
            format!(r#"{{"path":"src/file{index}.rs"}}"#),
        )],
    ));
    let mut result = noise(bytes / 3);
    result.truncate(bytes);
    messages.push(Message::tool_result(id, result));
}

/// Across windows from 4K to 128K, with each profile's preamble at its
/// measured size (tiny: about 500 tokens of system prompt and 760 of tools;
/// small: 1,350 and 1,790; large: 1,350 and about 4,240), the summary is an
/// eighth of `W_eff` (at most 2,048 tokens), at least 15 % of `W_eff` of
/// history fits before the threshold, a summary plan exists under both
/// pressures, and the summary request fits the window.
#[tokio::test]
async fn every_window_from_4k_to_128k_can_summarise() {
    for (context, output) in [
        (4096, 1024),
        (4096, 2048),
        (4096, 4096),
        (6144, 1024),
        (6144, 2048),
        (6144, 4096),
        (8192, 1024),
        (8192, 2048),
        (8192, 4096),
        (16_384, 2048),
        (32_768, 4096),
        (131_072, 8192),
    ] {
        let budget = ContextBudget::new(&ProviderConfig {
            model: "unrecognized-custom-model".into(),
            context_tokens: context,
            output_tokens: output,
            timeout_seconds: 5,
            ..Default::default()
        });
        let window = budget.window();
        let label = format!("{context}/{output}");
        let (system, tools) = if window.tiny() {
            (noise(190), schemas(5, 9))
        } else if window.small() {
            (noise(412), schemas(8, 31))
        } else {
            (noise(412), schemas(19, 31))
        };
        let system = vec![Message::system(system)];
        let summary = budget.summary_tokens();
        assert_eq!(summary, (window.tokens() / 8).clamp(192, 2048), "{label}");
        let room = budget
            .allowance(&tools, output, budget.proactive_context())
            .saturating_sub(budget.costs(&system)[0]);
        assert!(
            room * 100 >= window.tokens() as usize * 15,
            "{label}: {room} tokens of history before the threshold, W_eff {}",
            window.tokens()
        );
        let mut history = vec![Message::user("Fix the parser in src/")];
        let mut index = 0;
        loop {
            let mut view = system.clone();
            view.extend_from_slice(&history);
            if budget.over_threshold(&view, &tools, output) {
                break;
            }
            sized_round(&mut history, index, window.tool_result_bytes() / 2);
            index += 1;
        }
        for pressure in [Pressure::Overflow, Pressure::Normal] {
            assert!(
                budget
                    .summary_plan(&system, &history, &tools, output, pressure)
                    .is_some(),
                "{label}: no {pressure:?} summary plan; system {}, tools {}, summary {summary}, record {}, allowance {}, history {:?}",
                budget.costs(&system)[0],
                budget.tool_tokens(&tools),
                budget.record_tokens(),
                budget.allowance(&tools, output, budget.target_context(pressure)),
                budget.costs(&history)
            );
        }
        let plan = budget
            .summary_plan(&system, &history, &tools, output, Pressure::Normal)
            .unwrap();
        let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let capture = Capture {
            requests: requests.clone(),
        };
        let condensed = budget
            .summarize(
                &capture,
                &history,
                &plan,
                Duration::from_secs(5),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let request = requests.lock().pop().unwrap();
        assert_eq!(request.max_tokens, summary.min(window.output()), "{label}");
        let sent = budget.usage(&request.messages, &[], request.max_tokens);
        assert!(
            sent.used() + u64::from(request.max_tokens) <= u64::from(window.context()),
            "{label}: summary request {sent:?}"
        );
        let mut view = system.clone();
        view.extend_from_slice(&condensed);
        assert!(budget.fit(view, &tools, output).is_ok(), "{label}");
    }
}

/// Windows of 32K and more summarise exactly as before the summary size
/// scaled with the window: 2,048-token summaries, the 900-word prompt, 80 %
/// and 60 % thresholds and the same record budget.
#[test]
fn large_windows_keep_their_summary_sizes() {
    for (context, output) in [(32_768, 4096), (131_072, 8192), (131_072, 32_768)] {
        let budget = ContextBudget::new(&ProviderConfig {
            model: "unrecognized-custom-model".into(),
            context_tokens: context,
            output_tokens: output,
            ..Default::default()
        });
        let margin = (context / 32).clamp(128, 4096);
        assert_eq!(budget.window().tokens(), context - output - margin);
        assert_eq!(budget.summary_tokens(), 2048);
        assert_eq!(budget.proactive_context(), context / 100 * 80);
        assert_eq!(budget.target_context(Pressure::Normal), context / 100 * 80);
        assert_eq!(
            budget.target_context(Pressure::Overflow),
            context / 100 * 60
        );
        assert_eq!(
            budget.record_tokens(),
            (context - output - margin) as usize / 8
        );
        assert_eq!(budget.min_condensed_tokens(), 4096);
        let prompt = budget.compaction_prompt();
        assert_eq!(
            prompt,
            include_str!("../../prompts/compaction.md")
                .trim_end()
                .replace("{{words}}", "900")
        );
    }
}
