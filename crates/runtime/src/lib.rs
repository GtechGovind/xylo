//! Native application services.

pub mod drafts;
pub use workbench_services::action_review::*;
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
pub use workbench_notifications as notifications;
pub use workbench_scheduler as scheduler;
pub use workbench_services::{
    DocumentCopyIntent, DocumentDraftCursor, DocumentDraftKey, DocumentDraftPage,
    DocumentDraftRelinkReceipt, DocumentDraftRelinkRequest, DocumentDraftSave,
    DocumentDraftSnapshot, DocumentDraftState, DocumentDraftSummary, DocumentDraftWrite,
    ExportSelection, FactCursor, FactDeleteKind, FactDeletePreview, FactDeleteRequest,
    FactDeletion, FactPage, FactVersion, MAX_DOCUMENT_DRAFT_BYTES, MemoryCursor, MemoryExport,
    MemoryFormSave, MemoryFormSnapshot, MemoryFormState, MemoryFormWrite, MemoryMutation,
    MemoryPage, SteeringOutcome, verify_document_relink_source,
};

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Stable application directories; tests pass explicit temporary directories instead.
#[derive(Clone, Debug)]
pub struct AppPaths {
    pub data: PathBuf,
    pub database: PathBuf,
    pub artifacts: PathBuf,
    pub models: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Result<Self> {
        let current = directories::ProjectDirs::from("com", "qurkos", "xylo")
            .context("Could not locate the Xylo application directory")?;
        let legacy = directories::ProjectDirs::from("org", "AgentWorkbench", "AgentWorkbench")
            .context("Could not locate the existing application directory")?;
        // Reuse an existing profile in place. No conversations, approval receipts,
        // models or draft references are copied, reset or moved during a rebrand.
        Self::at(select_data_directory(
            current.data_local_dir(),
            legacy.data_local_dir(),
        ))
    }

    pub fn at(data: PathBuf) -> Result<Self> {
        let paths = Self {
            database: data.join("workbench.db"),
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

/// New installations use the Xylo location. Existing installations keep their
/// original profile, including when a failed startup created an empty new folder.
/// If both locations contain databases, the explicit Xylo profile takes priority.
fn select_data_directory(current: &Path, legacy: &Path) -> PathBuf {
    if current.join("workbench.db").is_file() {
        current.to_owned()
    } else if legacy.join("workbench.db").is_file() || (!current.exists() && legacy.is_dir()) {
        legacy.to_owned()
    } else {
        current.to_owned()
    }
}

#[cfg(test)]
mod branding_compatibility_tests {
    use super::*;

    #[test]
    fn new_identity_preserves_existing_profiles_without_moving_or_merging_data() {
        let directory = tempfile::tempdir().unwrap();
        let current = directory.path().join("xylo");
        let legacy = directory.path().join("legacy");
        assert_eq!(select_data_directory(&current, &legacy), current);
        std::fs::create_dir(&legacy).unwrap();
        std::fs::write(legacy.join("workbench.db"), b"existing profile bytes").unwrap();
        std::fs::write(legacy.join("draft-reference"), b"existing draft").unwrap();
        assert_eq!(select_data_directory(&current, &legacy), legacy);
        assert!(
            !current.exists(),
            "Selecting a profile must not create another one"
        );
        std::fs::create_dir(&current).unwrap();
        assert_eq!(
            select_data_directory(&current, &legacy),
            legacy,
            "An empty new directory must not hide an existing database"
        );
        std::fs::write(current.join("workbench.db"), b"new profile bytes").unwrap();
        assert_eq!(select_data_directory(&current, &legacy), current);
        assert_eq!(
            std::fs::read(legacy.join("workbench.db")).unwrap(),
            b"existing profile bytes"
        );
        assert_eq!(
            std::fs::read(legacy.join("draft-reference")).unwrap(),
            b"existing draft"
        );
        assert_eq!(workbench_core::APPLICATION_ID, "com.qurkos.xylo");
        assert_eq!(workbench_core::CREDENTIAL_SERVICE, "org.agentworkbench");
    }
}
