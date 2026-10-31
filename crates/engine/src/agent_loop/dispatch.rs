//! Turning a model's tool calls into executed, ordered tool results.
//!
//! Every call is checked before dispatch, and so before any permission
//! decision or approval prompt: arguments are parsed leniently, repaired
//! where the tool's JSON Schema makes the intent unambiguous, and validated
//! against that schema (see [`arguments`]). `call_tool` targets and
//! `connector_call` targets with a discovered schema are checked the same
//! way. Malformed arguments and unknown names become tool error results that
//! say what to fix, with the nearest real names, never turn failures. A small
//! exact alias table maps common foreign tool names to Xylo's.
//!
//! A repaired call is recorded in the assistant message as it will run
//! ([`record`]), so the transcript, the journal and a resumed turn all see
//! the same arguments.
//!
//! Read-only calls run concurrently (bounded); a writing call waits for every
//! earlier call and runs alone, so calls that touch the same state apply in
//! the order the model wrote them.
mod arguments;

use crate::{
    model::{Message, ToolCall, ToolSchema},
    tools::{catalog, lookup::CompiledSchema},
};
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
    /// The JSON Schema of `name`'s arguments, compiled once and reused;
    /// `None` leaves them unchecked.
    fn parameters(&self, _name: &str) -> Option<Arc<CompiledSchema>> {
        None
    }
    /// Every tool name this loop may call, offered or deferred, for
    /// suggestions when the model names one that does not exist.
    fn names(&self) -> Vec<String> {
        self.schemas()
            .into_iter()
            .map(|schema| schema.name)
            .collect()
    }
    /// The discovered tools, by name with their compiled schemas, of the
    /// connector a `connector_call` names; `None` when the connector is
    /// unknown or not yet discovered, which the executor reports itself.
    fn connector_tools(&self, _arguments: &Value) -> Option<Vec<(String, Arc<CompiledSchema>)>> {
        None
    }
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

/// Parses, repairs and validates `call` against `tools`.
pub(crate) fn prepare(call: &ToolCall, tools: &dyn ToolExecutor) -> Prepared {
    let arguments = match arguments::parse(&call.arguments) {
        Ok(arguments) => arguments,
        Err(problem) => {
            tracing::info!(tool = %call.name, "Tool call rejected: unparseable arguments");
            return Prepared::Reject(error_result(format!(
                "The arguments for `{}` {problem}.",
                call.name
            )));
        }
    };
    let Some(name) = resolve(&call.name, &arguments, tools) else {
        tracing::info!(tool = %call.name, "Tool call rejected: unknown tool");
        return Prepared::Reject(error_result(unknown_tool(&call.name, tools)));
    };
    let arguments = match check(&name, arguments, tools) {
        Ok(arguments) => arguments,
        Err(problem) => {
            tracing::info!(tool = %name, "Tool call rejected: arguments do not match the schema");
            return Prepared::Reject(error_result(problem));
        }
    };
    match tools.access(&name, &arguments) {
        Some(access) => Prepared::Run {
            name,
            arguments,
            access,
        },
        None => Prepared::Reject(error_result(unknown_tool(&call.name, tools))),
    }
}

/// Records in `assistant` the call as it will run: the resolved tool name and,
/// when they were repaired, the arguments.
pub(crate) fn record(assistant: &mut Message, call: &ToolCall, prepared: &Prepared) {
    let Prepared::Run {
        name, arguments, ..
    } = prepared
    else {
        return;
    };
    if *name != call.name {
        assistant.rename_tool_call(&call.id, name);
    }
    let unchanged =
        serde_json::from_str::<Value>(&call.arguments).is_ok_and(|original| original == *arguments);
    if !unchanged {
        tracing::info!(tool = %name, "Tool call arguments repaired");
        // Call ids are unique within a turn by the time calls are prepared.
        if let Some(position) = assistant
            .tool_calls
            .iter()
            .position(|recorded| recorded.id == call.id)
        {
            assistant.set_tool_call_arguments(position, arguments);
        }
    }
}

/// The tool `requested` names: itself, or the Xylo tool an alias stands for.
fn resolve(requested: &str, arguments: &Value, tools: &dyn ToolExecutor) -> Option<String> {
    if tools.access(requested, arguments).is_some() {
        return Some(requested.to_owned());
    }
    let (_, name) = ALIASES.iter().find(|(alias, _)| *alias == requested)?;
    tools
        .access(name, arguments)
        .is_some()
        .then(|| (*name).to_owned())
}

fn unknown_tool(requested: &str, tools: &dyn ToolExecutor) -> String {
    let names = tools.names();
    let suggestions = catalog::nearest(requested, names.iter().map(String::as_str));
    let mut message = format!("There is no tool named `{requested}`.");
    if !suggestions.is_empty() {
        message.push_str(&format!(
            " Did you mean {}?",
            arguments::quoted(&suggestions)
        ));
    }
    if requested.contains("__") {
        message.push_str(
            " Connector tools run through connector_call with the connector_id and tool that search_tools returns.",
        );
    } else {
        message.push_str(" Use search_tools to find the right tool and call_tool to run it.");
    }
    message
}

/// Repairs and validates `arguments` for `name`, then the target of a
/// `call_tool` or `connector_call`.
fn check(name: &str, mut arguments: Value, tools: &dyn ToolExecutor) -> Result<Value, String> {
    if let Some(schema) = tools.parameters(name) {
        conform(name, &mut arguments, &schema)?;
    }
    match name {
        "call_tool" => check_call_tool(arguments, tools),
        "connector_call" => check_connector_call(arguments, tools),
        _ => Ok(arguments),
    }
}

fn conform(tool: &str, arguments: &mut Value, schema: &CompiledSchema) -> Result<(), String> {
    arguments::repair(arguments, schema.schema());
    arguments::validate(arguments, schema).map_err(|problems| {
        format!(
            "The arguments for `{tool}` do not match its schema:\n- {}\nExpected arguments: {}. Send the call again with corrected arguments.",
            problems.join("\n- "),
            catalog::signature(schema.schema())
        )
    })
}

fn check_call_tool(mut arguments: Value, tools: &dyn ToolExecutor) -> Result<Value, String> {
    let requested = arguments
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if requested == "call_tool" {
        return Err("call_tool cannot run call_tool; name the target tool directly.".into());
    }
    let nested = arguments
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let Some(target) = resolve(&requested, &nested, tools) else {
        return Err(unknown_tool(&requested, tools));
    };
    let nested = check(&target, nested, tools)
        .map_err(|problem| format!("call_tool `{target}`: {problem}"))?;
    arguments["name"] = Value::String(target);
    arguments["arguments"] = nested;
    Ok(arguments)
}

fn check_connector_call(mut arguments: Value, tools: &dyn ToolExecutor) -> Result<Value, String> {
    let Some(discovered) = tools.connector_tools(&arguments) else {
        return Ok(arguments);
    };
    let requested = arguments
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let Some((_, schema)) = discovered.iter().find(|(name, _)| *name == requested) else {
        let suggestions =
            catalog::nearest(&requested, discovered.iter().map(|(name, _)| name.as_str()));
        let hint = if suggestions.is_empty() {
            "Use search_tools or connector_tools to see its tools.".to_owned()
        } else {
            format!("Did you mean {}?", arguments::quoted(&suggestions))
        };
        return Err(format!(
            "This connector has no tool named `{requested}`. {hint}"
        ));
    };
    let mut nested = arguments
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    conform(&requested, &mut nested, schema)?;
    arguments["arguments"] = nested;
    Ok(arguments)
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

#[cfg(test)]
mod tests;
