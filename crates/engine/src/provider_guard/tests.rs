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
    let budget = ContextBudget::new(&config(5));
    let system = vec![Message::system("Policy")];
    let mut history = vec![Message::user("Refactor the parser")];
    for index in 0..60 {
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
    // Small windows summarise at 70 % rather than 80 %.
    assert_eq!(usage.autocompact_threshold, 8192 / 100 * 70);
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
