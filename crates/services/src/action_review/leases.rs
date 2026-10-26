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
pub(super) struct LiveAction;
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
        Err(e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
        {
            Ok(None)
        }
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
        let eligible_local = matches!(
            name,
            "file_write" | "file_edit" | "memory_remember" | "skill_propose"
        );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independent_execution_handles_report_contention_until_the_owner_drops() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("execution.lock");
        let owner = try_lock(open(&path).unwrap()).unwrap().unwrap();
        assert!(
            try_lock(open(&path).unwrap()).unwrap().is_none(),
            "A second handle must classify the live OS lock as contention"
        );
        drop(owner);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if try_lock(open(&path).unwrap()).unwrap().is_some() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Execution lease still has an OS owner after parent drop"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[cfg(unix)]
    #[test]
    fn cloexec_fork_reference_keeps_authority_until_child_exit() {
        use std::{
            process::{Command, Stdio},
            time::{Duration, Instant},
        };
        const CHILD: &str = "XYLO_FLOCK_FORK_PROBE";
        if std::env::var_os(CHILD).as_deref() == Some(std::ffi::OsStr::new("1")) {
            inherited_flock_probe();
            return;
        }
        // Exec first so this deliberately held fork cannot inherit unrelated
        // parallel fixtures' authority. The environment is child-local only.
        struct Child(Option<std::process::Child>);
        impl Drop for Child {
            fn drop(&mut self) {
                if let Some(mut child) = self.0.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
        let mut child = Child(Some(Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "action_review::leases::tests::cloexec_fork_reference_keeps_authority_until_child_exit", "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped())
            .spawn().unwrap()));
        let deadline = Instant::now() + Duration::from_secs(6);
        while child.0.as_mut().unwrap().try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "Isolated inherited-flock probe did not finish"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let output = child.0.take().unwrap().wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "Isolated inherited-flock probe failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    fn inherited_flock_probe() {
        use std::{
            io::{Read, Write},
            os::{fd::AsRawFd, unix::net::UnixStream},
            time::{Duration, Instant},
        };
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("execution.lock");
        let owner = try_lock(open(&path).unwrap()).unwrap().unwrap();
        let owner_fd = owner.as_raw_fd();
        // SAFETY: the actual owned production lease descriptor is live.
        assert_ne!(
            unsafe { libc::fcntl(owner_fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        let (mut parent, child) = UnixStream::pair().unwrap();
        parent
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        parent
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let (parent_fd, child_fd) = (parent.as_raw_fd(), child.as_raw_fd());
        // SAFETY: the forked branch below calls only async-signal-safe libc
        // functions and exits without running inherited Rust destructors.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "Fork probe: {}", std::io::Error::last_os_error());
        if pid == 0 {
            unsafe {
                libc::close(parent_fd);
                if libc::write(child_fd, b"1".as_ptr().cast(), 1) != 1 {
                    libc::_exit(2);
                }
                let mut release = [0_u8; 1];
                let read = libc::read(child_fd, release.as_mut_ptr().cast(), 1);
                libc::_exit(if read == 1 || read == 0 { 0 } else { 3 });
            }
        }
        struct ForkOwner(Option<libc::pid_t>);
        impl Drop for ForkOwner {
            fn drop(&mut self) {
                if let Some(pid) = self.0.take() {
                    // SAFETY: this is this probe's unreaped direct child.
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                        while libc::waitpid(pid, std::ptr::null_mut(), 0) < 0
                            && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
                        {
                        }
                    }
                }
            }
        }
        let mut child_owner = ForkOwner(Some(pid));
        drop(child);
        let mut ready = [0_u8; 1];
        parent.read_exact(&mut ready).unwrap();
        assert_eq!(ready, *b"1");
        drop(owner);
        assert!(
            try_lock(open(&path).unwrap()).unwrap().is_none(),
            "CLOEXEC must not erase a still-live fork reference before exec/exit"
        );
        parent.write_all(b"1").unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut status = 0;
        loop {
            // SAFETY: pid is the unreaped direct child, status is valid output.
            let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if result == pid {
                child_owner.0 = None;
                break;
            }
            assert!(
                result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR),
                "Wait for fork probe: {}",
                std::io::Error::last_os_error()
            );
            assert!(
                Instant::now() < deadline,
                "Fork owner did not release its descriptor"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 0);
        assert!(
            try_lock(open(&path).unwrap()).unwrap().is_some(),
            "Lease becomes available only after every kernel owner exits"
        );
    }
}
