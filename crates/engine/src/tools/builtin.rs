//! Small, explicit native tools. Workspace boundaries are checked before I/O.

use super::{ToolBroker, code};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;
use workbench_core::{MemoryRecord, ToolDefinition, now_ms};

pub(super) fn definitions() -> Vec<ToolDefinition> {
    vec![
        def(
            "agent_delegate",
            "Delegate a bounded, self-contained subtask (for example a broad search or an independent investigation) to a supervised child agent with this task's provider and permissions. Give it full context; it returns its findings",
            json!({"task":{"type":"string"},"context":{"type":"string"}}),
            &["task"],
            false,
        ),
        def(
            "file_read",
            "Read a text file in the workspace as numbered lines (cat -n style). Returns up to 2000 lines from offset (1-based); when truncated is true, continue from next_offset. Line-number prefixes are not part of the file. Also reads a saved full tool result named in full_result",
            json!({"path":{"type":"string","description":"Workspace-relative or absolute path"},"offset":{"type":"integer","minimum":1,"description":"First line to read (1-based)"},"limit":{"type":"integer","minimum":1,"maximum":10000,"description":"Maximum lines to return"}}),
            &["path"],
            false,
        ),
        def(
            "file_edit",
            "Replace exact text in an existing workspace file after approval. old_string must match the current file exactly once, including whitespace and indentation: read the file first and copy the text without line-number prefixes. Set replace_all to change every occurrence. Prefer this to file_write for changing existing files",
            json!({"path":{"type":"string"},"old_string":{"type":"string","description":"Exact existing text to replace"},"new_string":{"type":"string","description":"Replacement text"},"replace_all":{"type":"boolean","description":"Replace every occurrence instead of requiring exactly one"}}),
            &["path", "old_string", "new_string"],
            true,
        ),
        def(
            "file_write",
            "Create a file, or replace a file's entire contents, inside the workspace after approval. For changes to an existing file use file_edit",
            json!({"path":{"type":"string"},"content":{"type":"string","description":"Complete new file contents"}}),
            &["path", "content"],
            true,
        ),
        def(
            "files_list",
            "List one workspace directory; directory names end with /",
            json!({"path":{"type":"string","description":"Directory, default the workspace root"}}),
            &[],
            false,
        ),
        def(
            "find_files",
            "Find workspace files by glob pattern such as **/*.rs or src/**/test_*.py (a pattern without / matches at any depth). Respects .gitignore; newest files first",
            json!({"pattern":{"type":"string"},"path":{"type":"string","description":"Directory to search, default the workspace root"},"max_results":{"type":"integer","minimum":1,"maximum":5000}}),
            &["pattern"],
            false,
        ),
        def(
            "search_project",
            "Search file contents with a regular expression (ripgrep engine), respecting .gitignore. Returns path:line:text rows. Use glob to filter files (for example *.ts or src/**/*.rs), path to limit to a directory or file, literal for exact text, context for surrounding lines and files_only to list matching files",
            json!({"query":{"type":"string","description":"Regular expression, or exact text when literal is true"},"path":{"type":"string"},"glob":{"type":"string","description":"Comma-separated glob filters"},"literal":{"type":"boolean"},"case_insensitive":{"type":"boolean"},"context":{"type":"integer","minimum":0,"maximum":10},"max_results":{"type":"integer","minimum":1,"maximum":2000},"files_only":{"type":"boolean"}}),
            &["query"],
            false,
        ),
        def(
            "git_status",
            "Inspect Git worktree status and the current branch",
            json!({}),
            &[],
            false,
        ),
        def(
            "git_diff",
            "Read the Git diff of uncommitted changes without invoking external diff tools. Set staged for the index, or path to limit to one file or directory",
            json!({"staged":{"type":"boolean"},"path":{"type":"string"}}),
            &[],
            false,
        ),
        def(
            "run_command",
            "Run a program directly with an argument array (no shell) in the workspace after approval. Non-zero exit codes are returned with stdout and stderr; inspect them and continue",
            json!({"program":{"type":"string"},"args":{"type":"array","items":{"type":"string"}},"timeout_seconds":{"type":"integer","minimum":1,"maximum":1200}}),
            &["program", "args"],
            true,
        ),
        def(
            "shell_command",
            "Run a command line through the platform shell in the workspace after approval. Use for pipes, redirection, chained commands and shell built-ins. Returns exit_code, stdout and stderr; a non-zero exit is reported, not hidden",
            json!({"command":{"type":"string"},"timeout_seconds":{"type":"integer","minimum":1,"maximum":1200}}),
            &["command"],
            true,
        ),
        def(
            "todo_write",
            "Keep a visible checklist for multi-step work. Send the complete list every time; keep exactly one item in_progress while working and mark items completed as soon as they are done",
            json!({"todos":{"type":"array","items":{"type":"object","properties":{"content":{"type":"string"},"status":{"type":"string","enum":["pending","in_progress","completed"]}},"required":["content","status"],"additionalProperties":false}}}),
            &["todos"],
            false,
        ),
        def(
            "memory_search",
            "Recall project-scoped facts with source references",
            json!({"query":{"type":"string"}}),
            &["query"],
            false,
        ),
        def(
            "memory_remember",
            "Retain a sourced project fact or correction",
            json!({"content":{"type":"string"},"source":{"type":"string"}}),
            &["content", "source"],
            true,
        ),
        def(
            "skill_list",
            "List promoted skills and candidate versions",
            json!({}),
            &[],
            false,
        ),
        def(
            "connector_list",
            "List configured service and MCP connectors",
            json!({}),
            &[],
            false,
        ),
        def(
            "connector_tools",
            "Discover real tools for a configured connector; local executable startup requires approval",
            json!({"connector_id":{"type":"string"}}),
            &["connector_id"],
            true,
        ),
        def(
            "connector_call",
            "Execute a discovered connector tool with its actual permission classification",
            json!({"connector_id":{"type":"string"},"tool":{"type":"string"},"arguments":{"type":"object"}}),
            &["connector_id", "tool", "arguments"],
            true,
        ),
        def(
            "skill_read",
            "Read a promoted procedural skill",
            json!({"name":{"type":"string"}}),
            &["name"],
            false,
        ),
        def(
            "skill_propose",
            "Queue the latest verified completed turn for background procedural evaluation; requires independent later evidence before promotion",
            json!({}),
            &[],
            true,
        ),
        def(
            "search_tools",
            "Discover additional tool schemas (documents, browser, environments, images, connectors) by relevant keywords",
            json!({"query":{"type":"string"}}),
            &["query"],
            false,
        ),
        def(
            "call_tool",
            "Execute a discovered tool with its documented parameters and normal permissions",
            json!({"name":{"type":"string"},"arguments":{"type":"object"}}),
            &["name", "arguments"],
            false,
        ),
    ]
}

