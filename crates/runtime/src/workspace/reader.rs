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
mod tests {
    use super::*;
    use std::io::Read;
    #[test]
    fn final_symlink_substitution_is_refused_before_any_bytes_are_read() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(root.join("file"), "inside").unwrap();
        std::fs::write(outside.path().join("secret"), "outside").unwrap();
        let stop = CancellationToken::new();
        let reader = RootReader::open(&root, &stop, &stop).unwrap();
        assert!(
            reader
                .file_before(&root.join("file"), &stop, &stop, || {
                    std::fs::remove_file(root.join("file"))?;
                    std::os::unix::fs::symlink(outside.path().join("secret"), root.join("file"))?;
                    Ok(())
                })
                .is_err()
        );
    }
    #[test]
    fn swapped_ancestor_reads_the_original_authorized_handle_and_internal_links_work() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.join("nested")).unwrap();
        std::fs::write(root.join("nested/file"), "inside").unwrap();
        std::fs::write(outside.path().join("file"), "outside").unwrap();
        std::os::unix::fs::symlink(root.join("nested/file"), root.join("internal")).unwrap();
        let stop = CancellationToken::new();
        let reader = RootReader::open(&root, &stop, &stop).unwrap();
        let mut original = reader.file(&root.join("internal"), &stop, &stop).unwrap();
        let mut text = String::new();
        original.read_to_string(&mut text).unwrap();
        assert_eq!(text, "inside");
        let mut file = reader
            .file_before(&root.join("nested/file"), &stop, &stop, || {
                std::fs::rename(root.join("nested"), root.join("moved"))?;
                std::os::unix::fs::symlink(outside.path(), root.join("nested"))?;
                Ok(())
            })
            .unwrap();
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        assert_eq!(text, "inside");
    }
}
