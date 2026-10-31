//! Evaluation and trace harness for Xylo's agent engine.
//!
//! The harness runs tasks through the real engine (tool broker, journal,
//! checkpoints, context guard) with the model swapped for a scripted one, or,
//! when the user opts in, for configured real providers. Every trial yields a
//! replayable [`Trace`], task checks, harness [`invariants`] and
//! [`metrics`]; a [`Report`] aggregates trials into pass^k, tokens per
//! success, cache-hit rate, loop-detector firings and compactions.
//!
//! Invariants:
//! - Default runs are offline and deterministic: scripted replies, scripted
//!   token counts, a fresh temporary profile and workspace per trial, no
//!   network. Live runs require [`matrix::LIVE_ENV`]`=1`.
//! - Nothing here changes engine behaviour: the engine sees the recorder
//!   only as a provider layer beneath its own guards.
//! - A check the engine cannot support yet is reported as skipped, never as
//!   passed.

pub mod invariants;
pub mod matrix;
pub mod metrics;
pub mod recorder;
pub mod replay;
pub mod runner;
pub mod scripted;
pub mod suite;
pub mod task;
pub mod trace;

pub use metrics::{Report, TaskSummary, TrialMetrics};
pub use runner::{ModelSource, Trial, TrialResult, run_trial};
pub use task::{ApprovalPolicy, Check, CheckStatus, Task};
pub use trace::{TRACE_VERSION, Trace};

use anyhow::Result;
use std::{path::PathBuf, time::Duration};

/// How a suite run is carried out.
#[derive(Clone, Debug)]
pub struct RunOptions {
    /// Trials per task and model.
    pub trials: u32,
    /// Deadline of one trial.
    pub deadline: Duration,
    /// Where to write one trace per trial, if anywhere.
    pub traces: Option<PathBuf>,
}

/// Runs every task against every model source and grades the trials. Live
/// sources run only the tasks marked `live`.
pub async fn run_suite(
    suite: &str,
    tasks: &[Task],
    sources: &[ModelSource],
    options: &RunOptions,
) -> Result<Report> {
    let mut results = Vec::new();
    for source in sources {
        let live = matches!(source, ModelSource::Live(_));
        for task in tasks.iter().filter(|task| task.live || !live) {
            for trial in 1..=options.trials {
                let run = run_trial(task, source, trial, options.deadline).await?;
                let mut result = TrialResult::grade(task, &run);
                if let Some(directory) = &options.traces {
                    let path = directory
                        .join(&run.trace.model.label)
                        .join(format!("{}-{trial}.json", task.id));
                    run.trace.save(&path)?;
                    result.trace = Some(path);
                }
                results.push(result);
            }
        }
    }
    Ok(Report::new(suite, options.trials, results))
}