fn def(
    name: &str,
    description: &str,
    properties: Value,
    required: &[&str],
    writes: bool,
) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: description.into(),
        writes,
        parameters: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
    }
}

pub(super) fn handles(name: &str) -> bool {
    definitions().iter().any(|d| d.name == name)
}

fn text<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("Missing text parameter {key}"))
}

pub(crate) fn workspace_path(root: &Path, requested: &str, writing: bool) -> Result<PathBuf> {
    let root = std::fs::canonicalize(root)?;
    let path = if Path::new(requested).is_absolute() {
        PathBuf::from(requested)
    } else {
        root.join(requested)
    };
    let canonical = if path.exists() {
        std::fs::canonicalize(&path)?
    } else if writing {
        let parent = std::fs::canonicalize(path.parent().context("Missing parent")?)
            .context("The parent directory does not exist; create it first")?;
        parent.join(path.file_name().context("Missing filename")?)
    } else {
        bail!("Path does not exist: {}", path.display());
    };
    ensure!(
        canonical.starts_with(&root),
        "Path leaves the selected workspace"
    );
    Ok(canonical)
}

/// Reads may also open saved full tool results referenced by `full_result`.
fn readable_path(broker: &ToolBroker, requested: &str) -> Result<PathBuf> {
    let error = match workspace_path(&broker.request.workspace, requested, false) {
        Ok(path) => return Ok(path),
        Err(error) => error,
    };
    let saved = Path::new(requested);
    if saved.is_absolute()
        && let (Ok(saved), Ok(artifacts)) = (
            std::fs::canonicalize(saved),
            std::fs::canonicalize(&broker.artifacts),
        )
        && saved.starts_with(&artifacts)
        && saved.is_file()
    {
        return Ok(saved);
    }
    Err(error)
}

