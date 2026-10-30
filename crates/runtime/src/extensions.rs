//! Native settings operations for portable extensions; all I/O runs off GPUI.
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
pub use xylo_services::extensions::{
    ComponentKind, ComponentRuntimeStatus, ComponentSetup, DiscoveredExtension, ExtensionPackage,
    ExtensionService, ExtensionSource, default_sources,
};

pub async fn execute(
    root: std::path::PathBuf,
    store: xylo_services::Store,
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
        tokio::task::spawn_blocking(move || {
            xylo_services::CredentialVault::new(xylo_core::CREDENTIAL_SERVICE)?
                .set(&saved, &secret)
                .map_err(|_| {
                    anyhow::anyhow!("Could not store component credential in the OS vault")
                })
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
                xylo_services::CredentialVault::new(xylo_core::CREDENTIAL_SERVICE)?,
                cancel,
                &state,
            )
            .await?;
        let output = json!({"receipt_id":attempt_id,"component_status":service.component_status(package,component)?,"execution":result,"packages":service.list()?});
        store.finish_extension_execution(attempt_id, &output)?;
        receipt.complete = true;
        return Ok(output);
    }

    if name == "extension_discover" {
        let source: ExtensionSource = serde_json::from_value(args["source"].clone())?;
        return tokio::select! { _=cancel.cancelled()=>bail!("Discovery cancelled"), result=xylo_services::extensions::discover(&source)=>Ok(json!({"discovered":result?})) };
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

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(
        command: &str,
    ) -> (
        tempfile::TempDir,
        std::path::PathBuf,
        xylo_services::Store,
        ExtensionPackage,
        std::path::PathBuf,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let workspace = directory.path().join("workspace");
        std::fs::create_dir_all(source.join("hooks")).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(
            source.join("hooks/hooks.json"),
            json!({"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":command}]}]}})
                .to_string(),
        )
        .unwrap();
        let root = directory.path().join("profile/extensions");
        let service = ExtensionService::new(&root);
        let package = service.install_local(&source).unwrap();
        service.set_enabled(&package.id, true).unwrap();
        service
            .component_setup(
                &package.id,
                "hooks",
                ComponentSetup {
                    workspace: workspace.clone(),
                    ..Default::default()
                },
            )
            .unwrap();
        let store = xylo_services::Store::open(directory.path().join("profile/store.db")).unwrap();
        (directory, root, store, package, workspace)
    }
    #[tokio::test]
    async fn native_component_receipt_reuses_result_without_reexecuting_hook() {
        let (_directory, root, store, package, workspace) =
            fixture("printf once >> receipt-marker; printf '{}'");
        let attempt = uuid::Uuid::new_v4();
        let args = json!({"id":package.id,"component":"hooks"});
        let first = execute(
            root.clone(),
            store.clone(),
            attempt,
            "extension_component_verify",
            &args,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let second = execute(
            root,
            store.clone(),
            attempt,
            "extension_component_verify",
            &args,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(first, second);
        assert_eq!(
            std::fs::read_to_string(workspace.join("receipt-marker")).unwrap(),
            "once"
        );
        assert_eq!(first["component_status"]["readiness"], "execution_verified");
        assert_eq!(
            store.extension_execution_receipts(5).unwrap()[0]["state"],
            "succeeded"
        );
        assert!(store.conversations(10).unwrap().is_empty());
    }
    #[tokio::test]
    async fn cancelled_native_component_has_unknown_receipt_and_same_attempt_never_replays() {
        let (_directory, root, store, package, workspace) =
            fixture("printf once >> receipt-marker; sleep 30; printf '{}'");
        let attempt = uuid::Uuid::new_v4();
        let args = json!({"id":package.id,"component":"hooks"});
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let worker_root = root.clone();
        let worker_store = store.clone();
        let worker_args = args.clone();
        let running = tokio::spawn(async move {
            execute(
                worker_root,
                worker_store,
                attempt,
                "extension_component_verify",
                &worker_args,
                worker_cancel,
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            // Opening shell redirection creates the file before printf writes.
            // Cancel only after the observable effect this test asserts exists.
            while std::fs::read_to_string(workspace.join("receipt-marker")).ok().as_deref() != Some("once") {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        cancel.cancel();
        assert!(running.await.unwrap().is_err());
        assert_eq!(
            store.extension_execution_receipts(5).unwrap()[0]["state"],
            "outcome_unknown"
        );
        let retry = execute(
            root,
            store,
            attempt,
            "extension_component_verify",
            &args,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(retry.to_string().contains("outcome is unknown"));
        assert_eq!(
            std::fs::read_to_string(workspace.join("receipt-marker")).unwrap(),
            "once"
        );
    }
}
