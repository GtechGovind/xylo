//! Deterministic replay of a recorded trace.
//!
//! Replay rebuilds the trace's task in a fresh directory and runs it under
//! the recorded provider configuration, answering the engine's requests with
//! the recorded replies: root agent replies and provider failures in order,
//! each delegated child's replies routed by its task, summaries and titles
//! from their own queues. When the recording had a scripted served window,
//! replay applies the window again instead of replaying the rejections it
//! caused, so a harness change that fixes or breaks overflow recovery shows.
//! Tools really run again, so replay checks the harness, not a recording of
//! it. The replayed trial is graded like any other, and [`compare`] reports
//! the first point where the engine's requests diverge from the recording.
//!
//! Comparison ignores system prompts (they carry the date and host) and
//! values that differ on every run by design: UUIDs and timestamps are
//! replaced, on each side, by placeholders numbered in order of first
//! appearance. Two runs therefore match when they use the same *pattern* of
//! identifiers, not the same identifiers.

use crate::{
    runner::{ModelSource, Replay, Trial, run_trial_with},
    scripted::{ChildScript, Script, Step},
    suite,
    task::Check,
    trace::{ErrorKind, Exchange, Outcome, RequestKind, Trace},
};
use anyhow::{Context, Result};
use regex::Regex;
use std::{collections::HashMap, sync::LazyLock, time::Duration};
use xylo_engine::{Message, Role};

/// The script that answers exactly as the trace's model did.
pub fn script(trace: &Trace) -> Script {
    let replayed = |outcome: &Outcome| -> Option<Step> {
        match outcome {
            Outcome::Reply(reply) => Some(Step::Reply(reply.clone())),
            // The served window rejects again on its own.
            Outcome::Error(error)
                if trace.served_window.is_some() && error.kind == ErrorKind::ContextOverflow =>
            {
                None
            }
            Outcome::Error(error) => Some(Step::Fail(error.clone())),
            // A request the turn abandoned (cancellation) has no answer.
            Outcome::Abandoned => None,
        }
    };
    let mut script = Script {
        served_window: trace.served_window,
        ..Script::default()
    };
    for exchange in &trace.exchanges {
        match (exchange.kind, &exchange.outcome) {
            (RequestKind::Agent, outcome) => script.steps.extend(replayed(outcome)),
            (RequestKind::Summary, Outcome::Reply(reply)) => {
                script.summaries.push(reply.text.clone());
            }
            (RequestKind::Title, Outcome::Reply(reply)) => script.titles.push(reply.text.clone()),
            _ => {}
        }
    }
    script.children = trace
        .child_conversations()
        .into_iter()
        .map(|(task, exchanges)| ChildScript {
            task,
            steps: exchanges
                .iter()
                .filter_map(|exchange| replayed(&exchange.outcome))
                .collect(),
        })
        .collect();
    script
}

/// Replays `trace` against the current engine. The trace's task must be in
/// the built-in suite, which supplies its fixtures and grader.
pub async fn replay(trace: &Trace, deadline: Duration) -> Result<(Trial, Check)> {
    let task = suite::find(&trace.task)
        .with_context(|| format!("Trace task {} is not in the built-in suite", trace.task))?;
    let setup = Replay {
        script: script(trace),
        provider: trace.provider.clone(),
    };
    let trial = run_trial_with(
        &task,
        &ModelSource::Scripted,
        trace.trial,
        deadline,
        Some(setup),
    )
    .await?;
    let check = compare(trace, &trial.trace);
    Ok((trial, check))
}

