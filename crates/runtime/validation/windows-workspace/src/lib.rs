//! Compile-only qualification: this imports the production search reader body.
//! Actual Windows filesystem and reparse-race execution remains separate.
#[cfg(windows)]
mod index {
    pub(crate) fn check(
        cancel: &tokio_util::sync::CancellationToken,
        project: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !cancel.is_cancelled() && !project.is_cancelled(),
            "Search cancelled"
        );
        Ok(())
    }
}
#[cfg(windows)]
#[path = "../../../src/workspace/reader.rs"]
mod reader;

#[cfg(windows)]
pub fn open_project_file(
    root: &std::path::Path,
    path: &std::path::Path,
) -> anyhow::Result<std::fs::File> {
    let cancel = tokio_util::sync::CancellationToken::new();
    reader::RootReader::open(root, &cancel, &cancel)?.file(path, &cancel, &cancel)
}
