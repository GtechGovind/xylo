//! SSH and existing-container execution through explicit native executables.

use crate::{CapabilityConfig, definition, observation, process, required_str};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, time::Duration};
use tokio_util::sync::CancellationToken;
use workbench_core::{ToolDefinition, ToolObservation};

fn shell_quote(arg: &str) -> Result<String> {
    ensure!(
        !arg.contains('\0') && !arg.contains('\n') && !arg.contains('\r'),
        "Invalid remote argument"
    );
    Ok(format!("'{}'", arg.replace('\'', "'\\''")))
}

pub async fn execute(
    config: &CapabilityConfig,
    args: &Value,
    cancel: CancellationToken,
) -> Result<ToolObservation> {
    let executable = PathBuf::from(required_str(args, "executable")?);
    let kind = required_str(args, "kind")?;
    let target = required_str(args, "target")?;
    ensure!(
        !target.starts_with('-')
            && target.len() <= 255
            && target
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-@:".contains(c)),
        "Invalid remote target"
    );
    let command: Vec<String> =
        serde_json::from_value(args.get("command").cloned().unwrap_or(Value::Null))?;
    ensure!(
        !command.is_empty() && command.len() <= 128,
        "Command must contain 1..128 argument strings"
    );
    let mut argv: Vec<String> = match kind {
        "ssh" => {
            let port = args.get("port").and_then(Value::as_u64).unwrap_or(22);
            ensure!((1..=65535).contains(&port), "Invalid SSH port");
            vec![
                "-T".into(),
                "-oBatchMode=yes".into(),
                "-oConnectTimeout=15".into(),
                "-oStrictHostKeyChecking=yes".into(),
                "-p".into(),
                port.to_string(),
                "--".into(),
                target.into(),
            ]
        }
        "container" => vec!["exec".into(), target.into()],
        _ => anyhow::bail!("kind must be ssh or container"),
    };
    if kind == "ssh" {
        argv.push(
            command
                .iter()
                .map(|s| shell_quote(s))
                .collect::<Result<Vec<_>>>()?
                .join(" "),
        );
    } else {
        argv.extend(command);
    }
    let result = process::run(
        process::ProcessSpec {
            executable,
            args: argv,
            cwd: config.workspace.clone(),
            env: BTreeMap::new(),
            timeout: Duration::from_secs(config.operation_timeout_seconds),
            output_limit: config.max_output_bytes,
        },
        cancel,
    )
    .await?;
    let mut obs = observation(serde_json::to_value(&result)?, vec![]);
    if !result.success {
        obs.status = "failed".into();
    }
    Ok(obs)
}

pub fn definitions() -> Vec<ToolDefinition> {
    vec![definition(
        "remote_execute",
        "Execute a command using a native SSH client or docker/podman exec. SSH requires existing verified host keys and noninteractive credentials. No containers are created.",
        json!({"kind":{"type":"string","enum":["ssh","container"]},"executable":{"type":"string","description":"Absolute path of native ssh, docker or podman executable"},"target":{"type":"string"},"port":{"type":"integer","minimum":1,"maximum":65535},"command":{"type":"array","minItems":1,"maxItems":128,"items":{"type":"string"}}}),
        &["kind", "executable", "target", "command"],
        true,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ssh_arguments_are_single_quoted() {
        assert_eq!(
            shell_quote("a'$(touch /tmp/x)").unwrap(),
            "'a'\\''$(touch /tmp/x)'"
        );
        assert!(shell_quote("a\nb").is_err());
    }
}
