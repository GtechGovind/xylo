//! `xylo-evals`: run the evaluation suite, replay traces, list tasks.
//!
//! ```text
//! xylo-evals list
//! xylo-evals run [--suite builtin] [--task ID]... [--trials N] [--json PATH]
//!                [--traces DIR] [--timeout SECONDS] [--matrix FILE]
//! xylo-evals replay TRACE.json [--json PATH] [--timeout SECONDS]
//! ```
//!
//! Exits 0 when every trial passed, 1 when any failed, 2 on a usage or
//! setup error.

use anyhow::{Context, Result, bail, ensure};
use std::{path::PathBuf, process::ExitCode, time::Duration};
use xylo_evals::{
    ModelSource, Report, RunOptions, Trace, TrialResult, matrix, replay, run_suite, suite,
};

const USAGE: &str = "usage:
  xylo-evals list
  xylo-evals run [--suite builtin] [--task ID]... [--trials N] [--json PATH] [--traces DIR] [--timeout SECONDS] [--matrix FILE]
  xylo-evals replay TRACE.json [--json PATH] [--timeout SECONDS]

Runs are offline and scripted unless --matrix is given and XYLO_EVALS_LIVE=1.";

#[tokio::main]
async fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => {
            eprintln!("xylo-evals: {error:#}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// Parsed command-line options.
#[derive(Default)]
struct Options {
    positional: Vec<String>,
    suite: Option<String>,
    tasks: Vec<String>,
    trials: Option<u32>,
    json: Option<PathBuf>,
    traces: Option<PathBuf>,
    timeout: Option<u64>,
    matrix: Option<PathBuf>,
}

fn parse(arguments: Vec<String>) -> Result<Options> {
    let mut options = Options::default();
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        let mut value = || {
            arguments
                .next()
                .with_context(|| format!("{argument} needs a value"))
        };
        match argument.as_str() {
            "--suite" => options.suite = Some(value()?),
            "--task" => options.tasks.push(value()?),
            "--trials" => options.trials = Some(value()?.parse().context("--trials")?),
            "--json" => options.json = Some(value()?.into()),
            "--traces" => options.traces = Some(value()?.into()),
            "--timeout" => options.timeout = Some(value()?.parse().context("--timeout")?),
            "--matrix" => options.matrix = Some(value()?.into()),
            "-h" | "--help" => bail!("help requested"),
            flag if flag.starts_with('-') => bail!("unknown option {flag}"),
            _ => options.positional.push(argument),
        }
    }
    Ok(options)
}

async fn run(arguments: Vec<String>) -> Result<bool> {
    let options = parse(arguments)?;
    let deadline = Duration::from_secs(options.timeout.unwrap_or(300));
    match options.positional.first().map(String::as_str) {
        Some("list") => {
            for task in suite::builtin() {
                let scope = if task.live { "offline+live" } else { "offline" };
                println!("{:<24} {:<13} {}", task.id, scope, task.summary);
            }
            Ok(true)
        }
        Some("run") => {
            let name = options.suite.as_deref().unwrap_or("builtin");
            ensure!(
                name == "builtin",
                "unknown suite {name}; the only suite is builtin"
            );
            let mut tasks = suite::builtin();
            if !options.tasks.is_empty() {
                for id in &options.tasks {
                    ensure!(tasks.iter().any(|task| task.id == id), "unknown task {id}");
                }
                tasks.retain(|task| options.tasks.iter().any(|id| id == task.id));
            }
            let sources = match &options.matrix {
                Some(path) => matrix::load(path)?
                    .into_iter()
                    .map(|model| ModelSource::Live(Box::new(model)))
                    .collect(),
                None => vec![ModelSource::Scripted],
            };
            let trials = options.trials.unwrap_or(5);
            ensure!(trials > 0, "--trials must be at least 1");
            let report = run_suite(
                name,
                &tasks,
                &sources,
                &RunOptions {
                    trials,
                    deadline,
                    traces: options.traces,
                },
            )
            .await?;
            finish(&report, options.json.as_ref())
        }
        Some("replay") => {
            let path = options
                .positional
                .get(1)
                .context("replay needs a trace file")?;
            let trace = Trace::load(path.as_ref())?;
            let task =
                suite::find(&trace.task).with_context(|| format!("unknown task {}", trace.task))?;
            let (trial, matches) = replay::replay(&trace, deadline).await?;
            let mut result = TrialResult::grade(&task, &trial);
            result.invariants.push(matches);
            result.invariants_held = !result.invariants.iter().any(|check| check.failed());
            result.passed = result.task_passed && result.invariants_held;
            let report = Report::new("replay", 1, vec![result]);
            finish(&report, options.json.as_ref())
        }
        _ => bail!("expected list, run or replay"),
    }
}

fn finish(report: &Report, json: Option<&PathBuf>) -> Result<bool> {
    print!("{}", report.table());
    if let Some(path) = json {
        std::fs::write(path, serde_json::to_vec_pretty(report)?)
            .with_context(|| format!("Write {}", path.display()))?;
        println!("report written to {}", path.display());
    }
    Ok(report.all_passed())
}
