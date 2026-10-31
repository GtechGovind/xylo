use super::*;
use crate::trace::{
    Exchange, ModelLabel, ModelReply, RecordedError, RecordedRequest, TRACE_VERSION,
};

fn exchange(kind: RequestKind, messages: Vec<Message>, outcome: Outcome) -> Exchange {
    Exchange {
        kind,
        request: RecordedRequest {
            messages,
            tools: Vec::new(),
            tools_digest: String::new(),
            max_tokens: 1,
        },
        outcome,
    }
}

fn trace(exchanges: Vec<Exchange>) -> Trace {
    Trace {
        version: TRACE_VERSION,
        task: "tool-loop".into(),
        trial: 1,
        model: ModelLabel {
            label: "live".into(),
            model: "m".into(),
        },
        provider: xylo_core::ProviderConfig::default(),
        served_window: None,
        prompt: String::new(),
        exchanges,
        events: Vec::new(),
        actions: Vec::new(),
        checkpoint: None,
    }
}

#[test]
fn the_replay_script_keeps_each_kind_in_its_own_queue() {
    let recorded = trace(vec![
        exchange(
            RequestKind::Agent,
            vec![],
            Outcome::Error(RecordedError::context_overflow("too long")),
        ),
        exchange(
            RequestKind::Summary,
            vec![],
            Outcome::Reply(ModelReply::text("summary")),
        ),
        exchange(
            RequestKind::Agent,
            vec![],
            Outcome::Reply(ModelReply::text("answer")),
        ),
        exchange(
            RequestKind::Title,
            vec![],
            Outcome::Reply(ModelReply::text("Title")),
        ),
        exchange(RequestKind::Agent, vec![], Outcome::Abandoned),
    ]);
    let script = script(&recorded);
    assert_eq!(
        script.steps,
        vec![
            Step::Fail(RecordedError::context_overflow("too long")),
            Step::Reply(ModelReply::text("answer")),
        ]
    );
    assert_eq!(script.summaries, ["summary"]);
    assert_eq!(script.titles, ["Title"]);
}

#[test]
fn comparison_ignores_the_system_prompt_and_finds_the_first_difference() {
    let request = |system: &str, user: &str| {
        exchange(
            RequestKind::Agent,
            vec![Message::system(system), Message::user(user)],
            Outcome::Reply(ModelReply::text("x")),
        )
    };
    let recorded = trace(vec![request("monday", "go"), request("monday", "more")]);
    let same = trace(vec![request("tuesday", "go"), request("tuesday", "more")]);
    assert_eq!(compare(&recorded, &same).status, crate::CheckStatus::Pass);
    let different = trace(vec![request("monday", "go"), request("monday", "less")]);
    let check = compare(&recorded, &different);
    assert!(check.failed());
    assert!(check.detail.contains("agent request 2"), "{}", check.detail);
    let shorter = trace(vec![request("monday", "go")]);
    assert!(compare(&recorded, &shorter).failed());
}

#[test]
fn volatile_values_become_numbered_placeholders_in_order_of_appearance() {
    let mut normalizer = Normalizer::default();
    let text = normalizer.apply(
        "id 0b5e6f9c-3c1a-4c6e-9d6b-1f2a3b4c5d6e at 1790000000123, again \
         0b5e6f9c-3c1a-4c6e-9d6b-1f2a3b4c5d6e, other aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee \
         on 2026-10-31T08:15:02.5Z; line 42 stays",
    );
    assert_eq!(
        text,
        "id <uuid-1> at <time-1>, again <uuid-1>, other <uuid-2> on <time-2>; line 42 stays"
    );
}

fn with_tool_result(content: &str) -> Trace {
    trace(vec![exchange(
        RequestKind::Agent,
        vec![
            Message::system("s"),
            Message::user("go"),
            Message::tool_result("c1", content),
        ],
        Outcome::Reply(ModelReply::text("x")),
    )])
}

#[test]
fn runs_that_differ_only_in_fresh_ids_and_times_match() {
    let recorded = with_tool_result(
        r#"{"id":"11111111-2222-4333-8444-555555555555","created_at":1790000000123}"#,
    );
    let replayed = with_tool_result(
        r#"{"id":"99999999-8888-4777-8666-555555555555","created_at":1790000999999}"#,
    );
    assert_eq!(
        compare(&recorded, &replayed).status,
        crate::CheckStatus::Pass
    );
    let reused = with_tool_result(
        r#"{"id":"99999999-8888-4777-8666-555555555555","created_at":1790000999999,"copy":"99999999-8888-4777-8666-555555555555"}"#,
    );
    let distinct = with_tool_result(
        r#"{"id":"11111111-2222-4333-8444-555555555555","created_at":1790000000123,"copy":"22222222-2222-4333-8444-555555555555"}"#,
    );
    assert!(compare(&distinct, &reused).failed());
}

#[test]
fn a_served_window_replays_its_rejections_by_itself() {
    let mut recorded = trace(vec![
        exchange(
            RequestKind::Agent,
            vec![],
            Outcome::Error(RecordedError::context_overflow("too long")),
        ),
        exchange(
            RequestKind::Agent,
            vec![],
            Outcome::Reply(ModelReply::text("answer")),
        ),
    ]);
    assert_eq!(script(&recorded).steps.len(), 2);
    recorded.served_window = Some(9000);
    let windowed = script(&recorded);
    assert_eq!(windowed.served_window, Some(9000));
    assert_eq!(
        windowed.steps,
        vec![Step::Reply(ModelReply::text("answer"))]
    );
}

#[test]
fn child_replies_are_grouped_by_task() {
    let child = |task: &str, text: &str| {
        exchange(
            RequestKind::Child,
            vec![Message::system("helper"), Message::user(task)],
            Outcome::Reply(ModelReply::text(text)),
        )
    };
    let recorded = trace(vec![
        child("task a", "a1"),
        child("task b", "b1"),
        child("task a", "a2"),
    ]);
    let script = script(&recorded);
    assert!(script.steps.is_empty());
    assert_eq!(script.children.len(), 2);
    assert_eq!(script.children[0].task, "task a");
    assert_eq!(
        script.children[0].steps,
        vec![
            Step::Reply(ModelReply::text("a1")),
            Step::Reply(ModelReply::text("a2"))
        ]
    );
}
