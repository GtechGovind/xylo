//! Conversation-scoped file observations used to reject blind and stale writes.

use super::code::unapplied;
use anyhow::Result;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::SystemTime,
};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Fingerprint {
    modified: Option<SystemTime>,
    bytes: u64,
    digest: [u8; 32],
}
impl Fingerprint {
    pub(crate) fn read(path: &Path) -> Result<Self> {
        use std::io::Read;
        let file = std::fs::File::open(path)?;
        let metadata = file.metadata()?;
        anyhow::ensure!(
            metadata.is_file() && metadata.len() <= 64 * 1024 * 1024,
            "File exceeds the read-state budget"
        );
        let mut digest = Sha256::new();
        let mut reader = file.take(64 * 1024 * 1024 + 1);
        let bytes = std::io::copy(&mut reader, &mut DigestWriter(&mut digest))?;
        anyhow::ensure!(
            bytes <= 64 * 1024 * 1024,
            "File exceeds the read-state budget"
        );
        Ok(Self {
            modified: metadata.modified().ok(),
            bytes,
            digest: digest.finalize().into(),
        })
    }
}
struct DigestWriter<'a>(&'a mut Sha256);
impl std::io::Write for DigestWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Canonical workspace identity keeps observations isolated when a tool root changes.
/// A root change cannot authorize writes using a read from a different project.
type ReadKey = (Uuid, PathBuf, PathBuf);

#[derive(Default)]
pub(crate) struct ReadState(Mutex<HashMap<ReadKey, Fingerprint>>);
impl ReadState {
    fn key(session: Uuid, root: &Path, path: &Path) -> Result<(Uuid, PathBuf, PathBuf)> {
        Ok((session, root.canonicalize()?, path.canonicalize()?))
    }
    pub(crate) fn record(
        &self,
        session: Uuid,
        root: &Path,
        path: &Path,
        before: Fingerprint,
    ) -> Result<()> {
        let after = Fingerprint::read(path)?;
        anyhow::ensure!(
            before == after,
            "The file changed while being read; read it again"
        );
        self.0.lock().insert(Self::key(session, root, path)?, after);
        Ok(())
    }
    pub(crate) fn require(&self, session: Uuid, root: &Path, path: &Path) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }
        let key = Self::key(session, root, path).map_err(|error| unapplied(error.to_string()))?;
        let previous =
            self.0.lock().get(&key).cloned().ok_or_else(|| {
                unapplied("Read the existing file with file_read before editing it")
            })?;
        let current = Fingerprint::read(path).map_err(|error| unapplied(error.to_string()))?;
        if previous != current {
            return Err(unapplied(
                "The file changed since your last read; read it again",
            ));
        }
        Ok(())
    }
    pub(crate) fn updated(&self, session: Uuid, root: &Path, path: &Path) -> Result<()> {
        // Own successful writes advance an existing observation. Creating a file
        // does not invent a file_read observation for the next edit.
        let key = Self::key(session, root, path)?;
        let mut state = self.0.lock();
        if let Some(previous) = state.get_mut(&key) {
            *previous = Fingerprint::read(path)?;
        }
        Ok(())
    }
}

/// The exact disk revision presented for a pending approval, independent of later reads.
pub(crate) struct WriteRevision {
    path: PathBuf,
    fingerprint: Option<Fingerprint>,
}
impl WriteRevision {
    pub(crate) fn capture(path: &Path) -> Result<Self> {
        let fingerprint = if path.exists() {
            Some(Fingerprint::read(path)?)
        } else {
            None
        };
        Ok(Self {
            path: path.to_owned(),
            fingerprint,
        })
    }
    pub(crate) fn check(&self) -> Result<()> {
        let current = if self.path.exists() {
            Some(Fingerprint::read(&self.path).map_err(|error| unapplied(error.to_string()))?)
        } else {
            None
        };
        if current != self.fingerprint {
            return Err(unapplied(
                "The file changed after the edit was prepared; read it again",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
