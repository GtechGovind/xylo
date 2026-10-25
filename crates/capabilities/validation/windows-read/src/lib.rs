//! Focused API/type and native-filesystem validation of the actual Windows
//! production reader. This narrow config supplies only the paths that reader
//! uses; it does not claim to compile or qualify the whole application.
#![cfg(windows)]

use anyhow::{Result, ensure};
use std::{fs::File, path::PathBuf};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
pub struct CapabilityConfig {
    pub workspace: PathBuf,
    pub management_root: Option<PathBuf>,
    pub artifacts: PathBuf,
}

fn checked(cancel: &CancellationToken) -> Result<()> {
    ensure!(!cancel.is_cancelled(), "Plugin operation cancelled");
    Ok(())
}

#[path = "../../../src/plugins/windows_read.rs"]
mod windows_read;

pub fn open_workspace_read(
    config: &CapabilityConfig,
    path: &str,
    cancel: &CancellationToken,
) -> Result<File> {
    windows_read::open_workspace_read(config, path, cancel)
}

// Type-check the actual new output authority beside the production reader.
// Native handle/filesystem qualification still requires execution on Windows.
#[allow(dead_code)]
#[path = "../../../src/environments/capture_fs_windows.rs"]
mod environment_capture_fs;
