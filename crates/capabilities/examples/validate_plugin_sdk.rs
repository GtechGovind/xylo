//! Executes a compiled SDK guest through the real installed-plugin service.
//! Run `bash scripts/test-plugin-sdk.sh`; no accounts, devices or network calls.

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{path::PathBuf, time::Duration};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workbench_capabilities::{CapabilityConfig, CapabilityService};
use workbench_core::{PluginCatalog, PluginInspection, PluginManagementResult};

async fn control(service: &CapabilityService, name: &str, args: Value) -> Result<Value> {
    tokio::time::timeout(
        Duration::from_secs(30),
        service.execute_control(name, &args, CancellationToken::new()),
    )
    .await?
}

async fn grants(service: &CapabilityService, id: &str, selected: &[&str]) -> Result<()> {
    let catalog: PluginCatalog =
        serde_json::from_value(control(service, "helper_plugin_list", json!({})).await?)?;
    control(
        service,
        "helper_plugin_set_grants",
        json!({
            "operation_id": Uuid::new_v4(), "expected_revision": catalog.revision,
            "id": id, "grants": selected
        }),
    )
    .await?;
    Ok(())
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    ensure!(
        args.len() == 1,
        "Usage: validate_plugin_sdk /path/to/plugin.json"
    );
    let manifest = PathBuf::from(&args[0]).canonicalize()?;
    let fixture = tempfile::tempdir()?;
    let root = fixture.path().canonicalize()?;
    let workspace = root.join("project");
    std::fs::create_dir(&workspace)?;
    std::fs::write(workspace.join("notes.txt"), "hello\r\n世界 hello\n")?;
    let management = root.join("data/private");
    let service = CapabilityService::new(CapabilityConfig {
        workspace: workspace.clone(),
        artifacts: root.join("data/artifacts"),
        management_root: Some(management.clone()),
        ..Default::default()
    })?;
    let inspected: PluginInspection = serde_json::from_value(
        control(
            &service,
            "helper_plugin_inspect",
            json!({"manifest_path": manifest}),
        )
        .await?,
    )?;
    ensure!(
        inspected.manifest.id == "org.agentworkbench.text-report",
        "Unexpected example package"
    );
    let empty: PluginCatalog =
        serde_json::from_value(control(&service, "helper_plugin_list", json!({})).await?)?;
    let installed: PluginManagementResult = serde_json::from_value(
        control(
            &service,
            "helper_plugin_install",
            json!({"operation_id": Uuid::new_v4(),
            "expected_revision":empty.revision,"manifest_path":manifest,
            "expected_package_sha256":inspected.package_sha256,
            "grants":["read_workspace","write_artifacts"],"enabled":true,"replace":false}),
        )
        .await?,
    )?;
    let plugin = installed
        .catalog
        .plugins
        .first()
        .context("Plugin not installed")?;
    let input = json!({"id": plugin.manifest.id,
        "package_sha256":plugin.package_sha256, "input":r#"{"path":"notes.txt","output_name":"report-one"}"#});
    let first = service
        .execute("plugin_run_installed", &input, CancellationToken::new())
        .await?;
    ensure!(
        first.status == "completed" && first.artifacts.len() == 1,
        "Guest did not publish one completed report: {first:?}"
    );
    let result: Value = serde_json::from_str(&first.summary)?;
    let report: Value =
        serde_json::from_str(result["output"].as_str().context("No guest output")?)?;
    ensure!(
        report["bytes"] == 20
            && report["characters"] == 16
            && report["lines"] == 2
            && report["words"] == 3,
        "Incorrect Unicode report: {report}"
    );
    ensure!(
        report["top_words"][0] == json!({"word":"hello","occurrences":2}),
        "Wrong word frequencies"
    );
    let saved = std::fs::read(&first.artifacts[0])?;
    let saved_report: Value = serde_json::from_slice(&saved)?;
    ensure!(
        saved_report["top_words"] == report["top_words"],
        "Published bytes differ from guest summary"
    );
    ensure!(
        std::path::Path::new(
            report["artifact"]
                .as_str()
                .context("No artifact in guest receipt")?
        ) == first.artifacts[0],
        "Guest path does not match actual committed artifact"
    );
    let collision = service
        .execute("plugin_run_installed", &input, CancellationToken::new())
        .await
        .unwrap_err();
    ensure!(
        collision.to_string().contains("Output already exists"),
        "Same output name was not refused"
    );
    let mut second_input = input.clone();
    second_input["input"] = json!(r#"{"path":"notes.txt","output_name":"report-two"}"#);
    let second = service
        .execute(
            "plugin_run_installed",
            &second_input,
            CancellationToken::new(),
        )
        .await?;
    ensure!(
        second.artifacts.len() == 1
            && second.artifacts[0] != first.artifacts[0]
            && std::fs::read(&first.artifacts[0])? == saved,
        "Second run overwrote the first report"
    );
    let before = std::fs::read_dir(root.join("data/artifacts"))?.count();
    grants(&service, &plugin.manifest.id, &["read_workspace"]).await?;
    let denied = service
        .execute("plugin_run_installed", &input, CancellationToken::new())
        .await
        .unwrap_err();
    ensure!(
        denied
            .to_string()
            .contains("write_artifacts capability not granted"),
        "Write grant was bypassed: {denied}"
    );
    grants(&service, &plugin.manifest.id, &["write_artifacts"]).await?;
    let denied = service
        .execute("plugin_run_installed", &input, CancellationToken::new())
        .await
        .unwrap_err();
    ensure!(
        denied
            .to_string()
            .contains("read_workspace capability not granted"),
        "Read grant was bypassed: {denied}"
    );
    ensure!(
        std::fs::read_dir(root.join("data/artifacts"))?.count() == before,
        "Denied runs published artifacts"
    );
    grants(
        &service,
        &plugin.manifest.id,
        &["read_workspace", "write_artifacts"],
    )
    .await?;
    let mut escape = input.clone();
    escape["input"] =
        json!(r#"{"path":"../data/private/plugins/catalog.json","output_name":"escape"}"#);
    ensure!(
        service
            .execute("plugin_run_installed", &escape, CancellationToken::new())
            .await
            .is_err(),
        "Guest escaped the workspace"
    );
    ensure!(
        std::fs::read(workspace.join("notes.txt"))? == "hello\r\n世界 hello\n".as_bytes(),
        "Guest changed source file"
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "schema_version":1,"package_sha256":plugin.package_sha256,
            "component_sha256": inspected.manifest.sha256,
            "manifest_sha256":format!("{:x}",Sha256::digest(std::fs::read(&manifest)?)),
            "checks":["compiled_rust_component","actual_reviewed_install","unicode_statistics",
                "committed_artifact_bytes","no_clobber_repeat","write_grant_revocation",
                "read_grant_revocation","workspace_escape_refused","source_unchanged"],
            "network_requests":0,"device_operations":0
        }))?
    );
    Ok(())
}
