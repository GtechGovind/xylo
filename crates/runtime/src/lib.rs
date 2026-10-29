//! Native application services.

pub mod drafts;
pub use xylo_services::action_review::*;
pub mod history;
pub mod host;
pub mod learning;
pub mod lsp;
pub mod process;
pub mod resources;
pub mod steering;
pub mod terminal;
pub mod updates;
pub mod workspace;
pub use xylo_notifications as notifications;
pub use xylo_scheduler as scheduler;
pub use xylo_services::{
    DocumentCopyIntent, DocumentDraftCursor, DocumentDraftKey, DocumentDraftPage,
    DocumentDraftRelinkReceipt, DocumentDraftRelinkRequest, DocumentDraftSave,
    DocumentDraftSnapshot, DocumentDraftState, DocumentDraftSummary, DocumentDraftWrite,
    ExportSelection, FactCursor, FactDeleteKind, FactDeletePreview, FactDeleteRequest,
    FactDeletion, FactPage, FactVersion, MAX_DOCUMENT_DRAFT_BYTES, MemoryCursor, MemoryExport,
    MemoryFormSave, MemoryFormSnapshot, MemoryFormState, MemoryFormWrite, MemoryMutation,
    MemoryPage, SteeringOutcome, verify_document_relink_source,
};

use anyhow::{Context, Result};
use std::path::PathBuf;

/// Xylo application directories; tests pass explicit temporary directories instead.
#[derive(Clone, Debug)]
pub struct AppPaths {
    pub data: PathBuf,
    pub database: PathBuf,
    pub artifacts: PathBuf,
    pub models: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Result<Self> {
        let project = directories::ProjectDirs::from("com", "qurkos", "xylo")
            .context("Could not locate the Xylo application directory")?;
        Self::at(project.data_local_dir().to_owned())
    }

    pub fn at(data: PathBuf) -> Result<Self> {
        let paths = Self {
            database: data.join("xylo.db"),
            artifacts: data.join("artifacts"),
            models: data.join("models"),
            data,
        };
        for path in [&paths.data, &paths.artifacts, &paths.models] {
            std::fs::create_dir_all(path).with_context(|| format!("Create {}", path.display()))?;
        }
        Ok(paths)
    }
}

#[cfg(test)]
mod branding_tests {
    use super::*;

    #[test]
    fn profile_uses_xylo_identity_and_database() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("xylo");
        let paths = AppPaths::at(data.clone()).unwrap();
        assert_eq!(paths.data, data);
        assert_eq!(paths.database, data.join("xylo.db"));
        assert!(paths.artifacts.is_dir());
        assert!(paths.models.is_dir());
        assert!(
            !paths.database.exists(),
            "Path setup must not open a database"
        );
        assert_eq!(xylo_core::APPLICATION_NAME, "Xylo");
        assert_eq!(xylo_core::APPLICATION_ID, "com.qurkos.xylo");
        assert_eq!(xylo_core::CREDENTIAL_SERVICE, "com.qurkos.xylo");
    }
}
