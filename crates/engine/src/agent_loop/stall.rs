//! Deterministic loop and stall detection.
//!
//! After every tool round the loop records a [`Step`]: a signature of the
//! calls it made (tool names with canonical arguments, independent of key
//! order and call order), a signature of their results, and a signature of
//! each failed call (the call's signature plus its error text, with
//! durations such as `0.52s` normalized so timings do not hide a repeat).
//! [`StallDetector::observe`] looks for three patterns in the last
//! [`WINDOW`] steps:
//! - **Repeated calls:** [`REPEATED_PAIRS`] consecutive steps with the same
//!   calls and the same results, or [`REPEATED_CALLS`] with the same calls
//!   whatever the results.
//! - **Alternation:** two different steps alternating with the same results
//!   for [`ALTERNATION`] steps (A B A B A B).
//! - **Repeated error:** the same call (same tool, same arguments) failing
//!   with the same error in [`REPEATED_ERRORS`] of the recent steps, with
//!   other steps in between.
//!
//! Each detection is a strike and the response escalates with the strikes:
//! a note, then a warning that the task will stop, then a stop. After a
//! detection the history restarts from the latest step, so the next strike
//! needs fresh repeats. [`CALM_STEPS`] steps in a row whose calls are new to
//! the history clear the strikes.
//!
//! A step that changed a file (a successful result reporting a diff or a
//! created file) is progress by definition: the workspace the next calls see
//! is different, so an identical call afterwards is not a repeat. It clears
//! the history and the strikes. This keeps an edit-and-test cycle that hits
//! the same test failure while the model really edits from ever escalating.
//!
//! Everything here is a pure function of the recorded steps: no model is
//! consulted and nothing depends on time.
use crate::model::{Message, ToolCall};
use serde_json::Value;
use std::{
    collections::{VecDeque, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
};

/// Steps the detector remembers.
pub(super) const WINDOW: usize = 8;
/// Consecutive identical steps (same calls, same results) that are a loop.
pub(super) const REPEATED_PAIRS: usize = 3;
/// Consecutive steps with the same calls, whatever their results.
pub(super) const REPEATED_CALLS: usize = 4;
/// Steps of an A B A B … alternation that are a loop.
pub(super) const ALTERNATION: usize = 6;
/// Recent steps in which the same call failed with the same error.
pub(super) const REPEATED_ERRORS: usize = 3;
/// Steps in a row with new calls that clear earlier strikes.
pub(super) const CALM_STEPS: u32 = 4;

/// One tool round, reduced to signatures.
#[derive(Clone, Debug)]
pub(super) struct Step {
    action: u64,
    outcome: u64,
    /// `(tool, signature)` of every failed call; the signature covers the
    /// call's arguments and its error text.
    errors: Vec<(String, u64)>,
    /// The tools called, sorted and without duplicates.
    tools: Vec<String>,
    /// A call in this step changed a file.
    changed_files: bool,
}

impl Step {
    /// The step of `calls` answered by `results` (in call order).
    pub(super) fn new(calls: &[ToolCall], results: &[Message]) -> Self {
        let mut pairs = calls
            .iter()
            .enumerate()
            .map(|(index, call)| {
                let result = results
                    .iter()
                    .find(|result| result.tool_call_id.as_deref() == Some(call.id.as_str()))
                    .or_else(|| results.get(index))
                    .map_or("", |result| result.content.as_str());
                (call_signature(call), call.name.as_str(), result)
            })
            .collect::<Vec<_>>();
        pairs.sort_unstable();
        let action = hash(pairs.iter().map(|(call, _, _)| call));
        let outcome = hash(pairs.iter().map(|(call, _, result)| (call, result)));
        let mut errors = pairs
            .iter()
            .filter_map(|(call, tool, result)| {
                let error = error_text(result)?;
                Some(((*tool).to_owned(), hash([call.as_str(), error.as_str()])))
            })
            .collect::<Vec<_>>();
        errors.dedup();
        let changed_files = pairs.iter().any(|(_, _, result)| changed_file(result));
        let mut tools = calls
            .iter()
            .map(|call| call.name.clone())
            .collect::<Vec<_>>();
        tools.sort_unstable();
        tools.dedup();
        Self {
            action,
            outcome,
            errors,
            tools,
            changed_files,
        }
    }

    fn same(&self, other: &Self) -> bool {
        self.action == other.action && self.outcome == other.outcome
    }
}

/// A repeated pattern the detector found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Pattern {
    /// The same calls `count` times in a row.
    RepeatedCall { tools: Vec<String>, count: usize },
    /// Two steps alternating, each `count` times.
    Alternating { tools: Vec<String>, count: usize },
    /// The same `tool` call failed with the same error in `count` recent
    /// steps.
    RepeatedError { tool: String, count: usize },
}