/// Whether `replayed` made the same requests as `recorded`: the root agent
/// requests in order, and each child's requests in order.
pub fn compare(recorded: &Trace, replayed: &Trace) -> Check {
    const NAME: &str = "replay_matches_trace";
    let mut before = Normalizer::default();
    let mut after = Normalizer::default();
    let (old_root, new_root): (Vec<_>, Vec<_>) = (
        recorded.agent_exchanges().collect(),
        replayed.agent_exchanges().collect(),
    );
    if let Err(difference) = same_requests(
        "agent request",
        &old_root,
        &new_root,
        &mut before,
        &mut after,
    ) {
        return Check::expect(NAME, false, difference);
    }
    let children = replayed.child_conversations();
    for (task, old) in recorded.child_conversations() {
        let Some((_, new)) = children.iter().find(|(known, _)| *known == task) else {
            return Check::expect(NAME, false, format!("child {task:?} did not run"));
        };
        if let Err(difference) = same_requests("child request", &old, new, &mut before, &mut after)
        {
            return Check::expect(NAME, false, format!("{difference} of child {task:?}"));
        }
    }
    if children.len() != recorded.child_conversations().len() {
        return Check::expect(NAME, false, "an unrecorded child ran");
    }
    Check::expect(
        NAME,
        true,
        format!("{} agent requests match", new_root.len()),
    )
}

fn same_requests(
    label: &str,
    old: &[&Exchange],
    new: &[&Exchange],
    before: &mut Normalizer,
    after: &mut Normalizer,
) -> Result<(), String> {
    for (number, (old, new)) in old.iter().zip(new).enumerate() {
        let old = before.conversation(&old.request.messages);
        let new = after.conversation(&new.request.messages);
        if old.len() != new.len() {
            return Err(format!(
                "{label} {} has {} messages, recorded {}",
                number + 1,
                new.len(),
                old.len()
            ));
        }
        if let Some(index) = (0..old.len()).find(|&index| old[index] != new[index]) {
            return Err(format!(
                "{label} {} differs at message {}: recorded {}, replayed {}",
                number + 1,
                index + 1,
                excerpt(&old[index]),
                excerpt(&new[index])
            ));
        }
    }
    if old.len() != new.len() {
        return Err(format!("{} {label}s, recorded {}", new.len(), old.len()));
    }
    Ok(())
}

/// The start of a long message, for a divergence report.
fn excerpt(text: &str) -> String {
    const CHARS: usize = 240;
    let mut short: String = text.chars().take(CHARS).collect();
    if text.chars().count() > CHARS {
        short.push('…');
    }
    short
}

/// UUIDs, ISO 8601 date-times and millisecond Unix timestamps (2001-2286).
static VOLATILE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?P<uuid>\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b)",
        r"|(?P<time>\b\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}(?::\d{2}(?:\.\d+)?)?(?:Z|[+-]\d{2}:?\d{2})?",
        r"|\b1\d{12}\b)",
    ))
    .expect("the volatile-value pattern is valid")
});

/// Replaces run-specific values with numbered placeholders, consistently
/// across everything one side of a comparison passes through it.
#[derive(Default)]
pub(crate) struct Normalizer {
    seen: HashMap<String, String>,
    uuids: usize,
    times: usize,
}

impl Normalizer {
    /// `text` with each UUID and timestamp replaced by its placeholder.
    pub(crate) fn apply(&mut self, text: &str) -> String {
        VOLATILE
            .replace_all(text, |captures: &regex::Captures| {
                let value = &captures[0];
                if let Some(known) = self.seen.get(value) {
                    return known.clone();
                }
                let placeholder = if captures.name("uuid").is_some() {
                    self.uuids += 1;
                    format!("<uuid-{}>", self.uuids)
                } else {
                    self.times += 1;
                    format!("<time-{}>", self.times)
                };
                self.seen.insert(value.to_owned(), placeholder.clone());
                placeholder
            })
            .into_owned()
    }

    /// The non-system messages of a request, serialized and normalized.
    fn conversation(&mut self, messages: &[Message]) -> Vec<String> {
        messages
            .iter()
            .filter(|message| message.role != Role::System)
            .map(|message| self.apply(&serde_json::to_string(message).unwrap_or_default()))
            .collect()
    }
}

#[cfg(test)]
mod tests;
