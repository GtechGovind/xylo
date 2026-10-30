//! Reviewed native component plugins with a durable, application-owned catalog.
//! Management controls are intentionally absent from agent tool discovery.

mod catalog;
mod runtime;
mod security;

use crate::{CapabilityConfig, definition, observation, required_str};
use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use xylo_core::{ToolDefinition, ToolObservation};

/// Profile-wide native UI management. Never register these names as agent tools.
pub async fn execute_management(
    config: &CapabilityConfig,
    name: &str,
    args: &Value,
    cancel: CancellationToken,
) -> Result<Value> {
    security::checked(&cancel)?;
    ensure!(
        config.management_root.is_some(),
        "Plugin management requires application-owned management storage"
    );
    match name {
        "helper_plugin_list" => {
            let config = config.clone();
            Ok(serde_json::to_value(
                xylo_resources::spawn_blocking(move || catalog::list(&config, &cancel)).await??,
            )?)
        }
        "helper_plugin_inspect" => {
            let path = required_str(args, "manifest_path")?.to_owned();
            Ok(serde_json::to_value(
                xylo_resources::spawn_blocking(move || {
                    catalog::inspect(&path, &cancel).map(|package| package.inspection)
                })
                .await??,
            )?)
        }
        "helper_plugin_install"
        | "helper_plugin_set_enabled"
        | "helper_plugin_set_grants"
        | "helper_plugin_remove" => {
            let child = cancel.child_token();
            let _cancel_on_drop = child.clone().drop_guard();
            let replay_config = config.clone();
            let replay_name = name.to_owned();
            let replay_args = args.clone();
            let replay_cancel = child.clone();
            if let Some(result) = xylo_resources::spawn_blocking(move || {
                catalog::replay(&replay_config, &replay_name, &replay_args, &replay_cancel)
            })
            .await??
            {
                return Ok(serde_json::to_value(result)?);
            }
            let package = if name == "helper_plugin_install" {
                let request: catalog::InstallArgs = serde_json::from_value(args.clone())?;
                let inspect_cancel = child.clone();
                let package = xylo_resources::spawn_blocking(move || {
                    catalog::inspect(&request.manifest_path, &inspect_cancel)
                })
                .await??;
                ensure!(
                    required_str(args, "expected_package_sha256")?
                        == package.inspection.package_sha256,
                    "Plugin package changed since inspection; review it again"
                );
                let root_config = config.clone();
                let root =
                    xylo_resources::spawn_blocking(move || catalog::root(&root_config)).await??;
                let timeout = package.inspection.manifest.budgets.timeout_seconds;
                tokio::select! {
                    result = runtime::compile(package.module.clone(), root, None, child.clone()) => { result?; }
                    _ = cancel.cancelled() => bail!("Plugin installation cancelled"),
                    _ = tokio::time::sleep(std::time::Duration::from_secs(timeout)) => bail!("Plugin validation exceeded its time budget"),
                }
                Some(package)
            } else {
                None
            };
            let config = config.clone();
            let args = args.clone();
            let name = name.to_owned();
            // Cancellation is cooperative here. Await publication once admitted
            // so a generic cancelled response cannot hide a committed catalog.
            Ok(serde_json::to_value(
                xylo_resources::spawn_blocking(move || {
                    catalog::mutate(&config, &name, args, package, &child)
                })
                .await??,
            )?)
        }
        _ => bail!("Unknown plugin management control"),
    }
}

pub async fn execute_tool(
    config: &CapabilityConfig,
    name: &str,
    args: &Value,
    cancel: CancellationToken,
) -> Result<ToolObservation> {
    match name {
        "plugin_list" => {
            let config = config.clone();
            let catalog =
                xylo_resources::spawn_blocking(move || catalog::list(&config, &cancel)).await??;
            Ok(observation(serde_json::to_value(catalog)?, vec![]))
        }
        "plugin_run_installed" => {
            ensure!(
                args.as_object().is_some_and(|object| object
                    .keys()
                    .all(|key| matches!(key.as_str(), "id" | "input" | "package_sha256"))),
                "Installed runs accept only id, input and package_sha256; permissions and budgets come from the reviewed package"
            );
            let id = required_str(args, "id")?.to_owned();
            let pin = required_str(args, "package_sha256")?.to_owned();
            let config_owned = config.clone();
            let read_cancel = cancel.clone();
            let loaded = xylo_resources::spawn_blocking(move || {
                catalog::load(&config_owned, &id, &pin, &read_cancel)
            })
            .await??;
            let input = match args.get("input") {
                None => String::new(),
                Some(value) => value
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("Plugin input must be a string"))?
                    .to_owned(),
            };
            let identity = json!({"id": loaded.plugin.manifest.id, "version": loaded.plugin.manifest.version, "package_sha256": loaded.plugin.package_sha256});
            runtime::run(
                runtime::Invocation {
                    config: config.clone(),
                    module: loaded.module,
                    input,
                    grants: loaded.plugin.grants.into_iter().collect(),
                    budgets: loaded.plugin.manifest.budgets,
                    root: loaded.root,
                    lease: Some(loaded.lease),
                    identity,
                },
                cancel,
            )
            .await
        }
        "plugin_run" => runtime::legacy(config, args, cancel).await,
        _ => bail!("Unknown plugin tool"),
    }
}

/// Compatibility entry point; explicit-grant local component execution.
pub async fn execute(
    config: &CapabilityConfig,
    args: &Value,
    cancel: CancellationToken,
) -> Result<ToolObservation> {
    runtime::legacy(config, args, cancel).await
}

pub fn definitions() -> Vec<ToolDefinition> {
    vec![
        definition(
            "plugin_list",
            "List installed component plugins, reviewed package SHA256 identities, enablement and effective permissions.",
            json!({}),
            &[],
            false,
        ),
        definition(
            "plugin_run_installed",
            "Run an enabled installed component by its reviewed package identity. Uses only its stored user grants and package budgets; updates and permission revocation are fenced while running.",
            json!({"id":{"type":"string"},"input":{"type":"string","maxLength":1048576},"package_sha256":{"type":"string","pattern":"^[a-f0-9]{64}$"}}),
            &["id", "package_sha256"],
            true,
        ),
        definition(
            "plugin_run",
            "Run a workspace component with explicit grants and bounded resources. No ambient WASI, process or network access. Prefer reviewed installed plugins.",
            json!({"path":{"type":"string"},"input":{"type":"string"},"capabilities":{"type":"array","items":{"type":"string"},"maxItems":32},"memory_mib":{"type":"integer","minimum":8,"maximum":256},"fuel":{"type":"integer","minimum":1,"maximum":100000000},"timeout_seconds":{"type":"integer","minimum":1,"maximum":60}}),
            &["path"],
            true,
        ),
    ]
}

#[cfg(test)]
mod tests;