impl Pattern {
    /// The pattern as the model reads it in a note.
    pub(super) fn describe(&self) -> String {
        match self {
            Self::RepeatedCall { tools, count } => {
                crate::prompts::stall_repeated_call(&quoted(tools, " + "), *count)
            }
            Self::Alternating { tools, count } => {
                crate::prompts::stall_alternating(&quoted(tools, " and "), *count)
            }
            Self::RepeatedError { tool, count } => {
                crate::prompts::stall_repeated_error(tool, *count)
            }
        }
    }

    /// The pattern as the user reads it.
    pub(super) fn summary(&self) -> String {
        match self {
            Self::RepeatedCall { tools, count } => {
                format!(
                    "the same {} call {count} times in a row",
                    quoted(tools, " + ")
                )
            }
            Self::Alternating { tools, count } => format!(
                "{} alternating {count} times with the same results",
                quoted(tools, " and ")
            ),
            Self::RepeatedError { tool, count } => {
                format!("the same `{tool}` call failing with the same error {count} times")
            }
        }
    }
}

/// What the loop must do about a detected pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    /// First strike: a note asking for a different approach.
    Note(Pattern),
    /// Second strike: a note that the task stops if this continues.
    Warning(Pattern),
    /// Third strike: stop the turn.
    Stop(Pattern),
}

/// Watches the tool rounds of one run.
#[derive(Debug, Default)]
pub(super) struct StallDetector {
    history: VecDeque<Step>,
    strikes: u32,
    calm: u32,
}

impl StallDetector {
    /// Records a finished tool round; `Some` when it completes a pattern.
    pub(super) fn observe(&mut self, step: Step) -> Option<Verdict> {
        if step.changed_files {
            self.history.clear();
            self.history.push_back(step);
            self.strikes = 0;
            self.calm = 0;
            return None;
        }
        let novel = self.history.iter().all(|seen| seen.action != step.action);
        self.history.push_back(step);
        while self.history.len() > WINDOW {
            self.history.pop_front();
        }
        let Some(pattern) = self.pattern() else {
            self.calm = if novel { self.calm + 1 } else { 0 };
            if self.calm >= CALM_STEPS {
                self.strikes = 0;
            }
            return None;
        };
        self.strikes += 1;
        self.calm = 0;
        let latest = self.history.pop_back();
        self.history.clear();
        self.history.extend(latest);
        Some(match self.strikes {
            1 => Verdict::Note(pattern),
            2 => Verdict::Warning(pattern),
            _ => Verdict::Stop(pattern),
        })
    }

    fn pattern(&self) -> Option<Pattern> {
        self.repeated()
            .or_else(|| self.alternating())
            .or_else(|| self.repeated_error())
    }

    fn repeated(&self) -> Option<Pattern> {
        let last = self.history.back()?;
        let trailing = |matches: &dyn Fn(&Step) -> bool| {
            self.history
                .iter()
                .rev()
                .take_while(|step| matches(step))
                .count()
        };
        let pairs = trailing(&|step| step.same(last));
        let calls = trailing(&|step| step.action == last.action);
        let count = if pairs >= REPEATED_PAIRS {
            pairs
        } else if calls >= REPEATED_CALLS {
            calls
        } else {
            return None;
        };
        Some(Pattern::RepeatedCall {
            tools: last.tools.clone(),
            count,
        })
    }

