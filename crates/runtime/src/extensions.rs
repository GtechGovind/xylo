//! Native settings operations for portable extensions; all I/O runs off GPUI.
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
pub use xylo_services::extensions::{
    ComponentKind, ComponentRuntimeStatus, ComponentSetup, DirectoryEntry, DirectoryPage,
    DirectoryQuery, DirectorySourceStatus, DiscoveredExtension, ExtensionPackage, ExtensionService,
    ExtensionSource, default_sources,
};

/// Runs one extension settings action. `vault` is the host's secret vault;
/// actions that need no secret never touch it.
pub async fn execute(
    root: std::path::PathBuf,
    store: xylo_services::Store,
    vault: Result<xylo_services::CredentialVault>,
    attempt_id: uuid::Uuid,
    name: &str,
    args: &Value,
    cancel: CancellationToken,
) -> Result<Value> {
    let service = ExtensionService::new(root);
    ensure!(!cancel.is_cancelled(), "Extension request cancelled");
    if name == "extension_component_credential" {
        let package = args["id"].as_str().context("Extension missing")?.to_owned();
        let component = args["component"]
            .as_str()
            .context("Component missing")?
            .to_owned();
        let variable = args["variable"]
            .as_str()
            .context("Environment variable missing")?
            .to_owned();
        let secret = args["secret"]
            .as_str()
            .context("Secret missing")?
            .to_owned();
        ensure!(
            !secret.is_empty() && secret.len() <= 65536,
            "Secret must be between 1 and 65536 bytes"
        );
        ensure!(
            !variable.is_empty()
                && variable.len() <= 128
                && variable
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                && variable != "XYLO_SANDBOX_CHILD_ENV",
            "Invalid environment variable"
        );
        let package_state = service
            .list()?
            .into_iter()
            .find(|entry| entry.id == package && entry.enabled)
            .context("Extension disabled or removed")?;
        ensure!(
            package_state
                .components
                .iter()
                .any(|entry| entry.name == component),
            "Component missing"
        );
        let reference = format!(
            "extension:{}:{}:{}:{}",
            package,
            component,
            variable,
            uuid::Uuid::new_v4()
        );
        ensure!(reference.len() <= 256, "Credential reference is too long");
        let saved = reference.clone();
        let vault = vault?;
        tokio::task::spawn_blocking(move || {
            vault
                .set(&saved, &secret)
                .map_err(|_| anyhow::anyhow!("Could not store the component credential"))
        })
        .await??;
        return Ok(json!({"credential":{"variable":variable,"reference":reference}}));
    }

    if matches!(
        name,
        "extension_component_verify" | "extension_component_run"
    ) {
        let package = args["id"].as_str().context("Extension missing")?;
        let component = args["component"].as_str().context("Component missing")?;
        let operation = if name == "extension_component_verify" {
            "verify"
        } else {
            args["operation"]
                .as_str()
                .context("Component operation missing")?
        };
        let state = service.component_status(package, component)?;
        let intent = json!({"package":package,"digest":state.digest,"setup":state.setup,"component":component,"operation":operation,"input":args.get("input").cloned().unwrap_or_else(||json!({}))});
        if let Some(cached) = store.begin_extension_execution(attempt_id, &intent)? {
            return Ok(cached);
        }
        let mut receipt = NativeReceiptGuard {
            store: store.clone(),
            id: attempt_id,
            complete: false,
        };
        let result = service
            .execute_component_bound(
                package,
                component,
                operation,
                args.get("input").cloned().unwrap_or_else(|| json!({})),
                store.clone(),
                vault?,
                cancel,
                &state,
            )
            .await?;
        let output = json!({"receipt_id":attempt_id,"component_status":service.component_status(package,component)?,"execution":result,"packages":service.list()?});
        store.finish_extension_execution(attempt_id, &output)?;
        receipt.complete = true;
        return Ok(output);
    }

    if name == "extension_directory_refresh" {
        let force = args["force"].as_bool().unwrap_or(false);
        let query = directory_query(&args["query"])?;
        let sources = tokio::select! {
            _ = cancel.cancelled() => bail!("Directory refresh cancelled"),
            sources = service.refresh_directory(force) => sources?,
        };
        let directory = tokio::task::spawn_blocking(move || service.directory(&query)).await??;
        return Ok(json!({"sources":sources,"directory":directory}));
    }
    if name == "extension_install_remote" {
        let item: DiscoveredExtension = serde_json::from_value(args["item"].clone())?;
        // Once installation starts its atomic catalog commit is authoritative;
        // closing the page must not pretend it rolled an installed package back.
        let package = service.install_discovered(&item).await?;
        return Ok(json!({"package":package,"packages":service.list()?}));
    }
    let name = name.to_owned();
    let args = args.clone();
    tokio::task::spawn_blocking(move || {
        ensure!(!cancel.is_cancelled(), "Extension request cancelled");
        match name.as_str() {
            "extension_snapshot" => {}
            // Reads only the cached directory: cheap enough for every keystroke.
            "extension_directory" => {
                let query = directory_query(&args)?;
                return Ok(json!({"directory":service.directory(&query)?}));
            }
            "extension_component_receipts" => {
                return Ok(json!({"receipts":store.extension_execution_receipts(100)?}));
            }
            "extension_component_setup" => {
                let status = service.component_setup(
                    args["id"].as_str().context("Extension missing")?,
                    args["component"].as_str().context("Component missing")?,
                    serde_json::from_value(args["setup"].clone())?,
                )?;
                return Ok(json!({"component_status":status,"packages":service.list()?}));
            }
            "extension_component_status" => {
                let status = match service.component_status(
                    args["id"].as_str().context("Extension missing")?,
                    args["component"].as_str().context("Component missing")?,
                ) {
                    Ok(status) => Some(status),
                    Err(error) if error.to_string().contains("needs explicit execution setup") => {
                        None
                    }
                    Err(error) => return Err(error),
                };
                return Ok(json!({"component_status":status,"packages":service.list()?}));
            }
            "extension_install_local" => {
                service.install_local(std::path::Path::new(
                    args["path"].as_str().context("Choose a package folder")?,
                ))?;
            }
            "extension_enable" => service.set_enabled(
                args["id"].as_str().context("Extension missing")?,
                args["enabled"].as_bool().context("Enabled state missing")?,
            )?,
            "extension_remove" => {
                service.remove(args["id"].as_str().context("Extension missing")?)?
            }
            "extension_sources" => service.save_sources(&serde_json::from_value::<
                Vec<ExtensionSource>,
            >(args["sources"].clone())?)?,
            _ => bail!("Unknown extension operation"),
        }
        Ok(json!({"packages":service.list()?,"sources":service.sources()?}))
    })
    .await?
}

struct NativeReceiptGuard {
    store: xylo_services::Store,
    id: uuid::Uuid,
    complete: bool,
}
impl Drop for NativeReceiptGuard {
    fn drop(&mut self) {
        if !self.complete {
            let _ = self.store.mark_extension_execution_unknown(self.id, "Component execution ended without a durable result; inspect effects before another explicit attempt");
        }
    }
}

/// A directory query from action arguments; absent fields take defaults.
fn directory_query(args: &Value) -> Result<DirectoryQuery> {
    if args.is_null() {
        return Ok(DirectoryQuery::default());
    }
    serde_json::from_value(args.clone()).context("Invalid directory search")
}

#[cfg(test)]
mod tests;
