//! Small, explicit native tools. Paths are resolved and checked against the
//! project before any I/O; leaving the project needs an explicit [`Access`].

use super::{ToolBroker, code};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Condvar, LazyLock, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use xylo_core::{MemoryRecord, ToolDefinition, now_ms};
use xylo_services::sandbox::{self, Network};

/// What the permission decision granted one call.
#[derive(Clone, Copy, Debug)]
pub(super) struct Access {
    /// Paths outside the project were approved (or the mode allows them).
    pub(super) outside: bool,
    /// Commands run inside the OS sandbox.
    pub(super) sandboxed: bool,
    /// The command asked for, and was granted, internet access.
    pub(super) network: bool,
}

/// Told to the model when the sandbox blocked a command's connections.
fn network_hint(denied: &[String]) -> String {
    format!(
        "The sandbox blocked internet access to {}. If the command needs the network (package installs, downloads, git fetch, pull, clone or push), run it again with network: true and a justification.",
        denied.join(", ")
    )
}

/// The justification of a command that asks for internet access, or `None`
/// for an offline command. Malformed requests are rejected before any intent.
pub(super) fn network_request(args: &Value) -> Result<Option<&str>> {
    let requested = match args.get("network") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(requested)) => *requested,
        Some(_) => bail!("network must be true or false"),
    };
    if !requested {
        return Ok(None);
    }
    let justification = args
        .get("justification")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .context("A command that sets network: true needs a justification for the user")?;
    ensure!(
        justification.chars().count() <= 500,
        "Keep the justification under 500 characters"
    );
    Ok(Some(justification))
}

static NETWORK_PARAMETER: LazyLock<Value> = LazyLock::new(
    || json!({"type":"boolean","description":"Request internet access for this command: package installs, downloads, and git fetch, pull, clone or push. Requires justification"}),
);
static JUSTIFICATION_PARAMETER: LazyLock<Value> = LazyLock::new(
    || json!({"type":"string","maxLength":500,"description":"Required with network: one sentence telling the user why the command needs the internet"}),
);

/// Largest diff shown for approval; the write itself is never shortened.
const APPROVAL_DIFF_BYTES: usize = 128 * 1024;
const WRITE_BYTES: usize = 2 * 1024 * 1024;

pub(super) fn definitions() -> Vec<ToolDefinition> {
    let mut tools = vec![
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
            "Replace exact text in an existing file. old_string must match the current file exactly once, including whitespace and indentation: read the file first and copy the text without line-number prefixes. Set replace_all to change every occurrence. Prefer this to file_write for changing existing files",
            json!({"path":{"type":"string"},"old_string":{"type":"string","description":"Exact existing text to replace"},"new_string":{"type":"string","description":"Replacement text"},"replace_all":{"type":"boolean","description":"Replace every occurrence instead of requiring exactly one"}}),
            &["path", "old_string", "new_string"],
            true,
        ),
        def(
            "file_write",
            "Create a file, or replace a file's entire contents. For changes to an existing file use file_edit",
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
            "Run a program directly with an argument array (no shell) in the workspace sandbox, which has no internet access unless network is true. Non-zero exit codes are returned with stdout and stderr; inspect them and continue",
            json!({"program":{"type":"string"},"args":{"type":"array","items":{"type":"string"}},"timeout_seconds":{"type":"integer","minimum":1,"maximum":1200},"network":NETWORK_PARAMETER.clone(),"justification":JUSTIFICATION_PARAMETER.clone()}),
            &["program", "args"],
            true,
        ),
        def(
            "shell_command",
            "Run a command line through bash in the workspace sandbox, which has no internet access unless network is true. Use for pipes, redirection, chained commands and shell built-ins. Returns exit_code, stdout and stderr; a non-zero exit is reported, not hidden",
            json!({"command":{"type":"string"},"timeout_seconds":{"type":"integer","minimum":1,"maximum":1200},"network":NETWORK_PARAMETER.clone(),"justification":JUSTIFICATION_PARAMETER.clone()}),
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
    ];
    // The OS sandbox is not qualified on Windows, so commands are not offered.
    if cfg!(windows) {
        tools.retain(|tool| !matches!(tool.name.as_str(), "run_command" | "shell_command"));
    }
    tools
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

pub(super) fn text<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("Missing text parameter {key}"))
}

/// Where a requested path resolves.
pub(super) enum Resolved {
    /// Inside the project, or a saved full tool result.
    Project(PathBuf),
    /// Outside the project.
    Outside(PathBuf),
}

/// Canonicalizes `requested` (relative to the project) and says whether it
/// stays inside the project. A path being created resolves through its
/// existing parent, so a symlinked parent cannot hide the real target.
pub(super) fn resolve(broker: &ToolBroker, requested: &str, writing: bool) -> Result<Resolved> {
    let root = std::fs::canonicalize(&broker.request.workspace)?;
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
    if canonical.starts_with(&root) {
        return Ok(Resolved::Project(canonical));
    }
    // Reads may also open saved full tool results referenced by `full_result`.
    if !writing
        && let Ok(artifacts) = std::fs::canonicalize(&broker.artifacts)
        && canonical.starts_with(&artifacts)
        && canonical.is_file()
    {
        return Ok(Resolved::Project(canonical));
    }
    Ok(Resolved::Outside(canonical))
}

