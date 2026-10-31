//! Per-trial measurements and multi-trial aggregates.
//!
//! Reliability is reported as pass^k (the chance that *all* of k independent
//! trials succeed, estimated without bias from n trials as C(c,k)/C(n,k), as
//! in τ-bench) next to pass@k (the chance that at least one does). Cost is
//! tokens per success: every token spent on the task, failed trials
//! included, divided by the number of successes. Cache-hit rate and the
//! harness's own interventions (loop-detector firings, compactions) are
//! counted from the trace; cache reads are reported as absent rather than
//! zero when no provider reply carries them.

use crate::{
    runner::{Trial, TrialResult},
    trace::{ModelLabel, Outcome, RequestKind},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use xylo_core::{EngineEvent, ErrorKind, NoteKind, TurnPhase};

/// The report format this crate writes.
pub const REPORT_VERSION: u32 = 1;

/// The k of the pass^k column in [`Report::table`].
pub const TABLE_K: u32 = 3;

/// Usage keys providers and the engine use for prompt tokens read from cache.
const CACHE_READ_KEYS: &[&str] = &[
    "cache_read_tokens",
    "cache_read_input_tokens",
    "cached_input_tokens",
    "cached_tokens",
];

/// What one trial measured.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TrialMetrics {
    /// Root agent-loop model requests.
    pub model_calls: u32,
    /// Model requests of delegated child agents.
    pub child_calls: u32,
    /// Context summary requests.
    pub summary_calls: u32,
    /// Prompt tokens over every request, summaries and titles included.
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Prompt tokens served from the provider's cache, when reported.
    pub cache_read_tokens: Option<u64>,
    /// Times the turn entered the compacting phase.
    pub compactions: u32,
    /// Notes the harness added to the conversation (answer continuations,
    /// retry nudges, stall and budget warnings), one per `HarnessNote` event.
    pub harness_notes: u32,
    /// Loop-detector firings that reached the model as a note or warning.
    pub loop_notes: u32,
    /// The loop detector stopped the turn.
    pub loop_stopped: bool,
    pub tool_calls: u32,
    pub failed_tool_calls: u32,
    pub approvals: u32,
    /// Provider errors the agent loops saw (each retry counts).
    pub provider_errors: u32,
    pub elapsed_ms: u64,
}

impl TrialMetrics {
    /// Measures `trial` from its trace.
    pub fn of(trial: &Trial) -> Self {
        let trace = &trial.trace;
        let mut metrics = Self::default();
        let mut cache_read = None;
        for exchange in &trace.exchanges {
            match exchange.kind {
                RequestKind::Agent => metrics.model_calls += 1,
                RequestKind::Child => metrics.child_calls += 1,
                RequestKind::Summary => metrics.summary_calls += 1,
                RequestKind::Title => {}
            }
            match &exchange.outcome {
                Outcome::Reply(reply) => {
                    if let Some(usage) = &reply.usage {
                        metrics.input_tokens += count(usage, "input_tokens");
                        metrics.output_tokens += count(usage, "output_tokens");
                        if let Some(read) = cache_read_tokens(usage) {
                            *cache_read.get_or_insert(0) += read;
                        }
                    }
                }
                Outcome::Error(_)
                    if matches!(exchange.kind, RequestKind::Agent | RequestKind::Child) =>
                {
                    metrics.provider_errors += 1;
                }
                _ => {}
            }
        }
        metrics.cache_read_tokens = cache_read;
        for event in &trace.events {
            match event {
                EngineEvent::Phase {
                    phase: TurnPhase::Compacting,
                } => metrics.compactions += 1,
                EngineEvent::ToolStarted { .. } => metrics.tool_calls += 1,
                EngineEvent::ToolFinished { success: false, .. } => {
                    metrics.failed_tool_calls += 1;
                }
                EngineEvent::ApprovalRequired { .. } => metrics.approvals += 1,
                EngineEvent::Completed { usage, .. } => metrics.elapsed_ms = usage.elapsed_ms,
                EngineEvent::HarnessNote { kind, .. } => {
                    metrics.harness_notes += 1;
                    if matches!(kind, NoteKind::StallNotice | NoteKind::StallWarning) {
                        metrics.loop_notes += 1;
                    }
                }
                EngineEvent::Error {
                    kind: ErrorKind::Stalled,
                    ..
                } => metrics.loop_stopped = true,
                _ => {}
            }
        }
        metrics
    }
}

