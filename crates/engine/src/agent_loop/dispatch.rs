//! Turning a model's tool calls into executed, ordered tool results.
//!
//! Every call is validated before dispatch: malformed arguments and unknown
//! tool names become tool error results the model can correct, never turn
//! failures. A small exact alias table maps common foreign tool names to
//! Xylo's; anything else points the model at `search_tools`/`call_tool`.
//! Read-only calls run concurrently (bounded); a writing call waits for every
//! earlier call and runs alone, so calls that touch the same state apply in
//! the order the model wrote them.
use crate::model::{Message, ToolCall, ToolSchema};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::task::JoinHandle;

/// Most read-only calls in flight at once.
pub(crate) const MAX_PARALLEL_READS: usize = 4;

/// Whether a tool may change anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Access {
    ReadOnly,
    Writes,
}

/// The tools a loop may call. Execution is blocking and exactly-once per
/// call id: the implementation journals every dispatch.
pub(crate) trait ToolExecutor: Send + Sync {
    /// Tools offered directly to the model right now. Re-read before every
    /// model call, because the permission mode can change mid-task.
    fn schemas(&self) -> Vec<ToolSchema>;
    /// Registered tools that are not offered directly (found via search).
    fn deferred_count(&self) -> usize;
    /// `None` when `name` is not a tool this loop may call.
    fn access(&self, name: &str, arguments: &Value) -> Option<Access>;
    /// Runs one call and returns the model-facing result text.
    fn execute(&self, call_id: &str, name: &str, arguments: Value) -> anyhow::Result<String>;
}

/// Exact names other agents use for Xylo's tools.
const ALIASES: [(&str, &str); 9] = [
    ("read", "file_read"),
    ("write", "file_write"),
    ("edit", "file_edit"),
    ("bash", "shell_command"),
    ("shell", "shell_command"),
    ("grep", "search_project"),
    ("glob", "find_files"),
    ("ls", "files_list"),
    ("todo", "todo_write"),
];

/// A call ready to run, or the error result the model receives instead.
pub(crate) enum Prepared {
    Run {
        name: String,
        arguments: Value,
        access: Access,
    },
    Reject(String),
}

/// The model-facing text of a tool error.
pub(crate) fn error_result(summary: impl Into<String>) -> String {
    json!({"status":"error","summary":summary.into()}).to_string()
}

/// Validates `call` against `tools`.
pub(crate) fn prepare(call: &ToolCall, tools: &dyn ToolExecutor) -> Prepared {
    let arguments = match serde_json::from_str::<Value>(&call.arguments) {
        Ok(value) if value.is_object() => value,
        Ok(_) => {
            return Prepared::Reject(error_result(format!(
                "The arguments for `{}` must be a JSON object. Send the call again with an object of named arguments.",
                call.name
            )));
        }
        Err(error) => {
            return Prepared::Reject(error_result(format!(
                "The arguments for `{}` are not valid JSON ({error}). Send the call again with a JSON object.",
                call.name
            )));
        }
    };
    let alias = ALIASES
        .iter()
        .find(|(alias, _)| *alias == call.name)
        .map(|(_, name)| *name);
    let resolved = tools
        .access(&call.name, &arguments)
        .map(|access| (call.name.clone(), access))
        .or_else(|| {
            let name = alias?;
            tools
                .access(name, &arguments)
                .map(|access| (name.to_owned(), access))
        });
    match resolved {
        Some((name, access)) => Prepared::Run {
            name,
            arguments,
            access,
        },
        None => Prepared::Reject(error_result(format!(
            "There is no tool named `{}`. Use search_tools to find the right tool and call_tool to run it.",
            call.name
        ))),
    }
}

/// Runs prepared calls and returns their results in call order.
pub(crate) async fn run_batch(
    tools: Arc<dyn ToolExecutor>,
    calls: Vec<(String, Prepared)>,
) -> Vec<Message> {
    let mut results: Vec<Option<String>> = vec![None; calls.len()];
    let mut ids = Vec::with_capacity(calls.len());
    let mut running: Vec<(usize, JoinHandle<String>)> = Vec::new();
    for (index, (id, prepared)) in calls.into_iter().enumerate() {
        ids.push(id.clone());
        let (name, arguments, access) = match prepared {
            Prepared::Reject(result) => {
                results[index] = Some(result);
                continue;
            }
            Prepared::Run {
                name,
                arguments,
                access,
            } => (name, arguments, access),
        };
        if access == Access::Writes || running.len() >= MAX_PARALLEL_READS {
            let limit = if access == Access::Writes {
                0
            } else {
                MAX_PARALLEL_READS - 1
            };
            while running.len() > limit {
                let (slot, handle) = running.remove(0);
                results[slot] = Some(joined(handle).await);
            }
        }
        let executor = tools.clone();
        let handle = tokio::task::spawn_blocking(move || {
            executor
                .execute(&id, &name, arguments)
                .unwrap_or_else(|error| error_result(format!("{error:#}")))
        });
        if access == Access::Writes {
            results[index] = Some(joined(handle).await);
        } else {
            running.push((index, handle));
        }
    }
    for (slot, handle) in running {
        results[slot] = Some(joined(handle).await);
    }
    ids.into_iter()
        .zip(results)
        .map(|(id, result)| Message::tool_result(id, result.unwrap_or_default()))
        .collect()
}

async fn joined(handle: JoinHandle<String>) -> String {
    handle
        .await
        .unwrap_or_else(|error| error_result(format!("The tool stopped unexpectedly: {error}")))
}
