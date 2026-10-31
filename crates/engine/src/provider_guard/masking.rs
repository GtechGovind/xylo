//! Observation masking: the first, deterministic tier of context compaction.
//!
//! Old tool *results* are replaced by one-line stubs while the agent's own
//! messages and tool calls stay intact. Masking runs long before the summary
//! threshold and costs no model call.
//!
//! Invariants:
//! - **Only successful tool results are masked.** User messages, assistant
//!   text, tool calls and results that report failure are never touched, and
//!   no result of the newest tool round is masked.
//! - **Durable and replayable.** A plan is applied to the conversation the
//!   loop checkpoints, not to one request, so every later request and every
//!   resume replays identical bytes and a stub is never masked again.
//! - **Batched.** Nothing is masked until history passes
//!   [`MASK_SHARE_PERCENT`] of the window, and then only when the batch frees
//!   at least [`BATCH_SHARE_PERCENT`] of the input allowance, so the cached
//!   prompt prefix breaks rarely. Under [`Pressure::Overflow`] (the provider
//!   rejected the request as too long) both gates are skipped and only the
//!   newest round is kept.
//! - **Per-tool retention.** A read that a later identical read supersedes,
//!   or whose file was changed afterwards, is masked first; other results
//!   once they are older than their tool's retention in tool rounds: short
//!   for listings, searches and web content, longer for edits and helper
//!   reports.
//! - **Recoverable.** A stub names the tool, its key argument, the call id
//!   and the original size, and says how to get the content back: the saved
//!   full result, a fresh read, or repeating the call.

use super::{ContextBudget, Pressure, record::failed_result};
use crate::{
    model::{Message, Role, ToolSchema},
    prompts::{self, MaskReason},
};
use serde_json::Value;
use std::collections::HashMap;

/// History share of the window at which masking starts.
pub(crate) const MASK_SHARE_PERCENT: u32 = 55;
/// Smallest batch worth breaking the cached prefix for, as a share of the
/// input allowance.
pub(crate) const BATCH_SHARE_PERCENT: usize = 20;
/// Results smaller than this are cheaper to keep than to replace.
const MIN_MASK_BYTES: usize = 1024;

/// Tool rounds a result stays in context after the round that produced it.
fn retention(tool: &str) -> usize {
    match tool {
        "file_edit" | "file_write" | "agent_delegate" | "todo_write" => 10,
        "file_read" | "extension_skill" | "extension_resource" | "skill_read" => 6,
        "shell_command" | "run_command" | "git_diff" => 4,
        "search_project" | "find_files" | "files_list" | "git_status" | "search_tools"
        | "memory_search" => 3,
        name if name.starts_with("browser_")
            || name.starts_with("web_")
            || name.starts_with("connector_") =>
        {
            2
        }
        _ => 4,
    }
}

/// Tool results to replace with stubs, by history index.
#[derive(Debug)]
pub(crate) struct MaskPlan {
    pub(crate) stubs: Vec<(usize, String)>,
}

impl MaskPlan {
    /// Replaces the planned results in `history`.
    pub(crate) fn apply(self, history: &mut [Message]) {
        for (index, stub) in self.stubs {
            history[index].content = stub;
        }
    }
}

/// One tool call as masking sees it.
struct Call<'a> {
    round: usize,
    name: &'a str,
    arguments: Value,
    /// Whether the result reports failure; parsed once per result.
    failed: bool,
}

