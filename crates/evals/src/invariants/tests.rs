use super::*;
use crate::{
    CheckStatus,
    runner::ApprovalDecision,
    trace::{Exchange, ModelLabel, ModelReply, RecordedRequest, RequestKind, TRACE_VERSION, Trace},
};
use serde_json::json;
use uuid::Uuid;
use xylo_core::{ErrorKind, Usage};
use xylo_engine::ToolCall;

fn trace(events: Vec<EngineEvent>) -> Trace {
    Trace {
        version: TRACE_VERSION,
        task: "unit".into(),
        trial: 1,
        model: ModelLabel {
            label: "scripted".into(),
            model: "scripted".into(),
        },
        provider: xylo_core::ProviderConfig::default(),
        served_window: None,
        prompt: String::new(),
        exchanges: Vec::new(),
        events,
        actions: Vec::new(),
        checkpoint: None,
    }
}

fn completed(input: u64, output: u64) -> EngineEvent {
    EngineEvent::Completed {
        text: "done".into(),
        usage: Usage {
            input_tokens: input,
            output_tokens: output,
            ..Usage::default()
        },
    }
}

fn named(trial: &Trial, name: &str) -> Check {
    check(trial)
        .into_iter()
        .find(|check| check.name == name)
        .unwrap()
}

fn agent(messages: Vec<Message>, tools_digest: &str, usage: (u64, u64)) -> Exchange {
    Exchange {
        kind: RequestKind::Agent,
        request: RecordedRequest {
            messages,
            tools: vec!["file_read".into()],
            tools_digest: tools_digest.into(),
            max_tokens: 100,
        },
        outcome: Outcome::Reply(ModelReply {
            usage: Some(json!({"input_tokens": usage.0, "output_tokens": usage.1})),
            ..ModelReply::text("x")
        }),
    }
}

fn call(id: &str) -> Message {
    Message::assistant_with_tool_calls("", vec![ToolCall::new(id, "file_read", "{}")])
}

#[test]
fn well_formed_requests_pair_every_call_with_one_result() {
    let system = Message::system("s");
    let ok = [
        system.clone(),
        Message::user("go"),
        call("a"),
        Message::tool_result("a", "r"),
        Message::assistant("done"),
    ];
    assert!(well_formed(&ok).is_ok());
    let missing = [system.clone(), call("a"), Message::user("next")];
    assert!(well_formed(&missing).unwrap_err().contains("no result"));
    let stray = [system.clone(), Message::tool_result("a", "r")];
    assert!(well_formed(&stray).is_err());
    let twice = [
        system.clone(),
        call("a"),
        Message::tool_result("a", "r"),
        Message::tool_result("a", "r"),
    ];
    assert!(well_formed(&twice).is_err());
    let reused = [
        system.clone(),
        call("a"),
        Message::tool_result("a", "r"),
        call("a"),
        Message::tool_result("a", "r"),
    ];
    assert!(well_formed(&reused).unwrap_err().contains("reused"));
    assert!(well_formed(&[Message::user("no system")]).is_err());
}

#[test]
fn a_second_terminal_event_or_late_event_breaks_the_terminal_invariant() {
    let ok = Trial::from_trace(trace(vec![
        EngineEvent::Started,
        completed(0, 0),
        EngineEvent::TitleUpdated { title: "t".into() },
    ]));
    assert_eq!(
        named(&ok, "single_terminal_event").status,
        CheckStatus::Pass
    );
    let doubled = Trial::from_trace(trace(vec![completed(0, 0), EngineEvent::Cancelled]));
    assert!(named(&doubled, "single_terminal_event").failed());
    let late = Trial::from_trace(trace(vec![
        EngineEvent::Error {
            message: "x".into(),
            retryable: false,
            kind: ErrorKind::Internal,
        },
        EngineEvent::Delta {
            text: "late".into(),
        },
    ]));
    assert!(named(&late, "single_terminal_event").failed());
}

#[test]
fn a_tool_without_a_finish_is_reported() {
    let id = Uuid::new_v4();
    let trial = Trial::from_trace(trace(vec![
        EngineEvent::ToolStarted {
            action_id: id,
            name: "file_read".into(),
            arguments: json!({}),
            parent_id: None,
        },
        completed(0, 0),
    ]));
    assert!(named(&trial, "tool_events_paired").failed());
}

