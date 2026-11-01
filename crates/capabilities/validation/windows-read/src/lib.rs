//! Focused API/type and native-filesystem validation of the actual Windows
//! production readers: the plugin reader and the tool-input walk. This narrow
//! config supplies only the paths they use; it does not claim to compile or
//! qualify the whole application.
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

// The NT open, identity and name helpers both Windows readers share.
#[path = "../../../src/windows_nt.rs"]
mod windows_nt;

#[path = "../../../src/plugins/windows_read.rs"]
mod windows_read;

// The actual tool-input walk and its tests, including the Windows link,
// junction, hard-link and management-directory cases.
// The tests' config spells `..Default::default()` for the full production
// config, which has more fields than this one.
#[cfg_attr(test, allow(clippy::needless_update))]
#[path = "../../../src/input.rs"]
mod input;

/// Opens a tool input through the production walk; see `input.rs`.
pub fn open_input(config: &CapabilityConfig, path: &str) -> Result<(File, PathBuf)> {
    let input = input::open_input(config, path)?;
    let _ = input.root;
    Ok((input.file, input.path))
}

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

// Execute the production learning sanitizer's native path/privacy regressions
// without initializing the SQLite store, keychain, inference or provider stack.
#[cfg(test)]
mod learning;