    fn alternating(&self) -> Option<Pattern> {
        let recent = self
            .history
            .iter()
            .rev()
            .take(ALTERNATION)
            .collect::<Vec<_>>();
        if recent.len() < ALTERNATION || recent[0].action == recent[1].action {
            return None;
        }
        if !(2..ALTERNATION).all(|index| recent[index].same(recent[index - 2])) {
            return None;
        }
        let mut tools = recent[0].tools.clone();
        tools.extend(recent[1].tools.iter().cloned());
        tools.sort_unstable();
        tools.dedup();
        Some(Pattern::Alternating {
            tools,
            count: ALTERNATION / 2,
        })
    }

    fn repeated_error(&self) -> Option<Pattern> {
        let last = self.history.back()?;
        last.errors.iter().find_map(|(tool, signature)| {
            let count = self
                .history
                .iter()
                .filter(|step| step.errors.iter().any(|(_, seen)| seen == signature))
                .count();
            (count >= REPEATED_ERRORS).then(|| Pattern::RepeatedError {
                tool: tool.clone(),
                count,
            })
        })
    }
}

/// `name` plus the arguments with object keys sorted, so the same call
/// written with a different key order or spacing has the same signature.
fn call_signature(call: &ToolCall) -> String {
    let mut signature = call.name.clone();
    signature.push('\0');
    match serde_json::from_str::<Value>(&call.arguments) {
        Ok(arguments) => canonical(&arguments, &mut signature),
        Err(_) => signature.push_str(&call.arguments),
    }
    signature
}

fn canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut entries = map.iter().collect::<Vec<_>>();
            entries.sort_unstable_by(|left, right| left.0.cmp(right.0));
            out.push('{');
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                canonical(value, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                canonical(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// The error text of a result that reports failure, with durations
/// normalized; `None` for a successful or non-JSON result. A result fails
/// the way tool observations do: `success: false`, `isError: true` or an
/// unsuccessful `status` (which covers the loop's own error results).
fn error_text(result: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(result).ok()?;
    let failed = value.get("success").and_then(Value::as_bool) == Some(false)
        || value.get("isError").and_then(Value::as_bool) == Some(true)
        || value
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| {
                matches!(
                    status,
                    "error" | "failed" | "failure" | "timeout" | "outcome_unknown"
                )
            });
    if !failed {
        return None;
    }
    let text = match value.get("summary") {
        Some(Value::String(summary)) => summary.clone(),
        Some(summary) => summary.to_string(),
        None => value.to_string(),
    };
    Some(without_durations(&text))
}

/// Whether `result` is a successful file change: no failure, and a diff
/// with added or removed lines, or a newly created file (the shape
/// `file_edit` and `file_write` report).
fn changed_file(result: &str) -> bool {
    if error_text(result).is_some() {
        return false;
    }
    let Ok(value) = serde_json::from_str::<Value>(result) else {
        return false;
    };
    let lines = |key| value.get(key).and_then(Value::as_u64).unwrap_or(0);
    lines("added") + lines("removed") > 0
        || value.get("created").and_then(Value::as_bool) == Some(true)
}

/// `text` with every number followed by a time unit (`12ms`, `0.52 s`,
/// `3 minutes`) replaced by `#` and the unit.
fn without_durations(text: &str) -> String {
    const UNITS: [&str; 10] = [
        "ms", "s", "sec", "secs", "second", "seconds", "m", "min", "mins", "minutes",
    ];
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(|c: char| c.is_ascii_digit()) {
        out.push_str(&rest[..start]);
        let number = &rest[start..];
        let length = number
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(number.len());
        let after = &number[length..];
        let unit_text = after.strip_prefix(' ').unwrap_or(after);
        let unit_length = unit_text
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(unit_text.len());
        let unit = &unit_text[..unit_length];
        if UNITS.contains(&unit) {
            out.push('#');
            out.push_str(unit);
            rest = &unit_text[unit_length..];
        } else {
            out.push_str(&number[..length]);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// `names` as code spans joined by `separator`.
fn quoted(names: &[String], separator: &str) -> String {
    names
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(separator)
}

fn hash(value: impl IntoIterator<Item = impl Hash>) -> u64 {
    let mut hasher = DefaultHasher::new();
    for item in value {
        item.hash(&mut hasher);
    }
    hasher.finish()
}

#[cfg(test)]
mod tests;