#[test]
fn a_changed_tool_block_breaks_the_stable_prefix() {
    let messages = vec![Message::system("s"), Message::user("go")];
    let mut stable = trace(vec![completed(2, 2)]);
    stable.exchanges = vec![
        agent(messages.clone(), "d1", (1, 1)),
        agent(messages.clone(), "d1", (1, 1)),
    ];
    let trial = Trial::from_trace(stable.clone());
    assert_eq!(named(&trial, "stable_prefix").status, CheckStatus::Pass);
    assert_eq!(named(&trial, "usage_accounted").status, CheckStatus::Pass);
    let mut changed = stable;
    changed.exchanges[1].request.tools_digest = "d2".into();
    assert!(named(&Trial::from_trace(changed), "stable_prefix").failed());
}

#[test]
fn reported_usage_must_equal_the_sum_of_requests() {
    let mut mismatched = trace(vec![completed(5, 2)]);
    mismatched.exchanges = vec![agent(vec![Message::system("s")], "d", (1, 1))];
    assert!(named(&Trial::from_trace(mismatched), "usage_accounted").failed());
}

#[test]
fn an_overfull_request_breaks_the_window_invariant() {
    let usage = xylo_core::ContextUsage {
        capacity: 1000,
        output_reserve: 200,
        messages: 900,
        ..Default::default()
    };
    let trial = Trial::from_trace(trace(vec![
        EngineEvent::ContextUsage { usage },
        completed(0, 0),
    ]));
    assert!(named(&trial, "requests_fit_window").failed());
}

#[test]
fn a_denied_action_that_ran_breaks_the_denial_invariant() {
    let id = Uuid::new_v4();
    let events = vec![
        EngineEvent::ToolStarted {
            action_id: id,
            name: "shell_command".into(),
            arguments: json!({}),
            parent_id: None,
        },
        EngineEvent::ApprovalRequired {
            action_id: id,
            name: "shell_command".into(),
            arguments: json!({}),
            rule: None,
            rule_key: None,
            reason: None,
            parent_id: None,
        },
        EngineEvent::ToolFinished {
            action_id: id,
            name: "shell_command".into(),
            result: "ran".into(),
            success: true,
            parent_id: None,
        },
        completed(0, 0),
    ];
    let mut trial = Trial::from_trace(trace(events));
    trial.approvals = vec![ApprovalDecision {
        tool: "shell_command".into(),
        arguments: json!({}),
        allowed: false,
    }];
    assert!(named(&trial, "denials_respected").failed());
    trial.approvals[0].allowed = true;
    assert_eq!(named(&trial, "denials_respected").status, CheckStatus::Skip);
}

fn child_exchange(system: &str, task: &str) -> Exchange {
    let mut exchange = agent(
        vec![Message::system(system), Message::user(task)],
        "d1",
        (1, 1),
    );
    exchange.kind = RequestKind::Child;
    exchange
}

#[test]
fn children_keep_their_own_stable_prefix() {
    let root = vec![Message::system("root"), Message::user("go")];
    let mut stable = trace(vec![completed(2, 2)]);
    stable.exchanges = vec![
        agent(root.clone(), "d1", (1, 1)),
        child_exchange("helper for a", "a"),
        child_exchange("helper for b", "b"),
        child_exchange("helper for a", "a"),
        agent(root, "d1", (1, 1)),
    ];
    assert_eq!(
        named(&Trial::from_trace(stable.clone()), "stable_prefix").status,
        CheckStatus::Pass
    );
    let mut drifting = stable;
    drifting.exchanges[3] = child_exchange("helper for a, changed", "a");
    let check = named(&Trial::from_trace(drifting), "stable_prefix");
    assert!(check.failed());
    assert!(check.detail.contains("child"), "{}", check.detail);
}

#[test]
fn a_hung_cancellation_breaks_its_invariant() {
    let mut trial = Trial::from_trace(trace(vec![EngineEvent::Cancelled]));
    assert_eq!(
        named(&trial, "cancellation_honoured").status,
        CheckStatus::Skip
    );
    trial.timed_out = true;
    assert_eq!(
        named(&trial, "cancellation_honoured").status,
        CheckStatus::Pass
    );
    trial.hung = true;
    assert!(named(&trial, "cancellation_honoured").failed());
}
