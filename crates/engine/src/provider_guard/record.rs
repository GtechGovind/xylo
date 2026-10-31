//! The harness record kept beside every context summary.
//!
//! Summaries written by a model drop constraints, paths and dates, so the
//! application keeps what it can derive deterministically itself and appends
//! it to the summary message verbatim:
//! - **Pinned user messages.** The first user message (usually the task) and
//!   the latest [`RECENT_MESSAGES`] earlier ones are kept word for word, so
//!   user constraints never pass through the summarizer; the ones between are
//!   counted in an "omitted" line. Within the record budget the longest are
//!   excerpted (keeping both ends) and then the oldest recent ones dropped.
//! - **Artifact ledger.** Files read and changed, commands run with their
//!   outcome and the latest todo list, taken from the tool calls and results
//!   of the condensed history.
//!
//! The record is the tail of the summary message and is parsed back when the
//! next summary replaces it, so it accumulates across compactions and turns
//! without being rewritten by a model, and never grows past the budget the
//! caller gives [`Record::render`] (the window's input budget / 8).
//!
//! The record is found by its opening marker on a line of its own; the last
//! such line wins, so it is always the application's own record. Items are
//! JSON strings, one per line, with `<` and `>` escaped, which keeps
//! multi-line user text on one line, makes parsing exact and means no item
//! can forge a marker. Markers inside the model-written summary are
//! neutralised ([`neutralize_markers`]).

use crate::{
    model::{Message, Role},
    prompts::{self, RecordSections},
};
use serde_json::Value;

/// Ledger items kept per list; older items are dropped first.
const MAX_FILES: usize = 40;
const MAX_COMMANDS: usize = 20;
const MAX_TODOS: usize = 30;
/// Longest command or todo item kept.
const ITEM_CHARS: usize = 200;
/// No pinned message is cut below this many bytes.
const MIN_PINNED_BYTES: usize = 256;
/// Earlier user messages kept after the first one.
pub(crate) const RECENT_MESSAGES: usize = 6;

/// User messages and the artifact ledger, oldest first in every list.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Record {
    /// The first user message, then the most recent earlier ones.
    pub user_messages: Vec<String>,
    /// User messages dropped between the first and the recent ones.
    pub omitted_messages: usize,
    pub files_read: Vec<String>,
    pub files_changed: Vec<String>,
    pub commands: Vec<String>,
    /// The latest todo list, replaced (not merged) by each `todo_write`.
    pub todos: Vec<String>,
}

impl Record {
    /// The record at the end of a stored summary, if any.
    pub(crate) fn parse(summary: &str) -> Option<Self> {
        let (_, close, labels) = prompts::compaction_record_markers();
        let start = record_start(summary)?;
        let mut record = Record::default();
        let mut section = None;
        for line in summary[start..].lines().skip(1) {
            if line == close {
                break;
            }
            if let Some(index) = labels.iter().position(|label| *label == line) {
                section = Some(index);
                continue;
            }
            if section == Some(0)
                && let Some(count) = prompts::parse_compaction_record_omitted(line)
            {
                record.omitted_messages += count;
                continue;
            }
            let (Some(index), Some(item)) = (section, line.strip_prefix("- ")) else {
                continue;
            };
            let Ok(item) = serde_json::from_str::<String>(item) else {
                continue;
            };
            record.list(index).push(item);
        }
        Some(record)
    }

    /// The summary text before its record.
    pub(crate) fn strip(summary: &str) -> &str {
        record_start(summary).map_or(summary, |start| summary[..start].trim_end())
    }

    fn list(&mut self, index: usize) -> &mut Vec<String> {
        match index {
            0 => &mut self.user_messages,
            1 => &mut self.files_read,
            2 => &mut self.files_changed,
            3 => &mut self.commands,
            _ => &mut self.todos,
        }
    }

