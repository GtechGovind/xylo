//! Private, project-scoped discovery for the local IDE bridge.
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    io::Read,
    path::{Path, PathBuf},
};
use xylo_core::ConnectorConfig;

const RECORD_LIMIT: u64 = 16 * 1024;
const BRIDGE_LIMIT: usize = 32;

#[derive(Clone, Debug)]
pub struct IdeBridge {
    pub connector: ConnectorConfig,
    pub workspaces: Vec<PathBuf>,
}

/// Credentials remain in memory and never enter the connector's saved config.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    pub version: u8,
    pub pid: u32,
    pub name: String,
    pub endpoint: String,
    pub token: String,
    pub workspaces: Vec<PathBuf>,
}

fn owned(metadata: &std::fs::Metadata, forbidden: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & forbidden == 0,
            "IDE discovery must be private and owned by this user"
        );
    }
    #[cfg(not(unix))]
    let _ = (metadata, forbidden);
    Ok(())
}
fn directory(home: &Path) -> Result<PathBuf> {
    let home = home.canonicalize()?;
    let parent = home.join(".xylo");
    let metadata = std::fs::symlink_metadata(&parent)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "IDE discovery directory cannot be a symbolic link"
    );
    owned(&metadata, 0o022)?;
    let directory = parent.join("ide");
    let metadata = std::fs::symlink_metadata(&directory)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "IDE discovery directory cannot be a symbolic link"
    );
    owned(&metadata, 0o077)?;
    Ok(directory)
}

pub(crate) fn read_record(home: &Path, path: &Path, workspace: &Path) -> Result<Record> {
    let directory = directory(home)?;
    ensure!(
        path.parent() == Some(directory.as_path()),
        "IDE record leaves the discovery directory"
    );
    ensure!(
        path.extension()
            .is_some_and(|extension| extension == "json"),
        "Invalid IDE record name"
    );
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    ensure!(
        !std::fs::symlink_metadata(path)?.file_type().is_symlink(),
        "IDE record cannot be a symbolic link"
    );
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= RECORD_LIMIT,
        "IDE record exceeds its size limit"
    );
    owned(&metadata, 0o077)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(metadata.nlink() == 1, "IDE record cannot be a hard link");
    }
    let mut bytes = Vec::new();
    file.take(RECORD_LIMIT + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= RECORD_LIMIT as usize,
        "IDE record grew beyond its size limit"
    );
    let record: Record = serde_json::from_slice(&bytes).context("Invalid IDE record")?;
    ensure!(
        record.version == 1 && record.pid > 0 && record.pid <= i32::MAX as u32,
        "Unsupported IDE record"
    );
    ensure!(
        !record.name.is_empty()
            && record.name.len() <= 80
            && !record.name.chars().any(char::is_control),
        "Invalid IDE name"
    );
    ensure!(
        record.token.len() == 64 && record.token.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Invalid IDE token"
    );
    let endpoint = url::Url::parse(&record.endpoint).context("Invalid IDE endpoint")?;
    ensure!(
        endpoint.scheme() == "ws"
            && endpoint.host_str() == Some("127.0.0.1")
            && endpoint.port().is_some_and(|port| port > 0)
            && endpoint.path() == "/mcp"
            && endpoint.username().is_empty()
            && endpoint.password().is_none()
            && endpoint.query().is_none()
            && endpoint.fragment().is_none(),
        "IDE endpoint must be a local authenticated bridge"
    );
    ensure!(
        !record.workspaces.is_empty() && record.workspaces.len() <= 16,
        "Invalid IDE workspace list"
    );
    let workspace = workspace.canonicalize()?;
    ensure!(
        record
            .workspaces
            .iter()
            .any(|path| path.is_absolute() && path == &workspace),
        "IDE bridge does not own the selected project"
    );
    Ok(record)
}

/// Finds at most 32 private bridges advertising the selected canonical project.
/// Malformed, stale-scope and inaccessible records never create connections.
pub fn discover(home: &Path, workspace: &Path) -> Result<Vec<IdeBridge>> {
    let directory = match directory(home) {
        Ok(directory) => directory,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(Vec::new());
        }
        Err(error) => return Err(error),
    };
    let mut paths: Vec<_> = std::fs::read_dir(directory)?
        .take(128)
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    paths.sort();
    Ok(paths
        .into_iter()
        .filter_map(|path| {
            let record = read_record(home, &path, workspace).ok()?;
            let endpoint = path.to_str()?.to_owned();
            let id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, endpoint.as_bytes());
            Some(IdeBridge {
                connector: ConnectorConfig {
                    id,
                    name: record.name,
                    transport: "ide".into(),
                    endpoint,
                    command: None,
                    args: Vec::new(),
                    credential_ref: None,
                    trusted: false,
                },
                workspaces: record.workspaces,
            })
        })
        .take(BRIDGE_LIMIT)
        .collect())
}

#[cfg(test)]
mod tests;
