//! Native application services.

pub mod drafts;
pub mod extensions;
pub use xylo_services::action_review::*;
pub mod history;
pub mod host;
pub mod learning;
pub mod lsp;
pub mod process;
pub mod steering;
pub mod terminal;
pub mod turn_queue;
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
mod tests;