/// A numeric usage field, zero when absent.
pub(crate) fn count(usage: &Value, key: &str) -> u64 {
    usage.get(key).and_then(Value::as_u64).unwrap_or_default()
}

/// Cache-read prompt tokens, if the usage reports them under any known key.
fn cache_read_tokens(usage: &Value) -> Option<u64> {
    CACHE_READ_KEYS
        .iter()
        .find_map(|key| usage.get(*key).and_then(Value::as_u64))
}

/// pass^k: the unbiased estimate, from `successes` of `trials`, that k fresh
/// trials all succeed. `None` when k exceeds the trials run.
pub fn pass_hat_k(trials: u32, successes: u32, k: u32) -> Option<f64> {
    if k == 0 || k > trials {
        return None;
    }
    Some(
        (0..k)
            .map(|i| f64::from(successes.saturating_sub(i)) / f64::from(trials - i))
            .product(),
    )
}

/// pass@k: the unbiased estimate that at least one of k fresh trials
/// succeeds. `None` when k exceeds the trials run.
pub fn pass_at_k(trials: u32, successes: u32, k: u32) -> Option<f64> {
    if k == 0 || k > trials {
        return None;
    }
    let failures = trials - successes;
    let all_fail: f64 = (0..k)
        .map(|i| f64::from(failures.saturating_sub(i)) / f64::from(trials - i))
        .product();
    Some(1.0 - all_fail)
}

/// Reliability at one k.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PassK {
    pub k: u32,
    pub pass_hat_k: f64,
    pub pass_at_k: f64,
}

/// One task's results against one model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskSummary {
    pub task: String,
    pub model: ModelLabel,
    pub trials: u32,
    /// Trials that passed their checks and kept every invariant.
    pub successes: u32,
    /// Trials that passed their checks but broke an invariant.
    pub succeeded_with_violations: u32,
    pub pass: Vec<PassK>,
    /// Mean tokens (input plus output) per trial.
    pub tokens_per_trial: f64,
    /// All tokens spent divided by successes; `None` without a success.
    pub tokens_per_success: Option<f64>,
    /// Cache-read share of prompt tokens; `None` when never reported.
    pub cache_hit_rate: Option<f64>,
    pub loop_notes: u32,
    /// Trials the loop detector stopped.
    pub loop_stops: u32,
    pub compactions: u32,
    pub mean_model_calls: f64,
    /// Checks that could not be evaluated, by name.
    pub skipped: BTreeMap<String, u32>,
    /// Failed checks and invariants, by name.
    pub failures: BTreeMap<String, u32>,
}

impl TaskSummary {
    /// Aggregates the trials of one task and model.
    pub fn of(results: &[&TrialResult]) -> Option<Self> {
        let first = results.first()?;
        let trials = results.len() as u32;
        let successes = results.iter().filter(|result| result.passed).count() as u32;
        let tokens: u64 = results
            .iter()
            .map(|result| result.metrics.input_tokens + result.metrics.output_tokens)
            .sum();
        let prompt: u64 = results
            .iter()
            .map(|result| result.metrics.input_tokens)
            .sum();
        let cache_reads: Vec<u64> = results
            .iter()
            .filter_map(|result| result.metrics.cache_read_tokens)
            .collect();
        let mut skipped = BTreeMap::new();
        let mut failures = BTreeMap::new();
        for check in results
            .iter()
            .flat_map(|result| result.checks.iter().chain(&result.invariants))
        {
            match check.status {
                crate::task::CheckStatus::Skip => {
                    *skipped.entry(check.name.clone()).or_default() += 1
                }
                crate::task::CheckStatus::Fail => {
                    *failures.entry(check.name.clone()).or_default() += 1
                }
                crate::task::CheckStatus::Pass => {}
            }
        }
        Some(Self {
            task: first.task.clone(),
            model: first.model.clone(),
            trials,
            successes,
            succeeded_with_violations: results
                .iter()
                .filter(|result| result.task_passed && !result.invariants_held)
                .count() as u32,
            pass: (1..=trials)
                .filter_map(|k| {
                    Some(PassK {
                        k,
                        pass_hat_k: pass_hat_k(trials, successes, k)?,
                        pass_at_k: pass_at_k(trials, successes, k)?,
                    })
                })
                .collect(),
            tokens_per_trial: tokens as f64 / f64::from(trials),
            tokens_per_success: (successes > 0).then(|| tokens as f64 / f64::from(successes)),
            cache_hit_rate: (!cache_reads.is_empty() && prompt > 0)
                .then(|| cache_reads.iter().sum::<u64>() as f64 / prompt as f64),
            loop_notes: results.iter().map(|result| result.metrics.loop_notes).sum(),
            loop_stops: results
                .iter()
                .filter(|result| result.metrics.loop_stopped)
                .count() as u32,
            compactions: results
                .iter()
                .map(|result| result.metrics.compactions)
                .sum(),
            mean_model_calls: results
                .iter()
                .map(|result| f64::from(result.metrics.model_calls))
                .sum::<f64>()
                / f64::from(trials),
            skipped,
            failures,
        })
    }
}

