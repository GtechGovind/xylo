//! A deterministic model that plays back a script.
//!
//! Root agent-loop requests consume [`Script::steps`] in order; summary and
//! title requests are answered from their own queues so a task's script does
//! not depend on when the harness decides to compact. A delegated child is
//! routed to the [`ChildScript`] whose `task` matches its task message, with
//! its own cursor, so parallel children get the same replies whatever order
//! they run in (two children with the same task share one script). Token
//! usage is counted from request and reply sizes, so offline runs report
//! stable costs.
//!
//! Invariants:
//! - The provider never touches the network and never sleeps.
//! - A request beyond the last agent step fails with a permanent error that
//!   names the step, so a harness that loops longer than the script expects
//!   fails loudly instead of hanging.
//! - With [`Script::served_window`] set, an agent request estimated above it
//!   fails with `ContextOverflow` *without* consuming a step, the way a server
//!   that serves a smaller window than configured rejects a prompt.

use crate::trace::{ModelReply, RecordedError, RequestKind, first_user};
use futures_util::stream;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use xylo_engine::{EventStream, ModelError, ModelProvider, ModelRequest, ModelUsage, StreamEvent};

/// Summary returned when a script names none.
pub const DEFAULT_SUMMARY: &str = "## Goal\nThe evaluation task.\n\n## Progress\nEarlier steps were completed as recorded in the tool results.";
/// Title returned when a script names none.
pub const DEFAULT_TITLE: &str = "Evaluation task";

/// One scripted response to an agent-loop request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum Step {
    Reply(ModelReply),
    Fail(RecordedError),
}

/// The replies of one delegated child agent.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChildScript {
    /// Text of the child's task message, or a part of it. An exact match is
    /// preferred over a partial one.
    pub task: String,
    pub steps: Vec<Step>,
}

/// What a scripted model answers.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Script {
    /// Root agent-loop replies, in order.
    pub steps: Vec<Step>,
    /// Replies of delegated children, routed by task.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<ChildScript>,
    /// Summary texts, in order; [`DEFAULT_SUMMARY`] once exhausted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub summaries: Vec<String>,
    /// Titles, in order; [`DEFAULT_TITLE`] once exhausted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub titles: Vec<String>,
    /// Estimated prompt tokens above which agent requests are rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub served_window: Option<u64>,
}

impl Script {
    /// A script of agent replies only.
    pub fn replies(replies: impl IntoIterator<Item = ModelReply>) -> Self {
        Self {
            steps: replies.into_iter().map(Step::Reply).collect(),
            ..Self::default()
        }
    }
}

#[derive(Default)]
struct Cursors {
    steps: usize,
    children: Vec<usize>,
    summaries: usize,
    titles: usize,
}

/// A [`ModelProvider`] that plays back a [`Script`].
pub struct ScriptedProvider {
    script: Script,
    cursors: Mutex<Cursors>,
}

impl ScriptedProvider {
    /// A provider at the start of `script`.
    pub fn new(script: Script) -> Self {
        Self {
            script,
            cursors: Mutex::default(),
        }
    }

    /// The reply to `request`, advancing the matching queue.
    fn answer(&self, request: &ModelRequest) -> Result<ModelReply, ModelError> {
        let mut cursors = self.cursors.lock();
        match RequestKind::of(request) {
            RequestKind::Title => {
                let title = self.script.titles.get(cursors.titles).cloned();
                cursors.titles += 1;
                Ok(ModelReply::text(
                    title.unwrap_or_else(|| DEFAULT_TITLE.into()),
                ))
            }
            RequestKind::Summary => {
                let summary = self.script.summaries.get(cursors.summaries).cloned();
                cursors.summaries += 1;
                Ok(ModelReply::text(
                    summary.unwrap_or_else(|| DEFAULT_SUMMARY.into()),
                ))
            }
            RequestKind::Agent => {
                self.check_window(request)?;
                let index = cursors.steps;
                cursors.steps += 1;
                play(self.script.steps.get(index), index, "")
            }
            RequestKind::Child => {
                self.check_window(request)?;
                let task = first_user(&request.messages).unwrap_or_default();
                let Some(child) = self.child(task) else {
                    return Err(ModelError::permanent(format!(
                        "The script has no child for the task {task:?}"
                    )));
                };
                if cursors.children.len() < self.script.children.len() {
                    cursors.children.resize(self.script.children.len(), 0);
                }
                let index = cursors.children[child];
                cursors.children[child] += 1;
                play(
                    self.script.children[child].steps.get(index),
                    index,
                    " of a child",
                )
            }
        }
    }
}