/// A resolved path the call may use under `access`.
fn permitted(
    broker: &ToolBroker,
    requested: &str,
    writing: bool,
    access: Access,
) -> Result<PathBuf> {
    match resolve(broker, requested, writing)? {
        Resolved::Project(path) => Ok(path),
        Resolved::Outside(path) if access.outside => Ok(path),
        Resolved::Outside(_) => bail!("Path leaves the selected workspace"),
    }
}

/// A writing path that is invalid is rejected before anything is changed.
/// Xylo's own data directory and executables, repository metadata and the
/// project's agent configuration are never writable by tools.
pub(super) fn write_target(
    broker: &ToolBroker,
    requested: &str,
    access: Access,
) -> Result<PathBuf> {
    // Git hooks and config run code at the next git command, and agent
    // directories hold instructions and permissions other tools trust; this
    // matches the command sandbox's `denyWrite` list. `.git` is refused at
    // any depth, since a nested repository runs its hooks just the same, the
    // agent directories at the project root like the sandbox. Names compare
    // ignoring ASCII case, as the default macOS and Windows file systems do.
    const PROTECTED_AT_ANY_DEPTH: &[&str] = &[".git"];
    const PROTECTED_AT_ROOT: &[&str] = &[".agents", ".claude", ".codex"];
    let target = permitted(broker, requested, true, access)
        .map_err(|error| code::unapplied(format!("{error:#}")))?;
    let root = std::fs::canonicalize(&broker.request.workspace)
        .map_err(|error| code::unapplied(format!("{error:#}")))?;
    // Outside the project every component counts, so another repository's
    // `.git` is refused even when outside writes are approved.
    let relative = target.strip_prefix(&root).unwrap_or(&target);
    let protected = |names: &[&str], component: std::path::Component| {
        names
            .iter()
            .any(|name| component.as_os_str().eq_ignore_ascii_case(name))
    };
    if relative
        .components()
        .any(|component| protected(PROTECTED_AT_ANY_DEPTH, component))
        || (target.starts_with(&root)
            && relative
                .components()
                .next()
                .is_some_and(|component| protected(PROTECTED_AT_ROOT, component)))
    {
        return Err(code::unapplied(format!(
            "Tools cannot modify repository metadata or agent configuration: {}",
            relative.display()
        )));
    }
    if let Some(data) = broker.artifacts.parent()
        && let Ok(data) = std::fs::canonicalize(data)
        && target.starts_with(data)
    {
        return Err(code::unapplied(
            "Tools cannot modify Xylo's application data",
        ));
    }
    xylo_services::sandbox::ensure_writable_target(&target)
        .map_err(|e| code::unapplied(e.to_string()))?;
    Ok(target)
}

/// Serializes writes to one file across every broker of the process. The
/// lock is held from preview to write, so a second edit plans against the
/// first edit's result instead of overwriting it.
pub(super) struct EditLock(PathBuf);

static EDITING: LazyLock<(Mutex<HashSet<PathBuf>>, Condvar)> =
    LazyLock::new(|| (Mutex::new(HashSet::new()), Condvar::new()));

impl EditLock {
    pub(super) fn acquire(path: &Path, cancel: &CancellationToken) -> Result<Self> {
        let (held, released) = &*EDITING;
        let mut paths = held.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        while paths.contains(path) {
            ensure!(
                !cancel.is_cancelled(),
                "Task cancelled while waiting to edit the file"
            );
            paths = released
                .wait_timeout(paths, Duration::from_millis(50))
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
        paths.insert(path.to_owned());
        Ok(Self(path.to_owned()))
    }
}

impl Drop for EditLock {
    fn drop(&mut self) {
        let (held, released) = &*EDITING;
        held.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.0);
        released.notify_all();
    }
}