/// A suite run: summaries per task and model, plus every graded trial.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report {
    pub version: u32,
    pub suite: String,
    /// Milliseconds since the Unix epoch.
    pub generated_at: i64,
    pub trials_per_task: u32,
    pub summaries: Vec<TaskSummary>,
    pub results: Vec<TrialResult>,
}

impl Report {
    /// Summarizes `results`, grouped by model then task in run order.
    pub fn new(suite: &str, trials_per_task: u32, results: Vec<TrialResult>) -> Self {
        let mut groups: Vec<((ModelLabel, String), Vec<&TrialResult>)> = Vec::new();
        for result in &results {
            let key = (result.model.clone(), result.task.clone());
            match groups.iter_mut().find(|(group, _)| *group == key) {
                Some((_, members)) => members.push(result),
                None => groups.push((key, vec![result])),
            }
        }
        let summaries = groups
            .iter()
            .filter_map(|(_, members)| TaskSummary::of(members))
            .collect();
        Self {
            version: REPORT_VERSION,
            suite: suite.into(),
            generated_at: xylo_core::now_ms(),
            trials_per_task,
            summaries,
            results,
        }
    }

    /// Whether every trial succeeded.
    pub fn all_passed(&self) -> bool {
        self.results.iter().all(|result| result.passed)
    }

    /// A plain-text table of the summaries. Reliability is shown as
    /// pass^k at [`TABLE_K`] (or the trials run, if fewer); the JSON report
    /// has every k.
    pub fn table(&self) -> String {
        let k = TABLE_K.min(self.trials_per_task).max(1);
        let mut out = format!(
            "{:<24} {:<12} {:>6} {:>7} {:>10} {:>12} {:>6} {:>7} {:>5}\n",
            "task",
            "model",
            "pass",
            format!("pass^{k}"),
            "tok/succ",
            "cache-hit",
            "loops",
            "compact",
            "viol"
        );
        for summary in &self.summaries {
            let pass_hat = summary
                .pass
                .iter()
                .find(|pass| pass.k == k)
                .map_or(0.0, |pass| pass.pass_hat_k);
            out.push_str(&format!(
                "{:<24} {:<12} {:>6} {:>7.2} {:>10} {:>12} {:>6} {:>7} {:>5}\n",
                summary.task,
                summary.model.label,
                format!("{}/{}", summary.successes, summary.trials),
                pass_hat,
                summary
                    .tokens_per_success
                    .map_or_else(|| "-".into(), |tokens| format!("{tokens:.0}")),
                summary.cache_hit_rate.map_or_else(
                    || "not reported".into(),
                    |rate| format!("{:.1}%", rate * 100.0)
                ),
                summary.loop_notes,
                summary.compactions,
                summary.succeeded_with_violations,
            ));
        }
        for result in self.results.iter().filter(|result| !result.passed) {
            let failed: Vec<String> = result
                .checks
                .iter()
                .chain(&result.invariants)
                .filter(|check| check.failed())
                .map(|check| format!("{} ({})", check.name, check.detail))
                .collect();
            out.push_str(&format!(
                "FAIL {} #{} [{}] {}: {}\n",
                result.task,
                result.trial,
                result.model.label,
                result.outcome,
                failed.join("; ")
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests;