impl ContextBudget {
    /// The results of `history` to mask now, or `None` when masking would
    /// not run (below the threshold or too small a batch under normal
    /// pressure, or nothing eligible).
    pub(crate) fn mask_plan(
        &self,
        system: &[Message],
        history: &[Message],
        tools: &[ToolSchema],
        max_tokens: u32,
        pressure: Pressure,
    ) -> Option<MaskPlan> {
        let used =
            self.costs(system).iter().sum::<usize>() + self.costs(history).iter().sum::<usize>();
        let mask_context = self.window.context() / 100 * MASK_SHARE_PERCENT;
        if pressure == Pressure::Normal && used <= self.allowance(tools, max_tokens, mask_context) {
            return None;
        }
        let calls = calls(history);
        let rounds = calls
            .iter()
            .flatten()
            .map(|call| call.round + 1)
            .max()
            .unwrap_or(0);
        let mut stubs = Vec::new();
        let mut saved = 0usize;
        for (index, message) in history.iter().enumerate() {
            if message.role != Role::Tool
                || message.content.len() < MIN_MASK_BYTES
                || prompts::is_masked_observation(&message.content)
            {
                continue;
            }
            let (Some(id), Some(call)) = (message.tool_call_id.as_deref(), &calls[index]) else {
                continue;
            };
            if call.failed {
                continue;
            }
            let age = rounds - 1 - call.round;
            if age == 0 {
                continue;
            }
            let keep = match pressure {
                Pressure::Normal => retention(call.name),
                Pressure::Overflow => 1,
            };
            let path = saved_path(&message.content);
            let reason = match superseded(history, index, call, &calls) {
                Some(reason) => reason,
                None if age >= keep => path.as_deref().map_or(MaskReason::Aged, MaskReason::Saved),
                None => continue,
            };
            let stub =
                prompts::masked_observation(&describe(call, id), message.content.len(), reason);
            let mut masked = message.clone();
            masked.content = stub.clone();
            saved += self
                .message_cost(message)
                .saturating_sub(self.message_cost(&masked));
            stubs.push((index, stub));
        }
        if stubs.is_empty() || saved == 0 {
            return None;
        }
        let allowance = self.allowance(tools, max_tokens, self.window.context());
        if pressure == Pressure::Normal && saved * 100 < allowance * BATCH_SHARE_PERCENT {
            return None;
        }
        Some(MaskPlan { stubs })
    }
}

/// The call each tool result in `history` answers, by history index, with
/// its round (the index of the assistant message among those that called
/// tools). A result answers a call of the assistant message just before its
/// run of results, so ids reused by a later turn never mix rounds.
fn calls(history: &[Message]) -> Vec<Option<Call<'_>>> {
    let mut calls = Vec::with_capacity(history.len());
    let mut current: HashMap<&str, &crate::model::ToolCall> = HashMap::new();
    let mut round = 0;
    for message in history {
        match message.role {
            Role::Assistant if !message.tool_calls.is_empty() => {
                current = message
                    .tool_calls
                    .iter()
                    .map(|call| (call.id.as_str(), call))
                    .collect();
                round += 1;
                calls.push(None);
            }
            Role::Tool => calls.push(
                message
                    .tool_call_id
                    .as_deref()
                    .and_then(|id| current.get(id))
                    .map(|call| Call {
                        round: round - 1,
                        name: call.name.as_str(),
                        arguments: serde_json::from_str(&call.arguments).unwrap_or(Value::Null),
                        failed: failed_result(&message.content),
                    }),
            ),
            _ => {
                current.clear();
                calls.push(None);
            }
        }
    }
    calls
}

/// Why the `file_read` result at `index` is stale: a later identical read,
/// or a later successful edit or write of the same file.
fn superseded(
    history: &[Message],
    index: usize,
    call: &Call,
    calls: &[Option<Call>],
) -> Option<MaskReason<'static>> {
    if call.name != "file_read" {
        return None;
    }
    let path = call.arguments.get("path")?.as_str()?;
    calls[index + 1..history.len()]
        .iter()
        .flatten()
        .filter(|later| !later.failed)
        .find_map(|later| {
            let same_path = later.arguments.get("path").and_then(Value::as_str) == Some(path);
            match later.name {
                "file_read" if same_path && later.arguments == call.arguments => {
                    Some(MaskReason::ReadSuperseded)
                }
                "file_edit" | "file_write" if same_path => Some(MaskReason::ReadChanged),
                _ => None,
            }
        })
}

/// The saved complete result a shortened tool result points to.
fn saved_path(content: &str) -> Option<String> {
    serde_json::from_str::<Value>(content)
        .ok()?
        .get("full_result")?
        .as_str()
        .map(str::to_owned)
}

/// `name `key argument` [id]` for a stub.
fn describe(call: &Call, id: &str) -> String {
    let argument = ["path", "command", "query", "pattern", "url", "name"]
        .iter()
        .find_map(|key| call.arguments.get(*key).and_then(Value::as_str))
        .map(str::to_owned)
        .or_else(|| {
            let program = call.arguments.get("program")?.as_str()?;
            let args = call
                .arguments
                .get("args")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str);
            Some(
                std::iter::once(program)
                    .chain(args)
                    .collect::<Vec<_>>()
                    .join(" "),
            )
        });
    match argument {
        Some(argument) => {
            let flat = argument.split_whitespace().collect::<Vec<_>>().join(" ");
            let shown: String = flat.chars().take(120).collect();
            let ellipsis = if shown.len() < flat.len() { "…" } else { "" };
            format!("{} `{shown}{ellipsis}` [{id}]", call.name)
        }
        None => format!("{} [{id}]", call.name),
    }
}

#[cfg(test)]
mod tests;
