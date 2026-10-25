//! Small, explicit native tools. Workspace boundaries are checked before I/O.

use super::ToolBroker;
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
            "Delegate a bounded subtask to a supervised child using this task's provider and permissions",
            json!({"task":{"type":"string"},"context":{"type":"string"}}),
            &["task"],
            false,
        ),
        def(
            "file_read",
            "Read UTF-8 text inside the selected workspace",
            json!({"path":{"type":"string"}}),
            &["path"],
            false,
        ),
        def(
            "file_write",
            "Atomically write UTF-8 text inside the workspace after approval",
            json!({"path":{"type":"string"},"content":{"type":"string"}}),
            &["path", "content"],
            true,
        ),
        def(
            "files_list",
            "List a workspace directory",
            json!({"path":{"type":"string"}}),
            &[],
            false,
        ),
        def(
            "search_project",
            "Search workspace text with ripgrep",
            json!({"query":{"type":"string"}}),
            &["query"],
            false,
        ),
        def(
            "git_status",
            "Inspect Git worktree status",
            json!({}),
            &[],
            false,
        ),
        def(
            "git_diff",
            "Read unstaged Git diff without invoking external diff tools",
            json!({}),
            &[],
            false,
        ),
        def(
            "run_command",
            "Execute a program and argument array in the workspace after approval",
            json!({"program":{"type":"string"},"args":{"type":"array","items":{"type":"string"}},"timeout_seconds":{"type":"integer","minimum":1,"maximum":300}}),
            &["program", "args"],
            true,
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
            "Discover tool schemas by relevant keywords",
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
        let parent = std::fs::canonicalize(path.parent().context("Missing parent")?)?;
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

pub(super) fn execute(broker: &ToolBroker, name: &str, args: &Value) -> Result<Value> {
    let root = &broker.request.workspace;
    let scope = root.to_string_lossy();
    let value = match name {
        "file_read" => {
            let path = workspace_path(root, text(args, "path")?, false)?;
            ensure!(
                std::fs::metadata(&path)?.len() <= 2 * 1024 * 1024,
                "File exceeds 2 MiB; use project search for a narrower excerpt"
            );
            json!(std::fs::read_to_string(&path).context("File is not readable UTF-8 text")?)
        }
        "file_write" => {
            let path = workspace_path(root, text(args, "path")?, true)?;
            let content = text(args, "content")?;
            ensure!(content.len() <= 2 * 1024 * 1024, "Write exceeds 2 MiB");
            let temp = path.with_file_name(format!(".workbench-{}.tmp", Uuid::new_v4()));
            use std::io::Write;
            let result = (|| -> Result<()> {
                let mut file = std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&temp)?;
                file.write_all(content.as_bytes())?;
                file.sync_all()?;
                std::fs::rename(&temp, &path)?;
                Ok(())
            })();
            if result.is_err() {
                let _ = std::fs::remove_file(&temp);
            }
            result?;
            json!({"path":path,"bytes":content.len()})
        }
        "files_list" => {
            let path = workspace_path(
                root,
                args.get("path").and_then(Value::as_str).unwrap_or("."),
                false,
            )?;
            let mut entries: Vec<_> = std::fs::read_dir(path)?
                .take(2000)
                .map(|entry| entry.map(|e| e.file_name().to_string_lossy().into_owned()))
                .collect::<std::io::Result<_>>()?;
            entries.sort_unstable();
            json!(entries)
        }
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
        "git_status" | "git_diff" | "search_project" | "run_command" => {
            let (program, argv, timeout) = match name {
                "git_status" => (
                    "git".to_owned(),
                    vec!["status".into(), "--short".into()],
                    30,
                ),
                "git_diff" => (
                    "git".to_owned(),
                    vec![
                        "diff".into(),
                        "--no-ext-diff".into(),
                        "--no-textconv".into(),
                    ],
                    30,
                ),
                "search_project" => (
                    "rg".to_owned(),
                    vec![
                        "--line-number".into(),
                        "--max-count".into(),
                        "100".into(),
                        "--".into(),
                        text(args, "query")?.into(),
                        ".".into(),
                    ],
                    30,
                ),
                _ => {
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
                    (
                        text(args, "program")?.to_owned(),
                        argv,
                        args.get("timeout_seconds")
                            .and_then(Value::as_u64)
                            .unwrap_or(120)
                            .clamp(1, 300),
                    )
                }
            };
            let result = broker.runtime.block_on(workbench_runtime::process::run(
                &program,
                &argv,
                root,
                Duration::from_secs(timeout),
                broker.cancel.child_token(),
            ))?;
            ensure!(
                result.code == Some(0) || (name == "search_project" && result.code == Some(1)),
                "Process exited {:?}: {}",
                result.code,
                result.stderr
            );
            json!(result)
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
