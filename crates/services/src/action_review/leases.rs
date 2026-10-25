//! Kernel-owned in-process execution leases. Files are never unlinked or
//! explicitly unlocked while another owner might still use the same authority.
use super::*;
use fs2::FileExt;
use std::{
    fs::{self, File, OpenOptions},
    path::Path,
    sync::Arc,
};

#[derive(Clone)]
pub struct ActionExecutionLease {
    file: Arc<File>,
}
impl ActionExecutionLease {
    pub fn file(&self) -> Arc<File> {
        self.file.clone()
    }
}
fn open(path: &Path) -> Result<File> {
    let parent = path.parent().context("Action lease has no directory")?;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(parent)?;
    ensure!(
        !fs::symlink_metadata(parent)?.file_type().is_symlink(),
        "Invalid action execution storage"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        ensure!(
            fs::metadata(parent)?.uid() == unsafe { libc::geteuid() },
            "Action lease storage belongs to another account"
        );
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options.open(path)?;
    ensure!(
        file.metadata()?.is_file() && !fs::symlink_metadata(path)?.file_type().is_symlink(),
        "Invalid action execution lease"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() }
                && metadata.nlink() == 1
                && metadata.mode() & 0o077 == 0,
            "Action lease must be an owned private regular file"
        );
    }
    Ok(file)
}
#[derive(Debug)]
struct LiveAction;
impl std::fmt::Display for LiveAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("This action is still running; wait until its execution has stopped")
    }
}
impl std::error::Error for LiveAction {}
pub(super) fn stripe(id: Uuid) -> String {
    let bytes = Sha256::digest(id.as_bytes());
    format!(
        "{:03x}.lock",
        u16::from_be_bytes([bytes[0], bytes[1]]) & 4095
    )
}
fn try_lock(file: File) -> Result<Option<Arc<File>>> {
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(Arc::new(file))),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e.into()),
    }
}
impl Store {
    /// Held before durable intent creation through real execution and result
    /// persistence. Async native code must retain this file in its task context.
    pub fn action_execution_lease(
        &self,
        id: Uuid,
        qualified: bool,
    ) -> Result<ActionExecutionLease> {
        let lease = self.lock_action_execution(id)?;
        self.call(move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO action_execution_protocol(action_id,version) VALUES(?1,?2)",
                params![id.to_string(), if qualified { 1 } else { 0 }],
            )?;
            Ok(())
        })?;
        Ok(lease)
    }
    pub fn lock_action_execution(&self, id: Uuid) -> Result<ActionExecutionLease> {
        ensure!(!id.is_nil(), "Invalid action identity");
        let file = try_lock(open(&self.action_lease_root.join(stripe(id)))?)?.ok_or(LiveAction)?;
        Ok(ActionExecutionLease { file })
    }
    pub(super) fn action_review_guard(&self, action: &JournalAction) -> Result<Vec<Arc<File>>> {
        let id = action.intent.id;
        let protocol = self.call(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT version FROM action_execution_protocol WHERE action_id=?1",
                    [id.to_string()],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
                .unwrap_or(0))
        })?;
        self.guard_metadata(id, &action.intent.name, protocol)
    }
    fn guard_metadata(&self, id: Uuid, name: &str, protocol: i64) -> Result<Vec<Arc<File>>> {
        let eligible_local = matches!(name, "file_write" | "memory_remember" | "skill_propose");
        // Native clients can close inherited handles and legacy workers never
        // held this authority. Neither can be inferred quiescent from a new lock.
        ensure!(
            eligible_local && protocol == 1,
            "Verify this action in the service where it ran"
        );
        Ok(vec![self.lock_action_execution(id)?.file()])
    }
    pub(super) fn action_review_status_metadata(
        &self,
        id: Uuid,
        name: &str,
        protocol: i64,
    ) -> (bool, Option<String>) {
        match self.guard_metadata(id, name, protocol) {
            Ok(_) => (false, None),
            Err(error) => (
                error.downcast_ref::<LiveAction>().is_some(),
                Some(error.to_string().chars().take(256).collect()),
            ),
        }
    }
    pub fn action_review_busy(&self, scope: &ActionReviewScope, id: Uuid) -> Result<bool> {
        Ok(self.action_review_status(scope, id)?.0)
    }
    /// List liveness reads small metadata, never the retained observation.
    pub fn action_review_status(
        &self,
        scope: &ActionReviewScope,
        id: Uuid,
    ) -> Result<(bool, Option<String>)> {
        let session = scope.session_id;
        let (name, protocol) = self.call(move |conn| {
            conn.query_row("SELECT json_extract(a.intent,'$.name'),coalesce(p.version,0) FROM actions a LEFT JOIN action_execution_protocol p ON p.action_id=a.id WHERE a.id=?1 AND a.session_id=?2", params![id.to_string(),session.to_string()], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))).optional()?.context("This action is unavailable in the selected chat")
        })?;
        Ok(self.action_review_status_metadata(id, &name, protocol))
    }
}
