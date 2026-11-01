//! Inert project MCP discovery. No process, network, or credential store is touched here.
use super::{ConnectorLaunchScope, mcp::validate_endpoint};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use xylo_core::ConnectorConfig;

/// A frozen project entry; approval binds its content and canonical root.
#[derive(Clone)]
pub struct ProjectConnector {
    pub config: ConnectorConfig,
    pub workspace: PathBuf,
    pub fingerprint: String,
    pub environment: BTreeMap<String, String>,
}
impl ProjectConnector {
    /// Stable authority key; it changes when any server entry or explicit environment changes.
    pub fn approval_key(&self) -> String {
        format!("project-mcp:{}", self.fingerprint)
    }
    /// Applies only explicitly declared environment variables to the sandbox.
    pub fn launch_scope(&self, mut scope: ConnectorLaunchScope) -> ConnectorLaunchScope {
        scope.workspace = self.workspace.clone();
        scope.environment.extend(self.environment.clone());
        scope
    }
}
#[derive(Deserialize)]
struct ProjectFile {
    #[serde(rename = "mcpServers")]
    servers: BTreeMap<String, Server>,
}
#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct Server {
    #[serde(rename = "type", default)]
    transport: Option<String>,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    url: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}
/// Reads conventional project configuration without persisting or starting its servers.
pub fn project_connectors(workspace: &Path) -> Result<Vec<ProjectConnector>> {
    let workspace = workspace.canonicalize()?;
    let path = workspace.join(".mcp.json");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let path = path.canonicalize()?;
    ensure!(
        path.starts_with(&workspace),
        "Project MCP configuration leaves the workspace"
    );
    ensure!(
        std::fs::metadata(&path)?.len() <= 256 * 1024,
        "Project MCP configuration exceeds 256 KiB"
    );
    let document: ProjectFile = serde_json::from_slice(&std::fs::read(path)?)?;
    ensure!(
        document.servers.len() <= 32,
        "Project MCP configuration has more than 32 servers"
    );
    let mut output = Vec::new();
    for (name, server) in document.servers {
        ensure!(
            !name.is_empty() && name.len() <= 128 && !name.contains(['\n', '\r', '\0']),
            "Invalid project MCP server name"
        );
        let source_fingerprint = hex::encode(Sha256::digest(serde_json::to_vec(&(
            &workspace, &name, &server,
        ))?));
        let bytes = Sha256::digest(format!("{}:{name}", workspace.display()).as_bytes());
        let id = uuid::Uuid::from_bytes(bytes[..16].try_into()?);
        let transport = server.transport.unwrap_or_else(|| {
            if server.command.is_some() {
                "stdio".into()
            } else {
                "http".into()
            }
        });
        ensure!(
            matches!(
                transport.as_str(),
                "stdio" | "http" | "sse" | "streamable_http"
            ),
            "Unsupported project MCP transport"
        );
        let command = server
            .command
            .map(|command| resolve_command(&workspace, &command))
            .transpose()?;
        let fingerprint = hex::encode(Sha256::digest(serde_json::to_vec(&(
            source_fingerprint,
            &command,
        ))?));
        let endpoint = server.url.unwrap_or_default();
        if transport != "stdio" {
            validate_endpoint(&endpoint)?;
        } else {
            ensure!(command.is_some(), "Local MCP server needs a command");
        }
        ensure!(
            server.env.keys().all(|key| !key.is_empty()
                && key
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')),
            "Invalid MCP environment name"
        );
        output.push(ProjectConnector {
            config: ConnectorConfig {
                id,
                name,
                transport,
                endpoint,
                command,
                args: server.args,
                credential_ref: None,
                trusted: false,
            },
            workspace: workspace.clone(),
            fingerprint,
            environment: server.env,
        });
    }
    Ok(output)
}
fn resolve_command(workspace: &Path, command: &str) -> Result<PathBuf> {
    let path = Path::new(command);
    if path.is_absolute() || path.components().count() > 1 {
        return workspace
            .join(path)
            .canonicalize()
            .context("Project MCP executable is missing");
    }
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|path| path.join(command))
        .find(|path| path.is_file())
        .context("Project MCP executable was not found on PATH")?
        .canonicalize()
        .map_err(Into::into)
}

#[cfg(test)]
mod tests;

/// A cached MCP prompt exposed as a slash command. Arguments remain the server's schema hints.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct McpPromptCommand {
    pub command: String,
    pub connector_id: uuid::Uuid,
    pub prompt: String,
    pub description: String,
    pub arguments: serde_json::Value,
}
/// Maps discovered prompts into stable names for command menus and explicit invocations.
pub fn mcp_prompt_commands(
    config: &ConnectorConfig,
    prompts: &serde_json::Value,
) -> Vec<McpPromptCommand> {
    let slug = |name: &str| {
        let words = name
            .split(|character: char| !character.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .map(str::to_lowercase)
            .collect::<Vec<_>>();
        if words.is_empty() {
            "connector".to_owned()
        } else {
            words.join("_")
        }
    };
    prompts
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|prompt| {
            let name = prompt["name"].as_str()?;
            Some(McpPromptCommand {
                command: format!("/mcp__{}__{}", slug(&config.name), slug(name)),
                connector_id: config.id,
                prompt: name.into(),
                description: prompt["description"].as_str().unwrap_or_default().into(),
                arguments: prompt["arguments"].clone(),
            })
        })
        .collect()
}

impl crate::Store {
    /// Checks user consent for this exact project server configuration, independently of tool rules.
    pub fn project_mcp_approved(&self, entry: &ProjectConnector) -> Result<bool> {
        let root = entry.workspace.to_string_lossy().into_owned();
        let fingerprint = entry.fingerprint.clone();
        self.call(move |connection| Ok(connection.query_row("SELECT EXISTS(SELECT 1 FROM project_mcp_consents WHERE workspace=?1 AND fingerprint=?2)", rusqlite::params![root,fingerprint], |row| row.get(0))?))
    }
    /// Records only a user's explicit first-start decision; normal permission grants cannot set this.
    pub fn approve_project_mcp(&self, entry: &ProjectConnector) -> Result<()> {
        let root = entry.workspace.to_string_lossy().into_owned();
        let fingerprint = entry.fingerprint.clone();
        self.call(move |connection| {
            connection.execute(
                "INSERT OR IGNORE INTO project_mcp_consents(workspace,fingerprint) VALUES(?1,?2)",
                rusqlite::params![root, fingerprint],
            )?;
            Ok(())
        })
    }
}
