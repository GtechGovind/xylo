use super::*;
use crate::{
    runner::Trial,
    task::{Check, CheckStatus},
    trace::{Exchange, ModelReply, RecordedRequest, TRACE_VERSION, Trace},
};
use serde_json::json;
use xylo_engine::Message;

fn close(actual: Option<f64>, expected: f64) -> bool {
    actual.is_some_and(|actual| (actual - expected).abs() < 1e-9)
}

#[test]
fn pass_hat_k_is_the_unbiased_all_succeed_estimate() {
    assert!(close(pass_hat_k(5, 5, 5), 1.0));
    assert!(close(pass_hat_k(5, 4, 1), 0.8));
    assert!(close(pass_hat_k(5, 4, 2), 0.6));
    assert!(close(pass_hat_k(5, 1, 2), 0.0));
    assert_eq!(pass_hat_k(5, 5, 6), None);
    assert_eq!(pass_hat_k(5, 5, 0), None);
}

#[test]
fn pass_at_k_is_the_unbiased_any_succeeds_estimate() {
    assert!(close(pass_at_k(5, 1, 5), 1.0));
    assert!(close(pass_at_k(5, 0, 3), 0.0));
    assert!(close(pass_at_k(4, 2, 2), 1.0 - (2.0 / 4.0) * (1.0 / 3.0)));
}

fn exchange(messages: Vec<Message>, usage: serde_json::Value) -> Exchange {
    Exchange {
        kind: RequestKind::Agent,
        request: RecordedRequest {
            messages,
            tools: Vec::new(),
            tools_digest: String::new(),
            max_tokens: 1,
        },
        outcome: Outcome::Reply(ModelReply {
            usage: Some(usage),
            ..ModelReply::text("x")
        }),
    }
}

fn trial(exchanges: Vec<Exchange>) -> Trial {
    Trial::from_trace(Trace {
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
        exchanges,
        events: Vec::new(),
        actions: Vec::new(),
        checkpoint: None,
    })
}

#[test]
fn notes_and_loop_firings_are_counted_from_harness_note_events() {
    let mut stalled = trial(Vec::new());
    stalled.trace.events = vec![
        EngineEvent::HarnessNote {
            kind: NoteKind::StallNotice,
            text: "Any wording".into(),
        },
        EngineEvent::HarnessNote {
            kind: NoteKind::Continuation,
            text: "Any wording".into(),
        },
        EngineEvent::Status {
            text: "A status line is not a note".into(),
        },
        EngineEvent::HarnessNote {
            kind: NoteKind::StallWarning,
            text: "Any wording".into(),
        },
        EngineEvent::Error {
            message: "Any wording".into(),
            retryable: false,
            kind: ErrorKind::Stalled,
        },
    ];
    let metrics = TrialMetrics::of(&stalled);
    assert_eq!((metrics.harness_notes, metrics.loop_notes), (3, 2));
    assert!(metrics.loop_stopped);
}

#[test]
fn an_empty_reply_failure_is_not_a_loop_stop() {
    let mut empty = trial(Vec::new());
    empty.trace.events = vec![EngineEvent::Error {
        message: "The model returned no answer".into(),
        retryable: true,
        kind: ErrorKind::EmptyResponse,
    }];
    assert!(!TrialMetrics::of(&empty).loop_stopped);
}

#[test]
fn cache_reads_are_absent_until_a_provider_reports_them() {
    let plain = trial(vec![exchange(
        vec![],
        json!({"input_tokens": 100, "output_tokens": 10}),
    )]);
    let metrics = TrialMetrics::of(&plain);
    assert_eq!(metrics.cache_read_tokens, None);
    assert_eq!((metrics.input_tokens, metrics.output_tokens), (100, 10));
    let cached = trial(vec![exchange(
        vec![],
        json!({"input_tokens": 100, "output_tokens": 10, "cache_read_tokens": 80}),
    )]);
    assert_eq!(TrialMetrics::of(&cached).cache_read_tokens, Some(80));
}

fn result(task: &str, passed: bool, invariants_held: bool, metrics: TrialMetrics) -> TrialResult {
    TrialResult {
        task: task.into(),
        trial: 1,
        model: ModelLabel {
            label: "scripted".into(),
            model: "scripted".into(),
        },
        outcome: "completed".into(),
        task_passed: passed,
        invariants_held,
        passed: passed && invariants_held,
        checks: vec![Check::skip("origin", "absent")],
        invariants: vec![Check::expect("stable_prefix", invariants_held, "")],
        metrics,
        trace: None,
    }
}

#[test]
fn summaries_report_reliability_cost_and_violations() {
    let spend = |input, cache| TrialMetrics {
        input_tokens: input,
        output_tokens: 10,
        cache_read_tokens: cache,
        loop_notes: 1,
        ..TrialMetrics::default()
    };
    let report = Report::new(
        "builtin",
        3,
        vec![
            result("a", true, true, spend(90, Some(45))),
            result("a", true, false, spend(90, None)),
            result("a", false, true, spend(90, Some(45))),
            result("b", true, true, spend(10, None)),
        ],
    );
    assert_eq!(report.summaries.len(), 2);
    let a = &report.summaries[0];
    assert_eq!(
        (a.trials, a.successes, a.succeeded_with_violations),
        (3, 1, 1)
    );
    assert!(close(a.tokens_per_success, 300.0));
    assert!(close(a.cache_hit_rate, 90.0 / 270.0));
    assert_eq!((a.loop_notes, a.loop_stops), (3, 0));
    assert!(close(Some(a.pass[0].pass_hat_k), 1.0 / 3.0));
    assert_eq!(a.skipped["origin"], 3);
    assert_eq!(a.failures["stable_prefix"], 1);
    assert_eq!(report.summaries[1].cache_hit_rate, None);
    assert!(!report.all_passed());
    let table = report.table();
    assert!(table.contains("not reported"), "{table}");
    assert!(table.contains("FAIL a #1"), "{table}");
    assert_eq!(CheckStatus::Skip, report.results[0].checks[0].status);
}