    /// Adds what `history` shows: user messages except `skip` (the current
    /// request, which stays a message of its own) and application notes, and
    /// the ledger entries of every tool call with a result.
    pub(crate) fn extend(&mut self, history: &[Message], skip: Option<usize>) {
        for (index, message) in history.iter().enumerate() {
            match message.role {
                Role::User if Some(index) != skip && !message.summary => {
                    if !is_application_note(&message.content) && !message.content.trim().is_empty()
                    {
                        self.user_messages.push(message.content.clone());
                    }
                }
                Role::Assistant => {
                    for call in &message.tool_calls {
                        let result = history[index + 1..]
                            .iter()
                            .take_while(|m| m.role == Role::Tool)
                            .find(|m| m.tool_call_id.as_deref() == Some(call.id.as_str()));
                        if let Some(result) = result {
                            self.add_call(&call.name, &call.arguments, &result.content);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn add_call(&mut self, name: &str, arguments: &str, result: &str) {
        let arguments = serde_json::from_str::<Value>(arguments).unwrap_or(Value::Null);
        let text = |key: &str| arguments.get(key).and_then(Value::as_str);
        let outcome = serde_json::from_str::<Value>(result).ok();
        let failed = outcome.as_ref().is_some_and(failed);
        match name {
            "file_read" => {
                if let Some(path) = text("path") {
                    push_unique(&mut self.files_read, path, MAX_FILES);
                }
            }
            "file_edit" | "file_write" if !failed => {
                if let Some(path) = text("path") {
                    push_unique(&mut self.files_changed, path, MAX_FILES);
                }
            }
            "shell_command" | "run_command" => {
                let command = match text("command") {
                    Some(command) => command.to_owned(),
                    None => std::iter::once(text("program").unwrap_or_default().to_owned())
                        .chain(
                            arguments
                                .get("args")
                                .and_then(Value::as_array)
                                .into_iter()
                                .flatten()
                                .filter_map(Value::as_str)
                                .map(str::to_owned),
                        )
                        .collect::<Vec<_>>()
                        .join(" "),
                };
                let status = match outcome.as_ref().and_then(|value| value.get("exit_code")) {
                    Some(code) if !code.is_null() => format!("exit {code}"),
                    _ if failed => "failed".to_owned(),
                    // Only results without a failure are ever masked.
                    _ if prompts::is_masked_observation(result) => "succeeded".to_owned(),
                    _ => "finished".to_owned(),
                };
                let entry = format!("{} → {status}", one_line(&command));
                self.commands.push(entry);
                let excess = self.commands.len().saturating_sub(MAX_COMMANDS);
                self.commands.drain(..excess);
            }
            "todo_write" => {
                if let Some(todos) = arguments.get("todos").and_then(Value::as_array) {
                    self.todos = todos
                        .iter()
                        .take(MAX_TODOS)
                        .map(|todo| {
                            let field = |key| todo.get(key).and_then(Value::as_str).unwrap_or("");
                            format!("[{}] {}", field("status"), one_line(field("content")))
                        })
                        .collect();
                }
            }
            _ => {}
        }
    }

    /// The record as text of at most `budget` bytes (unless even its fixed
    /// lines and one minimal message exceed it). Pinned user messages keep
    /// the first and the latest [`RECENT_MESSAGES`]; the ledger starts at a
    /// third of `budget`; then the oldest recent message, and after that the
    /// oldest ledger entry of the longest list, go until the text fits.
    pub(crate) fn render(&self, budget: usize) -> String {
        let mut record = self.clone();
        record.keep_recent_messages();
        record.trim_ledger(budget / 3);
        loop {
            let text = record.render_within(budget * 2 / 3);
            if text.len() <= budget || !record.shrink() {
                return text;
            }
        }
    }

    /// Keeps the first user message and the latest [`RECENT_MESSAGES`].
    fn keep_recent_messages(&mut self) {
        let excess = self.user_messages.len().saturating_sub(1 + RECENT_MESSAGES);
        if excess == 0 {
            return;
        }
        self.user_messages.drain(1..1 + excess);
        self.omitted_messages += excess;
    }

    /// Drops one more item: the oldest recent message, else the oldest entry
    /// of the longest ledger list. False when nothing is left to drop.
    fn shrink(&mut self) -> bool {
        if self.user_messages.len() > 1 {
            self.user_messages.remove(1);
            self.omitted_messages += 1;
            return true;
        }
        let used = self.ledger_bytes();
        used > 0 && {
            self.trim_ledger(used - 1);
            true
        }
    }

    fn render_within(&self, message_budget: usize) -> String {
        let lines = |items: &[String]| -> String {
            if items.is_empty() {
                return "- (none)".to_owned();
            }
            items
                .iter()
                .map(|item| item_line(item))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let pinned = pinned(&self.user_messages, message_budget);
        let mut user_messages = Vec::new();
        if let Some((first, recent)) = pinned.split_first() {
            user_messages.push(item_line(first));
            if self.omitted_messages > 0 {
                user_messages.push(prompts::compaction_record_omitted(self.omitted_messages));
            }
            user_messages.extend(recent.iter().map(|item| item_line(item)));
        } else {
            user_messages.push("- (none)".to_owned());
        }
        prompts::compaction_record(&RecordSections {
            user_messages: &user_messages.join("\n"),
            files_read: &lines(&self.files_read),
            files_changed: &lines(&self.files_changed),
            commands: &lines(&self.commands),
            todos: &lines(&self.todos),
        })
    }

    fn ledger_bytes(&self) -> usize {
        [
            &self.files_read,
            &self.files_changed,
            &self.commands,
            &self.todos,
        ]
        .iter()
        .map(|list| ledger_size(list))
        .sum()
    }

    /// Drops the oldest ledger entries, longest list first, until the ledger
    /// fits `budget` bytes.
    fn trim_ledger(&mut self, budget: usize) {
        let size = ledger_size;
        loop {
            let lists = [
                &mut self.files_read,
                &mut self.files_changed,
                &mut self.commands,
                &mut self.todos,
            ];
            let total: usize = lists.iter().map(|list| size(list)).sum();
            if total <= budget {
                return;
            }
            let Some(longest) = lists.into_iter().max_by_key(|list| size(list)) else {
                return;
            };
            if longest.is_empty() {
                return;
            }
            longest.remove(0);
        }
    }
}

fn ledger_size(list: &[String]) -> usize {
    list.iter().map(|item| item.len() + 4).sum()
}

/// One list item: `- ` and the item as a JSON string with `<` and `>`
/// escaped, so no item can contain a record marker.
fn item_line(item: &str) -> String {
    let json = serde_json::to_string(item).unwrap_or_default();
    format!("- {}", json.replace('<', "\\u003c").replace('>', "\\u003e"))
}

/// Where the application's record starts: the last line that is exactly the
/// opening marker.
fn record_start(summary: &str) -> Option<usize> {
    let (open, _, _) = prompts::compaction_record_markers();
    let mut offset = 0;
    let mut start = None;
    for line in summary.split_inclusive('\n') {
        if line.trim_end_matches(['\n', '\r']) == open {
            start = Some(offset);
        }
        offset += line.len();
    }
    start
}

/// `text` (a model-written summary) with record markers made inert, so it
/// cannot open or close a record of its own.
pub(crate) fn neutralize_markers(text: &str) -> String {
    let (open, close, _) = prompts::compaction_record_markers();
    let inert = |marker: &str| marker.replace('<', "[").replace('>', "]");
    text.replace(close, &inert(close))
        .replace(open, &inert(open))
}

/// User messages fitted to `budget` bytes. Long messages are excerpted
/// (keeping both ends) rather than dropped, so no instruction disappears.
fn pinned(messages: &[String], budget: usize) -> Vec<String> {
    let total: usize = messages.iter().map(String::len).sum();
    if total <= budget || messages.is_empty() {
        return messages.to_vec();
    }
    // Share the budget so short messages stay whole and long ones are cut to
    // the same ceiling.
    let mut lengths: Vec<usize> = messages.iter().map(String::len).collect();
    lengths.sort_unstable();
    let mut remaining = budget;
    let mut ceiling = MIN_PINNED_BYTES;
    for (index, length) in lengths.iter().enumerate() {
        let share = remaining / (lengths.len() - index);
        if *length > share {
            ceiling = share.max(MIN_PINNED_BYTES);
            break;
        }
        remaining -= length;
        ceiling = *length;
    }
    messages
        .iter()
        .map(|message| super::context_excerpt(message, ceiling))
        .collect()
}

fn push_unique(list: &mut Vec<String>, item: &str, limit: usize) {
    list.retain(|existing| existing != item);
    list.push(item.to_owned());
    let excess = list.len().saturating_sub(limit);
    list.drain(..excess);
}

fn one_line(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(ITEM_CHARS) {
        Some((cut, _)) => format!("{}…", &flat[..cut]),
        None => flat,
    }
}

/// Whether a tool result reports failure (the same signals the broker uses).
fn failed(value: &Value) -> bool {
    value.get("success").and_then(Value::as_bool) == Some(false)
        || value.get("isError").and_then(Value::as_bool) == Some(true)
        || value
            .get("exit_code")
            .and_then(Value::as_i64)
            .is_some_and(|code| code != 0)
        || value
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| {
                matches!(
                    status,
                    "error"
                        | "failed"
                        | "failure"
                        | "incomplete"
                        | "cancelled"
                        | "timeout"
                        | "outcome_unknown"
                )
            })
}

/// Whether a tool result reports failure; failures stay in context.
pub(crate) fn failed_result(content: &str) -> bool {
    serde_json::from_str::<Value>(content).is_ok_and(|value| failed(&value))
}

/// Notes the application inserts (permission changes, step limits) are not
/// user words and are not pinned.
fn is_application_note(content: &str) -> bool {
    let note = prompts::application_note("");
    let prefix = note.lines().next().unwrap_or_default();
    !prefix.is_empty() && content.trim_start().starts_with(prefix)
}

#[cfg(test)]
mod tests;
