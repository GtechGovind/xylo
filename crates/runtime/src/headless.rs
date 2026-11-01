//! Headless profile ownership, conversation admission and permission resolution.
//! The engine depends on runtime, so a headless frontend constructs its engine
//! after opening this profile, using the same store, vault and approval rules.

use crate::AppPaths;
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use std::{fs::File, path::Path};
use uuid::Uuid;
use xylo_core::{AppSettings, Conversation, PermissionMode, TitleSource};
use xylo_services::{CredentialVault, Store};

/// Exclusive ownership of a profile, shared with the desktop host lock.
pub struct Profile {
    pub paths: AppPaths,
    pub store: Store,
    pub settings: AppSettings,
    _lock: File,
}

impl Profile {
    /// Opens a profile without starting native windows or background services.
    pub fn open(paths: AppPaths) -> Result<Self> {
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(paths.data.join("host.lock"))?;
        lock.try_lock_exclusive()
            .context("Xylo is already running for this profile; choose another --data-dir")?;
        let store = Store::open(&paths.database)?;
        store.recover_interrupted()?;
        let settings = store.settings()?;
        Ok(Self {
            paths,
            store,
            settings,
            _lock: lock,
        })
    }

    /// Resolves the CLI override over the stored permission preset.
    /// Package F must apply managed/workspace restrictions here before returning
    /// the effective mode; neither SDK approvals nor CLI flags may widen them.
    pub fn permission_mode(&self, requested: Option<PermissionMode>) -> PermissionMode {
        requested.unwrap_or(self.settings.permissions)
    }

    /// Opens secrets using the profile's recorded storage choice.
    pub fn vault(&self) -> Result<CredentialVault> {
        CredentialVault::open(&self.paths.data, self.settings.secret_storage)
    }

    /// Resolves an explicit or most recent conversation, or creates one.
    /// A resumed conversation keeps its original workspace.
    pub fn conversation(
        &self,
        resume: Option<Uuid>,
        latest: bool,
        workspace: &Path,
    ) -> Result<Conversation> {
        ensure!(
            !(resume.is_some() && latest),
            "--resume and --continue are mutually exclusive"
        );
        if let Some(id) = resume {
            return self
                .store
                .conversation(id)?
                .context("The requested conversation does not exist");
        }
        if latest {
            return self
                .store
                .conversations(1)?
                .into_iter()
                .next()
                .context("There is no conversation to continue");
        }
        let conversation = Conversation {
            id: Uuid::new_v4(),
            title: "Headless chat".into(),
            workspace: workspace.canonicalize()?,
            updated_at: xylo_core::now_ms(),
            title_source: TitleSource::Prompt,
            pinned: false,
            archived: false,
        };
        self.store.upsert_conversation(&conversation)?;
        Ok(conversation)
    }
}

#[cfg(test)]
#[path = "headless/tests.rs"]
mod tests;