/// A writing path that is invalid is rejected before anything is changed.
pub(super) fn write_target(root: &Path, requested: &str) -> Result<PathBuf> {
    workspace_path(root, requested, true).map_err(|error| code::unapplied(format!("{error:#}")))
}

/// Approval presentation for file changes: the path and an actual diff rather
/// than the full payload. Invalid edits are rejected before an intent exists.
pub(super) fn change_preview(root: &Path, name: &str, args: &Value) -> Result<Value> {
    let path = write_target(root, text(args, "path")?)?;
    let shown = code::display(root, &path);
    let mut preview = match name {
        "file_edit" => {
            let plan = code::plan_edit(&path, &shown, args)?;
            let mut preview = code::diff(&shown, &plan.original, &plan.updated);
            preview["replacements"] = json!(plan.replacements);
            preview
        }
        _ => {
            let content = text(args, "content")?;
            ensure!(content.len() <= 2 * 1024 * 1024, "Write exceeds 2 MiB");
            let existing = code::existing_text(&path);
            let mut preview = code::diff(&shown, existing.as_deref().unwrap_or(""), content);
            preview["created"] = json!(!path.exists());
            preview
        }
    };
    preview["path"] = json!(shown);
    Ok(preview)
}

fn todo_list(args: &Value) -> Result<Value> {
    let todos = args
        .get("todos")
        .and_then(Value::as_array)
        .context("todos must be an array")?;
    ensure!(
        todos.len() <= 100,
        "Keep the checklist to 100 items or fewer"
    );
    let mut lines = Vec::new();
    for item in todos {
        let content = item
            .get("content")
            .and_then(Value::as_str)
            .filter(|c| !c.trim().is_empty())
            .context("Each todo needs content")?;
        let mark = match item.get("status").and_then(Value::as_str) {
            Some("completed") => "[x]",
            Some("in_progress") => "[~]",
            Some("pending") => "[ ]",
            _ => bail!("Todo status must be pending, in_progress or completed"),
        };
        lines.push(format!("{mark} {content}"));
    }
    Ok(json!({"checklist": lines.join("\n"), "items": todos.len()}))
}

