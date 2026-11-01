//! Durable exposure from instruction expansion, independent of action history.
//! Exposure is conservative for a whole conversation: rewinding cannot erase it.
use crate::Store;
use anyhow::{Result, ensure};
use rusqlite::params;
use std::path::{Path, PathBuf};
use uuid::Uuid;
use xylo_core::MemoryOrigin;

impl Store {
    /// Record canonical contributing files before their instruction text is emitted.
    /// Classification is bound to the original workspace, never re-derived after
    /// deletion, checkout switching, or reopening the store. No file content is saved.
    pub fn record_instruction_sources(
        &self,
        session: Uuid,
        workspace: &Path,
        sources: &[PathBuf],
    ) -> Result<()> {
        if sources.is_empty() {
            return Ok(());
        }
        let root = workspace.canonicalize()?;
        ensure!(
            sources.iter().all(|path| path.is_absolute()),
            "Instruction sources must be canonical paths"
        );
        let sources = sources
            .iter()
            .map(|path| {
                (
                    path.to_string_lossy().into_owned(),
                    !path.starts_with(&root),
                )
            })
            .collect::<Vec<_>>();
        let workspace = root.to_string_lossy().into_owned();
        self.call(move |conn| {
            let tx = conn.transaction()?;
            for (path, outside) in sources {
                tx.execute("INSERT OR IGNORE INTO instruction_sources(session_id,workspace,path,outside) VALUES(?1,?2,?3,?4)",params![session.to_string(),workspace,path,outside])?;
            }
            tx.commit()?;
            Ok(())
        })
    }
    /// Least-trusted instruction origin ever exposed in this conversation.
    pub fn session_instruction_origin(&self, session: Uuid) -> Result<MemoryOrigin> {
        self.call(move |conn| {
            let outside: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM instruction_sources WHERE session_id=?1 AND outside=1)",[session.to_string()],|row|row.get(0))?;
            Ok(if outside { MemoryOrigin::ToolOutput } else { MemoryOrigin::AgentInferred })
        })
    }
    /// Copy exposure when forking any history that may contain instructions. Call
    /// before admitting the fork's first turn; failure must abort fork admission.
    /// Rewind in the same conversation deliberately retains its exposure records.
    pub fn copy_instruction_provenance(&self, source: Uuid, target: Uuid) -> Result<()> {
        self.call(move |conn| {
            conn.execute("INSERT OR IGNORE INTO instruction_sources(session_id,workspace,path,outside) SELECT ?2,workspace,path,outside FROM instruction_sources WHERE session_id=?1",params![source.to_string(),target.to_string()])?;
            Ok(())
        })
    }
}
