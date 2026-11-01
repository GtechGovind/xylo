//! Sandboxed command hooks and validated lifecycle control output.
use crate::settings_files::{EffectiveSettings, PermissionDecision};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

/// Events delivered by the harness and portable plugins.
pub const EVENTS: &[&str] = &[
    "PreToolUse",
    "PostToolUse",
    "UserPromptSubmit",
    "Stop",
    "SubagentStop",
    "SessionStart",
    "SessionEnd",
    "PreCompact",
    "Notification",
    "PermissionRequest",
];
/// Validated control fields; arbitrary output never acquires execution authority.
#[derive(Clone, Debug, Default)]
pub struct HookOutput {
    pub decision: Option<PermissionDecision>,
    pub updated_input: Option<Value>,
    pub context: Vec<String>,
    pub stop_reason: Option<String>,
    pub block_reason: Option<String>,
}
impl HookOutput {
    /// Parses both Claude's hook-specific form and the flat form.
    pub fn parse(value: &Value) -> Result<Self> {
        ensure!(value.is_object(), "Hook output must be an object");
        let field = |name: &str| {
            value
                .get("hookSpecificOutput")
                .and_then(|v| v.get(name))
                .or_else(|| value.get(name))
        };
        let mut output = Self::default();
        if let Some(decision) = field("permissionDecision").or_else(|| value.get("decision")) {
            output.decision = match decision
                .as_str()
                .context("Hook decision must be a string")?
            {
                "allow" | "approve" => Some(PermissionDecision::Allow),
                "deny" => Some(PermissionDecision::Deny),
                "ask" => Some(PermissionDecision::Ask),
                "block" => {
                    output.block_reason = Some(
                        value
                            .get("reason")
                            .and_then(Value::as_str)
                            .unwrap_or("The stop hook requested more work")
                            .into(),
                    );
                    None
                }
                _ => bail!("Unknown hook decision"),
            };
        }
        if let Some(input) = field("updatedInput").or_else(|| field("updatedToolInput")) {
            ensure!(input.is_object(), "Hook updatedInput must be an object");
            output.updated_input = Some(input.clone());
        }
        if let Some(context) = field("additionalContext") {
            output.context.push(
                context
                    .as_str()
                    .context("Hook additionalContext must be a string")?
                    .to_owned(),
            );
        }
        if value.get("continue") == Some(&Value::Bool(false)) {
            output.stop_reason = Some(
                value
                    .get("stopReason")
                    .and_then(Value::as_str)
                    .unwrap_or("Stopped by a configured hook")
                    .into(),
            );
        }
        if output.decision == Some(PermissionDecision::Deny) && output.block_reason.is_none() {
            output.block_reason = field("permissionDecisionReason")
                .or_else(|| value.get("reason"))
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        Ok(output)
    }
    /// Combines controls without letting a later allow erase an earlier denial.
    pub fn merge(&mut self, other: Self) {
        fn rank(value: Option<PermissionDecision>) -> u8 {
            match value {
                Some(PermissionDecision::Deny) => 3,
                Some(PermissionDecision::Ask) => 2,
                Some(PermissionDecision::Allow) => 1,
                None => 0,
            }
        }
        if rank(other.decision) > rank(self.decision) {
            self.decision = other.decision;
        }
        if other.updated_input.is_some() {
            self.updated_input = other.updated_input;
        }
        self.context.extend(other.context);
        self.stop_reason = self.stop_reason.take().or(other.stop_reason);
        self.block_reason = self.block_reason.take().or(other.block_reason);
    }
}
/// One configured command and its bounded deadline.
#[derive(Clone, Debug)]
pub struct CommandHook {
    pub command: String,
    pub timeout: Duration,
}
/// Regular expressions match canonical names and their Claude aliases.
pub fn matches(matcher: Option<&str>, name: &str) -> Result<bool> {
    let Some(matcher) = matcher.filter(|m| !m.is_empty() && *m != "*") else {
        return Ok(true);
    };
    let regex = regex::Regex::new(matcher).context("Invalid hook matcher regular expression")?;
    let alias = match name {
        "shell_command" | "run_command" => "Bash",
        "file_read" => "Read",
        "file_edit" => "Edit",
        "file_write" => "Write",
        "web_fetch" => "WebFetch",
        "find_files" | "files_list" => "Glob",
        "search_project" => "Grep",
        _ => name,
    };
    Ok(regex.is_match(name) || regex.is_match(alias))
}
/// Lists matching commands from a fresh effective settings snapshot.
pub fn configured(
    settings: &EffectiveSettings,
    event: &str,
    name: &str,
) -> Result<Vec<CommandHook>> {
    configured_alias(settings, event, name, None)
}
/// Matches either the host dispatcher name or a resolved MCP tool name.
pub fn configured_alias(
    settings: &EffectiveSettings,
    event: &str,
    name: &str,
    alias: Option<&str>,
) -> Result<Vec<CommandHook>> {
    ensure!(EVENTS.contains(&event), "Unknown hook event");
    if settings.value.get("disableAllHooks") == Some(&Value::Bool(true)) {
        return Ok(Vec::new());
    }
    let Some(groups) = settings.value.get("hooks").and_then(|h| h.get(event)) else {
        return Ok(Vec::new());
    };
    let mut hooks = Vec::new();
    for group in groups.as_array().context("Hook event must be an array")? {
        let matcher = group
            .get("matcher")
            .map(|value| value.as_str().context("Hook matcher must be a string"))
            .transpose()?;
        if !matches(matcher, name)?
            && !alias
                .map(|alias| matches(matcher, alias))
                .transpose()?
                .unwrap_or(false)
        {
            continue;
        }
        let commands = group
            .get("hooks")
            .and_then(Value::as_array)
            .context("Hook group must contain hooks")?;
        for command in commands {
            ensure!(
                command
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("command")
                    == "command",
                "Only command hooks are supported"
            );
            let script = command
                .get("command")
                .and_then(Value::as_str)
                .context("Hook command is required")?;
            ensure!(
                !script.trim().is_empty() && script.len() <= 64 * 1024,
                "Invalid hook command"
            );
            hooks.push(CommandHook {
                command: script.into(),
                timeout: Duration::from_secs(
                    command
                        .get("timeout")
                        .and_then(Value::as_u64)
                        .unwrap_or(30)
                        .clamp(1, 300),
                ),
            });
        }
    }
    ensure!(hooks.len() <= 128, "Too many hooks for one event");
    Ok(hooks)
}
struct OutputTask(tokio::task::AbortHandle);
impl Drop for OutputTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}
/// Runs offline inside the project sandbox with bounded JSON input and output.
pub async fn run(
    command: &CommandHook,
    input: &Value,
    workspace: &Path,
    protected: &[PathBuf],
    cancel: CancellationToken,
) -> Result<Value> {
    let bytes = serde_json::to_vec(input)?;
    ensure!(bytes.len() <= 1024 * 1024, "Hook input exceeds 1 MiB");
    ensure!(!cancel.is_cancelled(), "Hook cancelled before execution");
    let mut protected = protected.to_vec();
    protected.extend([workspace.join(".xylo"), workspace.join(".claude")]);
    let mut prepared = crate::sandbox::prepare_coding(
        "/bin/sh",
        &["-c".into(), command.command.clone()],
        workspace,
        &[],
        &protected,
    )?;
    crate::sandbox::set_child_input(&mut prepared, &bytes)?;
    prepared
        .command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(false);
    #[cfg(unix)]
    prepared.command.process_group(0);
    let mut child = prepared.command.spawn().context("Start sandboxed hook")?;
    let owner = child.stdin.take().context("Hook owner pipe missing")?;
    let stdout = child.stdout.take().context("Hook stdout missing")?;
    let output = tokio::spawn(async move {
        let mut output = Vec::new();
        stdout
            .take(1024 * 1024 + 1)
            .read_to_end(&mut output)
            .await?;
        ensure!(output.len() <= 1024 * 1024, "Hook output exceeds 1 MiB");
        Ok::<_, anyhow::Error>(output)
    });
    let _output_guard = OutputTask(output.abort_handle());
    // Only the anchored watchdog may signal this group. Helper stdin remains
    // the owner pipe; the command receives its JSON through a separate file.
    let status = tokio::select! {
        _ = cancel.cancelled() => Err(anyhow::anyhow!("Hook execution cancelled; effects may be incomplete")),
        result = tokio::time::timeout(command.timeout, child.wait()) =>
            result.context("Hook execution timed out; effects may be incomplete")
                .and_then(|result| result.context("Wait for sandboxed hook")),
    };
    drop(owner);
    let reaped = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    let cleanup_path = prepared.status_file.with_extension("cleanup");
    crate::sandbox::wait_for_guard_cleanup(&cleanup_path).await?;
    reaped.context("Sandboxed hook did not retire")??;
    let captured = tokio::time::timeout(Duration::from_secs(2), output).await;
    let status = status?;
    let output = captured.context("Hook output pipes did not close")???;
    if let Some(failure) = crate::sandbox::read_start_status(&prepared.status_file) {
        bail!("{failure}");
    }
    let code = crate::sandbox::read_guard_exit_code(&cleanup_path).or_else(|| status.code());
    if code == Some(2) {
        return Ok(json!({"decision":"deny"}));
    }
    ensure!(code == Some(0), "Hook command failed (exit {code:?})");
    if output.is_empty() {
        return Ok(json!({}));
    }
    let value: Value = serde_json::from_slice(&output).context("Hook output must be JSON")?;
    HookOutput::parse(&value)?;
    Ok(value)
}

#[cfg(all(test, unix))]
mod tests;