pub(super) fn execute(broker: &ToolBroker, name: &str, args: &Value) -> Result<Value> {
    let root = &broker.request.workspace;
    let scope = root.to_string_lossy();
    let value = match name {
        "file_read" => {
            let path = readable_path(broker, text(args, "path")?)?;
            code::read(&path, &code::display(root, &path), args)?
        }
        "file_edit" => {
            let path = write_target(root, text(args, "path")?)?;
            let shown = code::display(root, &path);
            let plan = code::plan_edit(&path, &shown, args)?;
            code::write_atomic(&path, &plan.updated)?;
            let mut result = code::diff(&shown, &plan.original, &plan.updated);
            result["path"] = json!(shown);
            result["replacements"] = json!(plan.replacements);
            result
        }
        "file_write" => {
            let path = write_target(root, text(args, "path")?)?;
            let content = text(args, "content")?;
            if content.len() > 2 * 1024 * 1024 {
                return Err(code::unapplied("Write exceeds 2 MiB"));
            }
            let shown = code::display(root, &path);
            let existing = code::existing_text(&path);
            let created = !path.exists();
            code::write_atomic(&path, content)?;
            let mut result = code::diff(&shown, existing.as_deref().unwrap_or(""), content);
            result["path"] = json!(shown);
            result["bytes"] = json!(content.len());
            result["created"] = json!(created);
            result
        }
        "files_list" => code::list(&workspace_path(
            root,
            args.get("path").and_then(Value::as_str).unwrap_or("."),
            false,
        )?)?,
        "find_files" => {
            let base = workspace_path(
                root,
                args.get("path").and_then(Value::as_str).unwrap_or("."),
                false,
            )?;
            code::find(root, &base, args, &broker.cancel)?
        }
        "search_project" => {
            let base = workspace_path(
                root,
                args.get("path").and_then(Value::as_str).unwrap_or("."),
                false,
            )?;
            code::search(root, &base, args, &broker.cancel)?
        }
        "todo_write" => todo_list(args)?,
        "memory_search" => json!(
            broker
                .memory
                .recall_sync(&scope, text(args, "query")?, 12)?
        ),
        "memory_remember" => {
            let record = MemoryRecord {
                id: Uuid::new_v4(),
                scope: scope.into_owned(),
                content: text(args, "content")?.into(),
                source: text(args, "source")?.into(),
                created_at: now_ms(),
                supersedes: None,
            };
            broker.memory.remember_sync(record.clone())?;
            json!(record)
        }
        "skill_list" => {
            let skills = workbench_services::LearningService::new(broker.store.clone())
                .skills(&root.to_string_lossy(), 100)?;
            json!({"promoted":skills.iter().filter(|skill|skill.state=="promoted").collect::<Vec<_>>(),"candidates":skills.iter().filter(|skill|skill.state!="promoted").collect::<Vec<_>>()})
        }
        "skill_read" => json!(
            workbench_services::LearningService::new(broker.store.clone())
                .active_skill(&root.to_string_lossy(), text(args, "name")?)?
        ),
        "skill_propose" => {
            json!({"queued_turn_id":workbench_services::LearningService::new(broker.store.clone()).queue_latest_completed(broker.request.session_id)?,"state":"background_evaluation"})
        }
        "git_status" | "git_diff" => {
            let mut argv: Vec<String> = if name == "git_status" {
                vec!["status".into(), "--short".into(), "--branch".into()]
            } else {
                vec![
                    "diff".into(),
                    "--no-ext-diff".into(),
                    "--no-textconv".into(),
                ]
            };
            if name == "git_diff" {
                if args.get("staged").and_then(Value::as_bool) == Some(true) {
                    argv.push("--cached".into());
                }
                if let Some(path) = args.get("path").and_then(Value::as_str) {
                    let path = workspace_path(root, path, false)?;
                    argv.push("--".into());
                    argv.push(path.to_string_lossy().into_owned());
                }
            }
            let git = code::resolve_program("git", root)?;
            let result = broker.runtime.block_on(workbench_runtime::process::run(
                &git.to_string_lossy(),
                &argv,
                root,
                Duration::from_secs(30),
                broker.cancel.child_token(),
            ))?;
            ensure!(
                result.code == Some(0),
                "git exited {:?}: {}",
                result.code,
                result.stderr
            );
            json!({"output": if result.stdout.is_empty() { "No changes".to_owned() } else { result.stdout }, "truncated": result.truncated})
        }
        "run_command" | "shell_command" => {
            let (program, argv) = if name == "shell_command" {
                let command = text(args, "command")?;
                ensure!(!command.trim().is_empty(), "command is empty");
                code::shell_invocation(command, root)?
            } else {
                let argv: Vec<String> = args
                    .get("args")
                    .and_then(Value::as_array)
                    .context("args must be an array")?
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .map(str::to_owned)
                            .context("Each argument must be a string")
                    })
                    .collect::<Result<_>>()?;
                (code::resolve_program(text(args, "program")?, root)?, argv)
            };
            let timeout = args
                .get("timeout_seconds")
                .and_then(Value::as_u64)
                .unwrap_or(120)
                .clamp(1, 1200);
            let result = broker.runtime.block_on(workbench_runtime::process::run(
                &program.to_string_lossy(),
                &argv,
                root,
                Duration::from_secs(timeout),
                broker.cancel.child_token(),
            ))?;
            let output = json!({
                "exit_code": result.code,
                "stdout": result.stdout,
                "stderr": result.stderr,
                "output_truncated": result.truncated,
            });
            // A process that ran to completion has a known outcome, whatever its
            // exit code. Only the status distinguishes success from failure.
            if result.code != Some(0) {
                return Ok(json!({
                    "status":"failed",
                    "exit_code": result.code,
                    "summary": output,
                    "next_actions":["Read stdout and stderr, fix the cause, then rerun"],
                    "artifacts":[]
                }));
            }
            output
        }
        _ => bail!("Unimplemented built-in tool {name}"),
    };
    Ok(json!({"status":"success","summary":value,"next_actions":[],"artifacts":[]}))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn paths_cannot_escape_through_parent_or_symlink() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("escape")).unwrap();
        assert!(workspace_path(workspace.path(), "../outside", true).is_err());
        assert!(workspace_path(workspace.path(), "escape/file", true).is_err());
        assert!(workspace_path(workspace.path(), "new.rs", true).is_ok());
    }
}
