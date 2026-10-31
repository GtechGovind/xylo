//! The built-in suite against the real engine, offline.

use super::*;
use crate::{
    RunOptions, TrialResult, replay, run_suite,
    runner::{ModelSource, run_trial},
    trace::Trace,
};
use std::time::Duration;

const DEADLINE: Duration = Duration::from_secs(120);

async fn graded(task: &Task) -> (Trial, TrialResult) {
    let trial = run_trial(task, &ModelSource::Scripted, 1, DEADLINE)
        .await
        .unwrap();
    let result = TrialResult::grade(task, &trial);
    (trial, result)
}

fn explain(result: &TrialResult) -> String {
    serde_json::to_string_pretty(&(&result.outcome, &result.checks, &result.invariants)).unwrap()
}

#[test]
fn task_ids_are_unique_and_findable() {
    let tasks = builtin();
    let mut ids: Vec<_> = tasks.iter().map(|task| task.id).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), tasks.len());
    assert!(tasks.iter().all(|task| find(task.id).is_some()));
    assert!(find("no-such-task").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_builtin_task_passes_offline() {
    let mut failures = Vec::new();
    for task in builtin() {
        let (_, result) = graded(&task).await;
        if !result.passed {
            failures.push(format!("{}: {}", task.id, explain(&result)));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn offline_trials_are_deterministic() {
    let task = find("tool-loop").unwrap();
    let (first, first_result) = graded(&task).await;
    let (second, second_result) = graded(&task).await;
    assert_eq!(
        first_result.metrics.input_tokens,
        second_result.metrics.input_tokens
    );
    assert_eq!(
        first_result.metrics.output_tokens,
        second_result.metrics.output_tokens
    );
    assert_eq!(
        first_result.metrics.model_calls,
        second_result.metrics.model_calls
    );
    assert_eq!(
        replay::compare(&first.trace, &second.trace).status,
        crate::CheckStatus::Pass
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_recorded_trace_replays_to_the_same_requests() {
    for task in builtin() {
        let id = task.id;
        let (trial, _) = graded(&task).await;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("trace.json");
        trial.trace.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains(&*trial.workspace.to_string_lossy()),
            "{id}: the trace names the trial's directory"
        );
        let loaded = Trace::load(&path).unwrap();
        let (replayed, matches) = replay::replay(&loaded, DEADLINE).await.unwrap();
        assert!(!matches.failed(), "{id}: {}", matches.detail);
        assert_eq!(replayed.trace.provider.model, loaded.provider.model);
        assert_eq!(replayed.trace.served_window, loaded.served_window);
        let result = TrialResult::grade(&task, &replayed);
        assert!(result.passed, "{id}: {}", explain(&result));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_suite_run_reports_pass_hat_k_and_writes_traces() {
    let directory = tempfile::tempdir().unwrap();
    let tasks = [find("tool-loop").unwrap()];
    let report = run_suite(
        "builtin",
        &tasks,
        &[ModelSource::Scripted],
        &RunOptions {
            trials: 2,
            deadline: DEADLINE,
            traces: Some(directory.path().into()),
        },
    )
    .await
    .unwrap();
    assert!(report.all_passed(), "{}", report.table());
    let summary = &report.summaries[0];
    assert_eq!((summary.trials, summary.successes), (2, 2));
    assert_eq!(summary.pass.len(), 2);
    assert!((summary.pass[1].pass_hat_k - 1.0).abs() < 1e-9);
    assert!(summary.tokens_per_success.is_some());
    for result in &report.results {
        assert!(result.trace.as_ref().unwrap().is_file());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_script_that_runs_out_fails_the_trial_without_hanging() {
    let mut task = find("tool-loop").unwrap();
    task.script.steps.truncate(1);
    let (trial, result) = graded(&task).await;
    assert!(!result.passed);
    assert!(!trial.timed_out);
    assert!(trial.outcome().starts_with("error"), "{}", trial.outcome());
    assert!(result.invariants_held, "{}", explain(&result));
}

/// Cache reads a provider reports travel through the engine's usage into
/// the trace, the trial metrics and the report's cache-hit rate.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reported_cache_reads_produce_a_cache_hit_rate() {
    let mut task = find("tool-loop").unwrap();
    for step in &mut task.script.steps {
        if let crate::scripted::Step::Reply(reply) = step {
            reply.usage = Some(serde_json::json!({
                "input_tokens": 1000,
                "output_tokens": 50,
                "cache_read_tokens": 750
            }));
        }
    }
    let (trial, result) = graded(&task).await;
    assert!(result.passed, "{}", explain(&result));
    let calls = u64::from(result.metrics.model_calls);
    assert_eq!(result.metrics.cache_read_tokens, Some(750 * calls));
    let completed = trial.terminal().unwrap();
    let EngineEvent::Completed { usage, .. } = completed else {
        panic!("{completed:?}");
    };
    assert_eq!(usage.cache_read_tokens, Some(750 * calls));
    let report = crate::Report::new("cache", 1, vec![result]);
    let rate = report.summaries[0].cache_hit_rate.unwrap();
    assert!(rate > 0.0 && rate <= 0.75, "{rate}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_children_are_classified_apart_from_the_parent() {
    let (trial, result) = graded(&find("delegation").unwrap()).await;
    assert!(result.passed, "{}", explain(&result));
    assert_eq!(result.metrics.model_calls, 2);
    assert_eq!(result.metrics.child_calls, 4);
    assert_eq!(trial.trace.child_conversations().len(), 2);
}