/// The approval and start presentation of a file change: a bounded diff
/// against the current content. The request repeats only the path and
/// options, never the content, so the event stays small.
pub(super) fn change_preview(
    broker: &ToolBroker,
    name: &str,
    args: &Value,
    access: Access,
) -> Result<Value> {
    let root = &broker.request.workspace;
    let path = write_target(broker, text(args, "path")?, access)?;
    let shown = code::display(root, &path);
    let mut preview = match name {
        "file_edit" => {
            let plan = code::plan_edit(&path, &shown, args)?;
            let mut preview =
                code::approval_diff(&shown, &plan.original, &plan.updated, APPROVAL_DIFF_BYTES);
            preview["replacements"] = json!(plan.replacements);
            preview["request"] =
                json!({"path":text(args, "path")?,"replace_all":args.get("replace_all")});
            preview
        }
        _ => {
            let content = text(args, "content")?;
            ensure!(content.len() <= WRITE_BYTES, "Write exceeds 2 MiB");
            let existing = code::existing_text(&path);
            let mut preview = code::approval_diff(
                &shown,
                existing.as_deref().unwrap_or(""),
                content,
                APPROVAL_DIFF_BYTES,
            );
            preview["created"] = json!(!path.exists());
            preview["request"] = json!({"path":text(args, "path")?,"bytes":content.len()});
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

pub(super) fn execute(
    broker: &ToolBroker,
    name: &str,
    args: &Value,
    access: Access,
) -> Result<Value> {
    let root = &broker.request.workspace;
    let scope = root.to_string_lossy();
    let directory = || {
        permitted(
            broker,
            args.get("path").and_then(Value::as_str).unwrap_or("."),
            false,
            access,
        )
    };
    let value = match name {
        "file_read" => {
            let path = permitted(broker, text(args, "path")?, false, access)?;
            code::read(&path, &code::display(root, &path), args)?
        }
        "file_edit" => {
            let path = write_target(broker, text(args, "path")?, access)?;
            let shown = code::display(root, &path);
            let plan = code::plan_edit(&path, &shown, args)?;
            code::write_atomic(&path, &plan.updated)?;
            let mut result = code::diff(&shown, &plan.original, &plan.updated);
            result["path"] = json!(shown);
            result["replacements"] = json!(plan.replacements);
            result
        }
        "file_write" => {
            let path = write_target(broker, text(args, "path")?, access)?;
            let content = text(args, "content")?;
            if content.len() > WRITE_BYTES {
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
        "files_list" => code::list(&directory()?)?,
        "find_files" => code::find(root, &directory()?, args, &broker.cancel)?,
        "search_project" => code::search(root, &directory()?, args, &broker.cancel)?,
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
            let skills = xylo_services::LearningService::new(broker.store.clone())
                .skills(&root.to_string_lossy(), 100)?;
            json!({"promoted":skills.iter().filter(|skill|skill.state=="promoted").collect::<Vec<_>>(),"candidates":skills.iter().filter(|skill|skill.state!="promoted").collect::<Vec<_>>()})
        }
        "skill_read" => json!(
            xylo_services::LearningService::new(broker.store.clone())
                .active_skill(&root.to_string_lossy(), text(args, "name")?)?
        ),
        "skill_propose" => {
            json!({"queued_turn_id":xylo_services::LearningService::new(broker.store.clone()).queue_latest_completed(broker.request.session_id)?,"state":"background_evaluation"})
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
                    let path = permitted(
                        broker,
                        path,
                        false,
                        Access {
                            outside: false,
                            ..access
                        },
                    )?;
                    argv.push("--".into());
                    argv.push(path.to_string_lossy().into_owned());
                }
            }
            let result = broker
                .runtime
                .block_on(xylo_runtime::workspace::git::run_read(
                    root,
                    &argv,
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
            if cfg!(windows) {
                return Err(code::unapplied(
                    "Commands are not supported on Windows yet; nothing was run",
                ));
            }
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
            let protected = broker
                .artifacts
                .parent()
                .map(|p| vec![p.to_path_buf()])
                .unwrap_or_default();
            let program = program.to_string_lossy();
            let timeout = Duration::from_secs(timeout);
            let (result, denied) = broker.runtime.block_on(async {
                if access.sandboxed {
                    // An approved network request keeps every filesystem rule;
                    // only outbound connections to public hosts are opened.
                    let network = if access.network {
                        Network::Unrestricted
                    } else {
                        Network::None
                    };
                    let prepared = sandbox::prepare_coding_with_network(
                        &program,
                        &argv,
                        root,
                        &[],
                        &protected,
                        network,
                    )
                    .map_err(|error| {
                        anyhow::Error::new(xylo_runtime::process::NotStarted(format!("{error:#}")))
                    })?;
                    // Start failures and refused connections come from the
                    // launcher's private status file, never from the output.
                    let sandboxed = xylo_runtime::process::run_prepared_sandboxed(
                        prepared,
                        root,
                        timeout,
                        broker.cancel.child_token(),
                    )
                    .await?;
                    Ok::<_, anyhow::Error>((sandboxed.result, sandboxed.network_denials))
                } else {
                    let result = xylo_runtime::process::run(
                        &program,
                        &argv,
                        root,
                        timeout,
                        broker.cancel.child_token(),
                    )
                    .await?;
                    Ok((result, Vec::new()))
                }
            })?;
            let mut output = json!({
                "exit_code": result.code,
                "stdout": result.stdout,
                "stderr": result.stderr,
                "output_truncated": result.truncated,
            });
            let hint = (!denied.is_empty()).then(|| network_hint(&denied));
            if let Some(hint) = &hint {
                output["network_hint"] = json!(hint);
            }
            // A process that ran to completion has a known outcome, whatever its
            // exit code. Only the status distinguishes success from failure.
            if result.code != Some(0) {
                return Ok(json!({
                    "status":"failed",
                    "exit_code": result.code,
                    "summary": output,
                    "next_actions":[hint.as_deref().unwrap_or("Read stdout and stderr, fix the cause, then rerun")],
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
mod tests;
