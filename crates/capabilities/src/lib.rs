//! Native capability services, isolated from the agent implementation.
//!
//! Constructing a service is cheap. Browsers, model servers and native libraries
//! are initialized only when their corresponding operation is requested.

pub mod assets;
pub mod audio;
pub mod browser;
pub mod desktop;
pub mod documents;
pub mod environments;
mod managed_endpoints;
pub mod models;
pub mod plugins;
pub mod process;
mod speech;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;
use xylo_core::{ToolDefinition, ToolObservation};

/// Optional assets are local paths supplied by the reviewed asset manager or an
/// explicit import. Service construction never downloads or loads them.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct CapabilityConfig {
    pub workspace: PathBuf,
    pub artifacts: PathBuf,
    /// Application-owned catalogs and lifecycle receipts. The process host
    /// replaces this path; imported capability settings cannot redirect it.
    pub management_root: Option<PathBuf>,
    pub chromium: Option<PathBuf>,
    pub pdfium_library: Option<PathBuf>,
    pub llama: Option<models::LlamaConfig>,
    pub whisper: Option<models::SpeechHelper>,
    pub sherpa: Option<models::SpeechHelper>,
    pub helper_idle_seconds: u64,
    pub operation_timeout_seconds: u64,
    pub max_output_bytes: usize,
}

impl Default for CapabilityConfig {
    fn default() -> Self {
        let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            artifacts: workspace.join("artifacts"),
            workspace,
            management_root: None,
            chromium: None,
            pdfium_library: None,
            llama: None,
            whisper: None,
            sherpa: None,
            helper_idle_seconds: 60,
            operation_timeout_seconds: 120,
            max_output_bytes: 1024 * 1024,
        }
    }
}

pub struct CapabilityService {
    config: CapabilityConfig,
    browser: browser::BrowserService,
    models: models::ModelService,
    scheduler: xylo_scheduler::Scheduler,
}

