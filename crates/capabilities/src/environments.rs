//! Application-owned native environments. Management helpers are deliberately
//! absent from the agent tool catalogue; execution remains an approved write.
mod archive;
mod capture;
mod capture_fs;
mod native;
mod operations;
mod storage;
#[cfg(test)]
mod tests;

use crate::{CapabilityConfig, definition, observation, process};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use storage::{Manager, Operation};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workbench_core::*;

/// The requested command was rejected by local preflight validation, after its
/// operation lease and prior receipts were checked, before a durable receipt
/// write or command dispatch was attempted. The private constructor prevents callers from
/// treating transport, lease or persistence failures as definitive rejection.
#[derive(Debug)]
pub struct CommandNotDispatched(anyhow::Error);
impl std::fmt::Display for CommandNotDispatched {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, output)
    }
}
impl std::error::Error for CommandNotDispatched {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    environment_id: Uuid,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationIdentity {
    operation_id: Uuid,
    environment_id: Option<Uuid>,
    action: Option<EnvironmentAction>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Resolution {
    operation_id: Uuid,
    succeeded: bool,
    note: String,
}

fn token(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}
fn credential_file(path: &Path, known_hosts: bool) -> Result<PathBuf> {
    ensure!(
        path.is_absolute(),
        "Choose an absolute SSH credential file path"
    );
    let path = path
        .canonicalize()
        .context("The selected SSH credential file is missing")?;
    let metadata = std::fs::metadata(&path)?;
    ensure!(
        metadata.is_file() && metadata.len() > 0 && metadata.len() <= 4 * 1024 * 1024,
        "The SSH credential file is empty, invalid or too large"
    );
    let text = path
        .to_str()
        .context("SSH credential paths must be UTF-8")?;
    ensure!(
        !text.contains(['\0', '\r', '\n', '%']),
        "SSH credential path contains unsupported characters"
    );
    if !known_hosts {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            ensure!(
                metadata.permissions().mode() & 0o077 == 0,
                "The private SSH identity file must be readable only by its owner"
            );
        }
    }
    Ok(path)
}
fn normalize(mut profile: EnvironmentProfile) -> Result<EnvironmentProfile> {
    ensure!(
        !profile.id.is_nil()
            && !profile.name.trim().is_empty()
            && profile.name.len() <= 160
            && !profile.name.contains(['\0', '\r', '\n']),
        "Choose a short environment name and a valid identity"
    );
    match &mut profile.target {
        EnvironmentTarget::Ssh {
            executable,
            host,
            user,
            port,
            known_hosts,
            identity_file,
        } => {
            *executable = process::validate_executable(executable)?;
            ensure!(
                !host.is_empty()
                    && host.len() <= 253
                    && !host.starts_with('-')
                    && host
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b".:-".contains(&byte))
                    && token(user, 64)
                    && *port > 0,
                "Choose an explicit SSH host, user and port without client options"
            );
            *known_hosts = credential_file(known_hosts, true)?;
            if let Some(identity) = identity_file {
                *identity = credential_file(identity, false)?;
            }
        }
        EnvironmentTarget::Container {
            executable,
            container,
            ..
        } => {
            *executable = process::validate_executable(executable)?;
            match container {
                ContainerTarget::Existing { target } => ensure!(
                    token(target, 128),
                    "Choose an existing container name or immutable ID"
                ),
                ContainerTarget::Managed { image, command } => {
                    ensure!(
                        !image.is_empty()
                            && image.len() <= 512
                            && !image.starts_with('-')
                            && image
                                .bytes()
                                .all(|byte| byte.is_ascii_alphanumeric()
                                    || b"._-/:@".contains(&byte)),
                        "Choose an explicit locally available image"
                    );
                    native::validate_command(command)?;
                }
            }
        }
    }
    ensure!(
        serde_json::to_vec(&profile)?.len() <= 32 * 1024,
        "Environment profile exceeds its size limit"
    );
    Ok(profile)
}
fn clients() -> Value {
    let mut result = Vec::new();
    for (runtime, name) in [("ssh", "ssh"), ("docker", "docker"), ("podman", "podman")] {
        let name = if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.into()
        };
        let candidate = std::env::var_os("PATH")
            .into_iter()
            .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
            .map(|directory| directory.join(&name))
            .find_map(|path| process::validate_executable(&path).ok());
        if let Some(executable) = candidate {
            result.push(json!({"runtime":runtime,"executable":executable,"available":true}));
        }
    }
    json!({"clients":result})
}
pub async fn execute_management(
    config: &CapabilityConfig,
    name: &str,
    args: &Value,
    cancel: CancellationToken,
) -> Result<Value> {
    ensure!(
        serde_json::to_vec(args)?.len() <= 128 * 1024,
        "Environment management request exceeds its byte limit"
    );
    if name == "helper_environment_clients" {
        let _: Empty = serde_json::from_value(args.clone())?;
        return Ok(clients());
    }
    let manager = Manager::open(config, &cancel).await?;
    match name {
        "helper_environment_list" => {
            let _: Empty = serde_json::from_value(args.clone())?;
            list(&manager, &cancel).await
        }
        "helper_environment_status" => {
            let request: Identity = serde_json::from_value(args.clone())?;
            Ok(serde_json::to_value(
                status(&manager, request.environment_id, cancel).await?,
            )?)
        }
        "helper_environment_save" => {
            operations::save(&manager, serde_json::from_value(args.clone())?, cancel)
                .await
                .and_then(|value| Ok(serde_json::to_value(value)?))
        }
        "helper_environment_operation" => {
            let request: OperationIdentity = serde_json::from_value(args.clone())?;
            ensure!(
                request.environment_id.is_some() == request.action.is_some(),
                "Provide the original environment and action together when checking admission"
            );
            Ok(serde_json::to_value(
                operations::operation_status(
                    &manager,
                    request.operation_id,
                    request.environment_id.zip(request.action),
                    cancel,
                )
                .await?,
            )?)
        }
        "helper_environment_operations" => {
            let request: Identity = serde_json::from_value(args.clone())?;
            let mut receipts = operation_summaries(manager.operations(request.environment_id)?);
            receipts.extend(archive::recent(&manager, request.environment_id)?);
            Ok(json!({"operations":receipts}))
        }
        "helper_environment_maintenance" => {
            let _: Empty = serde_json::from_value(args.clone())?;
            manager.maintain(1024, &cancel).await
        }
        "helper_environment_resolve" => {
            let request: Resolution = serde_json::from_value(args.clone())?;
            Ok(serde_json::to_value(
                operations::resolve(
                    &manager,
                    request.operation_id,
                    request.succeeded,
                    &request.note,
                    cancel,
                )
                .await?,
            )?)
        }
        "helper_environment_connect"
        | "helper_environment_disconnect"
        | "helper_environment_create"
        | "helper_environment_start"
        | "helper_environment_stop"
        | "helper_environment_remove_container"
        | "helper_environment_remove" => {
            let action = match name {
                "helper_environment_connect" => EnvironmentAction::Connect,
                "helper_environment_disconnect" => EnvironmentAction::Disconnect,
                "helper_environment_create" => EnvironmentAction::Create,
                "helper_environment_start" => EnvironmentAction::Start,
                "helper_environment_stop" => EnvironmentAction::Stop,
                "helper_environment_remove_container" => EnvironmentAction::RemoveContainer,
                "helper_environment_remove" => EnvironmentAction::Remove,
                _ => unreachable!(),
            };
            Ok(serde_json::to_value(
                operations::action(
                    &manager,
                    action,
                    serde_json::from_value(args.clone())?,
                    cancel,
                )
                .await?,
            )?)
        }
        _ => bail!("Unknown environment management action"),
    }
}
async fn list(manager: &Manager, cancel: &CancellationToken) -> Result<Value> {
    let records = manager.records(cancel).await?;
    let receipts = operation_summaries(
        manager
            .all_operations()?
            .into_iter()
            .filter(|operation| {
                matches!(
                    operation.receipt.state,
                    EnvironmentOperationState::Prepared
                        | EnvironmentOperationState::Running
                        | EnvironmentOperationState::OutcomeUnknown
                )
            })
            .collect(),
    );
    Ok(json!({"environments":records,"operations":receipts,"clients":clients()["clients"]}))
}
fn operation_summaries(operations: Vec<Operation>) -> Vec<EnvironmentReceipt> {
    operations
        .into_iter()
        .map(|mut operation| {
            if operation.receipt.created_at_ms.is_none() && operation.created_at > 0 {
                operation.receipt.created_at_ms = Some(operation.created_at);
            }
            operation.receipt.environment = None;
            operation.receipt.output = None;
            operation.receipt.message = operation.receipt.message.chars().take(256).collect();
            operation.receipt
        })
        .collect()
}
pub async fn execute_tool(
    config: &CapabilityConfig,
    name: &str,
    args: &Value,
    cancel: CancellationToken,
) -> Result<ToolObservation> {
    ensure!(
        serde_json::to_vec(args)?.len() <= 128 * 1024,
        "Environment tool request exceeds its byte limit"
    );
    let manager = Manager::open(config, &cancel).await?;
    let mut artifacts = Vec::new();
    let value = match name {
        "environment_list" => {
            let _: Empty = serde_json::from_value(args.clone())?;
            json!({"environments":manager.records(&cancel).await?.into_iter().map(|record|json!({"id":record.profile.id,"name":record.profile.name,"revision":record.revision,"connected":record.connected,"target":match record.profile.target{EnvironmentTarget::Ssh{host,user,port,..}=>json!({"kind":"ssh","host":host,"user":user,"port":port}),EnvironmentTarget::Container{runtime,container,..}=>json!({"kind":"container","runtime":runtime,"container":match container{ContainerTarget::Existing{target}=>json!({"mode":"existing","target":target}),ContainerTarget::Managed{image,..}=>json!({"mode":"managed","image":image})},"container_id":record.container_id})}})).collect::<Vec<_>>()})
        }
        "environment_status" => {
            let request: Identity = serde_json::from_value(args.clone())?;
            serde_json::to_value(status(&manager, request.environment_id, cancel).await?)?
        }
        "environment_execute" => {
            let receipt = operations::execute(
                &manager,
                serde_json::from_value(args.clone())?,
                config.operation_timeout_seconds,
                config.max_output_bytes,
                cancel,
            )
            .await?;
            artifacts = capture::artifact_paths(&receipt);
            serde_json::to_value(receipt)?
        }
        _ => bail!(
            "Unknown environment tool; lifecycle management is available only in the application"
        ),
    };
    let failed = value
        .get("state")
        .and_then(Value::as_str)
        .is_some_and(|state| state != "succeeded" && name == "environment_execute");
    let mut result = observation(value, artifacts);
    ensure!(
        result.summary.len() <= config.max_output_bytes,
        "Environment response exceeds the configured output budget; its durable receipt remains available"
    );
    if failed {
        result.status = "failed".into();
    }
    Ok(result)
}
/// Non-agent recovery hook for the application action journal. Exact terminal
/// inner receipts can resolve an outer uncertain write after explicit review.
/// No native command, connection probe or cancellation fence is created here.
pub async fn recover_execution(
    config: &CapabilityConfig,
    args: &Value,
    cancel: CancellationToken,
) -> Result<Option<ToolObservation>> {
    ensure!(
        serde_json::to_vec(args)?.len() <= 128 * 1024,
        "Environment recovery request exceeds its byte limit"
    );
    let request: EnvironmentExecuteRequest = serde_json::from_value(args.clone())?;
    let Some(manager) = Manager::existing(config)? else {
        return Ok(None);
    };
    let Some(receipt) = operations::recover_execution(&manager, &request, cancel).await? else {
        return Ok(None);
    };
    let succeeded = receipt.state == EnvironmentOperationState::Succeeded;
    let artifacts = capture::artifact_paths(&receipt);
    let mut result = observation(serde_json::to_value(receipt)?, artifacts);
    ensure!(
        result.summary.len() <= config.max_output_bytes,
        "Saved environment result exceeds the configured output budget; the receipt remains retained"
    );
    if !succeeded {
        result.status = "failed".into();
    }
    Ok(Some(result))
}
async fn status(
    manager: &Manager,
    id: Uuid,
    cancel: CancellationToken,
) -> Result<EnvironmentStatus> {
    let lease = manager.try_profile_lease(id)?;
    let record = manager.record(id, &cancel).await?;
    if lease.is_none() {
        return Ok(EnvironmentStatus {
            environment: record,
            state: EnvironmentState::OutcomeUnknown,
            message: "An operation is still running; check its receipt.".into(),
        });
    }
    let result = operations::probe(manager, &record, cancel).await;
    let (state,message)=match result {
        Ok(state)=>(state,match state {
            EnvironmentState::Connected if cfg!(unix)=>"Connection is ready.",
            EnvironmentState::Connected=>"Connection is ready. SSH commands use separate verified sessions on this platform.",
            EnvironmentState::Disconnected=>"This environment is disconnected.",EnvironmentState::Stopped=>"The container is stopped.",
            EnvironmentState::Missing=>"This container is not present.",EnvironmentState::OutcomeUnknown=>"Check the last operation before continuing.",
            EnvironmentState::Unavailable=>"The client could not verify this environment.",
        }.to_owned()),
        Err(error)=>(EnvironmentState::Unavailable,error.to_string()),
    };
    Ok(EnvironmentStatus {
        environment: record,
        state,
        message,
    })
}
/// Stop only masters created by this process. A task worker never closes a
/// helper-owned master merely because its own broker is shutting down.
pub async fn shutdown(config: &CapabilityConfig) -> Result<()> {
    let Some(root) = config.management_root.as_ref() else {
        return Ok(());
    };
    native::shutdown(&root.join("environments")).await
}
pub fn active_sessions(config: &CapabilityConfig) -> usize {
    config.management_root.as_ref().map_or(0, |root| {
        native::active(&root.join("environments")) + capture::active(&root.join("environments"))
    })
}
pub fn definitions() -> Vec<ToolDefinition> {
    vec![
        definition(
            "environment_list",
            "List saved native execution environments and their approval-binding revisions. Listing does not connect.",
            json!({}),
            &[],
            false,
        ),
        definition(
            "environment_status",
            "Check a saved environment without changing its target or lifecycle.",
            json!({"environment_id":{"type":"string","format":"uuid"}}),
            &["environment_id"],
            false,
        ),
        definition(
            "environment_execute",
            "Execute explicit argv in an already connected saved environment. Approval binds the profile revision. Reuse the same operation_id only for identical arguments; uncertain commands are never replayed automatically.",
            json!({"environment_id":{"type":"string","format":"uuid"},"operation_id":{"type":"string","format":"uuid"},"expected_revision":{"type":"integer","minimum":1},"command":{"type":"array","minItems":1,"maxItems":128,"items":{"type":"string"}},"timeout_seconds":{"type":"integer","minimum":1,"maximum":3600}}),
            &[
                "environment_id",
                "operation_id",
                "expected_revision",
                "command",
            ],
            true,
        ),
    ]
}
