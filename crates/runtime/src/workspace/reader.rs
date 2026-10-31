//! A read is tied to the selected directory handle. Canonicalisation is only
//! used to resolve an allowed internal symlink; subsequent IO never reopens the
//! checked absolute path, including when an ancestor is swapped concurrently.
use anyhow::{Context, Result, ensure};
use std::{
    fs::File,
    path::{Component, Path, PathBuf},
};
use tokio_util::sync::CancellationToken;

#[cfg(windows)]
#[path = "reader_windows.rs"]
mod platform;
#[cfg(unix)]
#[path = "reader_unix.rs"]
mod platform;

pub(super) struct RootReader {
    path: PathBuf,
    directory: File,
}
impl RootReader {
    pub fn open(
        root: &Path,
        cancel: &CancellationToken,
        project: &CancellationToken,
    ) -> Result<Self> {
        super::index::check(cancel, project)?;
        Ok(Self {
            path: root.to_owned(),
            directory: platform::open_root(root, cancel, project)?,
        })
    }
    pub fn file(
        &self,
        path: &Path,
        cancel: &CancellationToken,
        project: &CancellationToken,
    ) -> Result<File> {
        self.file_before(path, cancel, project, || Ok(()))
    }
    fn file_before(
        &self,
        path: &Path,
        cancel: &CancellationToken,
        project: &CancellationToken,
        before: impl FnOnce() -> Result<()>,
    ) -> Result<File> {
        super::index::check(cancel, project)?;
        let resolved = std::fs::canonicalize(path)?;
        let relative = resolved
            .strip_prefix(&self.path)
            .context("File is outside the selected project")?;
        ensure!(
            relative
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
                && relative.file_name().is_some(),
            "Choose a regular project file"
        );
        let mut parent = self.directory.try_clone()?;
        let mut parts = relative.components().peekable();
        while let Some(Component::Normal(name)) = parts.next() {
            super::index::check(cancel, project)?;
            if parts.peek().is_none() {
                before()?;
                return platform::open_relative(&parent, name, false);
            }
            parent = platform::open_relative(&parent, name, true)?;
        }
        anyhow::bail!("Choose a regular project file")
    }
}

#[cfg(all(test, unix))]
#[path = "reader/tests.rs"]
mod tests;