impl CapabilityService {
    pub fn new(mut config: CapabilityConfig) -> Result<Self> {
        config.workspace = std::fs::canonicalize(&config.workspace)?;
        ensure!(
            config.workspace.is_dir(),
            "Capability workspace must be a directory"
        );
        ensure!(
            config.artifacts.is_absolute(),
            "Artifacts path must be absolute"
        );
        ensure!(
            (1..=3600).contains(&config.operation_timeout_seconds),
            "Timeout must be 1..3600 seconds"
        );
        ensure!(
            (1024..=16 * 1024 * 1024).contains(&config.max_output_bytes),
            "Output budget must be 1KiB..16MiB"
        );
        std::fs::create_dir_all(&config.artifacts)?;
        config.artifacts = std::fs::canonicalize(&config.artifacts)?;
        if let Some(root) = &config.management_root {
            ensure!(root.is_absolute(), "Management path must be absolute");
            let mut directory = std::fs::DirBuilder::new();
            directory.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                directory.mode(0o700);
            }
            directory.create(root)?;
            let metadata = std::fs::symlink_metadata(root)?;
            ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "Management storage must be a directory owned by the application"
            );
            let root = std::fs::canonicalize(root)?;
            ensure!(
                !root.starts_with(&config.artifacts) && !config.artifacts.starts_with(&root),
                "Management and artifact directories must be separate"
            );
            config.management_root = Some(root);
        }
        Ok(Self {
            browser: browser::BrowserService::new(config.clone()),
            models: models::ModelService::new(config.clone()),
            scheduler: xylo_scheduler::Scheduler::new(
                config.artifacts.join("schedules.json"),
                config.workspace.clone(),
            )?,
            config,
        })
    }

    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        let mut tools = Vec::new();
        tools.extend(browser::definitions());
        tools.extend(documents::definitions());
        tools.extend(models::definitions());
        tools.extend(desktop::definitions());
        tools.extend(plugins::definitions());
        tools.extend(environments::definitions());
        tools.extend(xylo_scheduler::definitions());
        tools
    }

    /// Must be called after the application's tool permission decision.
    pub async fn execute(
        &self,
        name: &str,
        args: &Value,
        cancel: CancellationToken,
    ) -> Result<ToolObservation> {
        if let (Some(demand), Some(root)) = (
            xylo_resources::capability_demand(name),
            self.config.management_root.as_ref(),
        ) {
            let lease =
                xylo_resources::shared::acquire(root.join("admission"), demand, cancel.clone())
                    .await?;
            return xylo_resources::scope_shared(
                std::sync::Arc::new(lease),
                self.execute_admitted(name, args, cancel),
            )
            .await;
        }
        self.execute_admitted(name, args, cancel).await
    }

    async fn execute_admitted(
        &self,
        name: &str,
        args: &Value,
        cancel: CancellationToken,
    ) -> Result<ToolObservation> {
        ensure!(!cancel.is_cancelled(), "Operation cancelled");
        let result = match name {
            n if n.starts_with("browser_") => self.browser.execute(n, args, cancel).await,
            n if n.starts_with("document_") => {
                documents::execute(&self.config, n, args, cancel).await
            }
            n if n.starts_with("local_inference_") || n.starts_with("speech_") => {
                self.models.execute(n, args, cancel).await
            }
            n if n.starts_with("desktop_") => desktop::execute(&self.config, n, args, cancel).await,
            n if n.starts_with("plugin_") => {
                plugins::execute_tool(&self.config, n, args, cancel).await
            }
            n if n.starts_with("environment_") => {
                environments::execute_tool(&self.config, n, args, cancel).await
            }
            n if n.starts_with("schedule_") => self.scheduler.execute(n, args).await,
            _ => anyhow::bail!("Unknown capability tool: {name}"),
        };
        result.and_then(|mut observation| {
            if observation.summary.len() > self.config.max_output_bytes {
                ensure!(
                    !name.starts_with("plugin_") && !name.starts_with("environment_"),
                    "Structured capability response exceeds the configured byte budget"
                );
                let mut end = self.config.max_output_bytes;
                while !observation.summary.is_char_boundary(end) {
                    end -= 1;
                }
                observation.summary.truncate(end);
                observation
                    .summary
                    .push_str("\n[Capability response truncated at configured byte budget]");
            }
            Ok(observation)
        })
    }

    /// Review details for a capability call, when they differ from its raw
    /// arguments. Installed plugin runs show the reviewed package identity and
    /// the catalog grants that will actually apply; `None` means the arguments
    /// themselves are the presentation.
    pub async fn approval_presentation(
        &self,
        name: &str,
        args: &Value,
        cancel: CancellationToken,
    ) -> Result<Option<Value>> {
        match name {
            "plugin_run_installed" => plugins::approval_presentation(&self.config, args, cancel)
                .await
                .map(Some),
            _ => Ok(None),
        }
    }

    /// Only the application helper's control channel calls this entry point.
    /// Catalog changes and permission grants are deliberately absent from the
    /// agent's advertised tools and ordinary execution dispatcher.
    pub async fn execute_control(
        &self,
        name: &str,
        args: &Value,
        cancel: CancellationToken,
    ) -> Result<Value> {
        if let (Some(demand), Some(root)) = (
            xylo_resources::capability_demand(name),
            self.config.management_root.as_ref(),
        ) {
            let lease =
                xylo_resources::shared::acquire(root.join("admission"), demand, cancel.clone())
                    .await?;
            return xylo_resources::scope_shared(
                std::sync::Arc::new(lease),
                self.execute_control_admitted(name, args, cancel),
            )
            .await;
        }
        self.execute_control_admitted(name, args, cancel).await
    }

    async fn execute_control_admitted(
        &self,
        name: &str,
        args: &Value,
        cancel: CancellationToken,
    ) -> Result<Value> {
        ensure!(!cancel.is_cancelled(), "Operation cancelled");
        match name {
            n if n.starts_with("helper_plugin_") => {
                plugins::execute_management(&self.config, n, args, cancel).await
            }
            n if n.starts_with("helper_environment_") => {
                environments::execute_management(&self.config, n, args, cancel).await
            }
            _ => anyhow::bail!("Unknown application control: {name}"),
        }
    }

    /// Reconcile a previously journaled execution from an exact native receipt.
    /// This performs no command dispatch and never invents a successful result
    /// when a native operation is still pending or uncertain.
    pub async fn recover_execution(
        &self,
        name: &str,
        args: &Value,
        cancel: CancellationToken,
    ) -> Result<Option<ToolObservation>> {
        if name == "environment_execute" {
            environments::recover_execution(&self.config, args, cancel).await
        } else {
            Ok(None)
        }
    }

    pub async fn status(&self) -> Value {
        json!({ "browser": self.browser.status().await, "models": self.models.status().await,
            "pdfium": asset_status(self.config.pdfium_library.as_deref()),
            "desktop": desktop::availability(), "plugins": "available",
            "schedules": self.scheduler.status().await })
    }

    pub async fn shutdown(&self) -> Result<()> {
        let _activity = self.prepare_configuration_change().await?;
        environments::shutdown(&self.config).await?;
        Ok(())
    }

    /// Stop idle services and retain exclusive model activity ownership through
    /// a validated configuration replacement. Busy chats fail before any stop.
    pub async fn prepare_configuration_change(&self) -> Result<models::IdleActivityLease> {
        let activity = self.models.idle_activity_lease()?;
        self.models.shutdown_with_activity_lease(&activity).await?;
        self.browser.shutdown().await?;
        Ok(activity)
    }
}

pub(crate) fn asset_status(path: Option<&Path>) -> &'static str {
    match path {
        Some(p) if p.is_file() => "configured_unverified",
        Some(_) => "asset_missing",
        None => "configuration_needed",
    }
}

pub(crate) fn observation(value: Value, artifacts: Vec<PathBuf>) -> ToolObservation {
    ToolObservation {
        status: "completed".into(),
        summary: value.to_string(),
        next_actions: vec![],
        artifacts,
    }
}

pub(crate) fn definition(
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

pub(crate) fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Missing nonempty string: {key}"))
}

/// Restrict inputs to the selected workspace or the app's capability artifacts.
pub(crate) fn input_path(config: &CapabilityConfig, raw: &str) -> Result<PathBuf> {
    let requested = Path::new(raw);
    let joined = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        config.workspace.join(requested)
    };
    let canonical = std::fs::canonicalize(joined)?;
    ensure!(
        !config
            .management_root
            .as_ref()
            .is_some_and(|root| canonical.starts_with(root)),
        "Application management files are outside the tool filesystem scope"
    );
    ensure!(
        canonical.starts_with(&config.workspace) || canonical.starts_with(&config.artifacts),
        "Input is outside the workspace and artifacts roots"
    );
    ensure!(canonical.is_file(), "Input must be a regular file");
    Ok(canonical)
}

pub(crate) fn output_path(
    config: &CapabilityConfig,
    name: &str,
    extension: &str,
) -> Result<PathBuf> {
    ensure!(
        !name.is_empty()
            && name.len() <= 120
            && name
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_'),
        "Output name must be a basename containing letters, digits, '_' or '-'"
    );
    let path = config.artifacts.join(format!("{name}.{extension}"));
    ensure!(!path.exists(), "Output already exists; choose a new name");
    Ok(path)
}

#[cfg(test)]
mod tests;