impl ScriptedProvider {
    /// Rejects a request above the served window.
    fn check_window(&self, request: &ModelRequest) -> Result<(), ModelError> {
        let estimate = estimate_tokens(request);
        match self.script.served_window {
            Some(window) if estimate > window => Err(ModelError::ContextOverflow(format!(
                "This model's maximum context length is {window} tokens; the request has about {estimate}"
            ))),
            _ => Ok(()),
        }
    }

    /// The child script for `task`: an exact match, else the first partial one.
    fn child(&self, task: &str) -> Option<usize> {
        let children = &self.script.children;
        children
            .iter()
            .position(|child| child.task == task)
            .or_else(|| children.iter().position(|child| task.contains(&child.task)))
    }
}

/// The reply of step `index`, or a loud failure past the end of the script.
fn play(step: Option<&Step>, index: usize, whose: &str) -> Result<ModelReply, ModelError> {
    match step {
        Some(Step::Reply(reply)) => Ok(reply.clone()),
        Some(Step::Fail(error)) => Err(error.to_model_error()),
        None => Err(ModelError::permanent(format!(
            "The script has no step {}{whose}: the harness made more model calls than the task scripted",
            index + 1
        ))),
    }
}

impl ModelProvider for ScriptedProvider {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> EventStream {
        if cancel.is_cancelled() {
            return Box::pin(stream::iter([Err(ModelError::Interrupted)]));
        }
        let events = match self.answer(&request) {
            Ok(reply) => reply_events(&request, reply).into_iter().map(Ok).collect(),
            Err(error) => vec![Err(error)],
        };
        Box::pin(stream::iter(events))
    }
}

/// The stream events of `reply`, in provider order.
fn reply_events(request: &ModelRequest, reply: ModelReply) -> Vec<StreamEvent> {
    let usage = reply
        .usage
        .as_ref()
        .and_then(usage_from_json)
        .unwrap_or_else(|| usage(estimate_tokens(request), estimate_reply_tokens(&reply)));
    let mut events = Vec::new();
    if let Some(reasoning) = reply.reasoning {
        events.push(StreamEvent::Reasoning(reasoning));
    }
    if !reply.text.is_empty() {
        events.push(StreamEvent::Text(reply.text));
    }
    for (index, call) in reply.tool_calls.into_iter().enumerate() {
        events.push(StreamEvent::ToolCall {
            index: index as u32,
            id: Some(call.id),
            name: Some(call.name),
            arguments: Some(call.arguments),
        });
    }
    events.push(StreamEvent::Usage(usage));
    events.push(StreamEvent::Finished(reply.finish));
    events
}

/// Prompt tokens of `request` at four bytes per token: a stable stand-in for
/// a tokenizer, used for scripted costs and the served-window check.
pub fn estimate_tokens(request: &ModelRequest) -> u64 {
    let tools = serde_json::to_vec(&request.tools).map_or(0, |bytes| bytes.len());
    let messages: usize = request
        .messages
        .iter()
        .map(|message| {
            message.content.len()
                + message.reasoning.as_ref().map_or(0, String::len)
                + message
                    .tool_calls
                    .iter()
                    .map(|call| call.name.len() + call.arguments.len())
                    .sum::<usize>()
        })
        .sum();
    ((tools + messages) as u64).div_ceil(4)
}

fn estimate_reply_tokens(reply: &ModelReply) -> u64 {
    let bytes = reply.text.len()
        + reply.reasoning.as_ref().map_or(0, String::len)
        + reply
            .tool_calls
            .iter()
            .map(|call| call.name.len() + call.arguments.len())
            .sum::<usize>();
    (bytes as u64).div_ceil(4).max(1)
}

/// Engine usage with the given counts and every other field at its default.
fn usage(input: u64, output: u64) -> ModelUsage {
    usage_from_json(&serde_json::json!({"input_tokens": input, "output_tokens": output}))
        .unwrap_or_default()
}

/// Engine usage from recorded JSON. Fields the engine's usage type does not
/// have are ignored and missing ones take their defaults, so traces recorded
/// before or after a usage field was added still replay.
pub fn usage_from_json(value: &Value) -> Option<ModelUsage> {
    let mut base = serde_json::to_value(ModelUsage::default()).ok()?;
    if let (Some(base), Some(recorded)) = (base.as_object_mut(), value.as_object()) {
        for (key, field) in recorded {
            if let Some(slot) = base.get_mut(key) {
                *slot = field.clone();
            }
        }
    }
    serde_json::from_value(base).ok()
}

#[cfg(test)]
mod tests;
