//! Cross-process admission for heavy native work in the same application profile.
//!
//! Each live reservation owns an OS file lock. The small JSON record is only a
//! demand ledger: process IDs and timestamps never determine liveness. Dropping
//! a lease closes its handle without doing filesystem work on the calling thread.

use super::{ResourceSnapshot, TaskDemand};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const MAX_ACTIVE: usize = 128;
const MAX_RECORD_BYTES: u64 = 1024;
const MAX_DIRECTORY_ENTRIES: usize = MAX_ACTIVE * 4 + 1;
const METADATA_LOCK: &str = "metadata.lock";

/// A live native-work reservation. Keep this value until the admitted operation
/// and any admitted blocking cleanup finish. It is intentionally not Clone.
#[derive(Debug)]
pub struct SharedResourceLease {
    _lock: File,
    directory: PathBuf,
    id: Uuid,
}

impl SharedResourceLease {
    pub fn directory(&self) -> &Path {
        &self.directory
    }
    pub fn id(&self) -> Uuid {
        self.id
    }
}

/// Acquire a profile-wide native resource reservation using current machine
/// headroom. Directory initialization, metadata and locks stay off async/UI
/// threads. Queued callers recheck pressure every 500 ms and cancel promptly.
pub async fn acquire(
    directory: PathBuf,
    demand: TaskDemand,
    cancel: CancellationToken,
) -> Result<SharedResourceLease> {
    validate_demand(demand)?;
    let child = cancel.child_token();
    let _cancel_on_drop = child.clone().drop_guard();
    loop {
        checked(&child)?;
        let task_directory = directory.clone();
        let task_cancel = child.clone();
        let attempt = tokio::task::spawn_blocking(move || {
            let hardware = hardware_snapshot()?;
            try_acquire(&task_directory, demand, hardware, &task_cancel)
        });
        let lease = tokio::select! {
            result = attempt => result.context("Native admission worker stopped")??,
            _ = cancel.cancelled() => anyhow::bail!("Native work cancelled while queued"),
        };
        if let Some(lease) = lease {
            checked(&child)?;
            return Ok(lease);
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(500)) => {},
            _ = cancel.cancelled() => anyhow::bail!("Native work cancelled while queued"),
        }
    }
}

fn checked(cancel: &CancellationToken) -> Result<()> {
    ensure!(!cancel.is_cancelled(), "Native work cancelled while queued");
    Ok(())
}

fn validate_demand(demand: TaskDemand) -> Result<()> {
    ensure!(
        demand.memory_bytes > 0 && demand.cpu_share.is_finite() && demand.cpu_share > 0.,
        "Invalid native resource estimate"
    );
    Ok(())
}

fn hardware_snapshot() -> Result<ResourceSnapshot> {
    static SYSTEM: OnceLock<Mutex<sysinfo::System>> = OnceLock::new();
    let mut system = SYSTEM
        .get_or_init(|| Mutex::new(sysinfo::System::new()))
        .lock()
        .map_err(|_| anyhow::anyhow!("Native resource monitor unavailable"))?;
    system.refresh_memory();
    system.refresh_cpu_usage();
    Ok(super::snapshot(&system))
}

fn prepare_directory(directory: &Path, cancel: &CancellationToken) -> Result<PathBuf> {
    ensure!(
        directory.is_absolute()
            && directory
                .components()
                .all(|part| !matches!(part, Component::ParentDir | Component::CurDir)),
        "Native admission requires an absolute application-owned directory without traversal"
    );
    let mut current = PathBuf::new();
    for part in directory.components() {
        checked(cancel)?;
        current.push(part);
        // A drive prefix (C: or \\?\C:) is not a filesystem root yet.
        // Inspect it only after RootDir is attached. UNC prefixes already
        // denote absolute share roots and still receive the normal checks.
        // Rust treats a verbatim drive prefix as having an implicit root even
        // without its separator; is_absolute() cannot decide this boundary.
        if matches!(part, Component::Prefix(prefix) if matches!(
            prefix.kind(), std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_)
        )) {
            continue;
        }
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "Native admission directory cannot use symbolic links or non-directory parents"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let builder = std::fs::DirBuilder::new();
                #[cfg(unix)]
                let builder = {
                    use std::os::unix::fs::DirBuilderExt;
                    let mut builder = builder;
                    builder.mode(0o700);
                    builder
                };
                match builder.create(&current) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error.into()),
                }
                let metadata = std::fs::symlink_metadata(&current)?;
                ensure!(
                    metadata.is_dir() && !metadata.file_type().is_symlink(),
                    "Native admission storage was substituted"
                );
            }
            Err(error) => return Err(error.into()),
        }
    }
    let canonical = directory.canonicalize()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let metadata = std::fs::metadata(&canonical)?;
        // SAFETY: geteuid has no arguments or memory preconditions.
        let current_user = unsafe { libc::geteuid() };
        ensure!(
            metadata.uid() == current_user,
            "Native admission directory is not owned by the current application user"
        );
        if metadata.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(&canonical, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(canonical)
}

fn open_regular(path: &Path, create: bool, new: bool) -> Result<File> {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "Native admission metadata must be a regular non-symlink file"
        );
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    if new {
        options.create_new(true);
    } else if create {
        options.create(true).truncate(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        let file = options.open(path)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.nlink() == 1,
            "Native admission metadata must not use special files or hard links"
        );
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        let file = options.open(path)?;
        ensure!(
            file.metadata()?.is_file(),
            "Native admission metadata must be a regular file"
        );
        Ok(file)
    }
}

fn contended(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock
        || error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
}

fn metadata_lock(directory: &Path, cancel: &CancellationToken) -> Result<File> {
    let file = open_regular(&directory.join(METADATA_LOCK), true, false)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        checked(cancel)?;
        match FileExt::try_lock_exclusive(&file) {
            Ok(()) => return Ok(file),
            Err(error) if contended(&error) => {
                ensure!(
                    Instant::now() < deadline,
                    "Native admission metadata is busy; retry the action"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    format: u32,
    id: Uuid,
    memory_bytes: u64,
    cpu_share: f32,
}

impl Record {
    fn demand(&self) -> Result<TaskDemand> {
        ensure!(
            self.format == 1,
            "Unsupported native admission metadata format"
        );
        let demand = TaskDemand {
            memory_bytes: self.memory_bytes,
            cpu_share: self.cpu_share,
        };
        validate_demand(demand)?;
        Ok(demand)
    }
}

#[derive(Default)]
struct Entry {
    record: bool,
    lease: bool,
    partial: bool,
}

fn parse_name(name: &str) -> Result<(Uuid, &str)> {
    let (stem, extension) = name
        .rsplit_once('.')
        .context("Native admission storage contains an unexpected filename")?;
    ensure!(
        matches!(extension, "json" | "lock" | "partial"),
        "Native admission storage contains an unsupported file"
    );
    let id = Uuid::parse_str(stem).context("Native admission metadata identity is invalid")?;
    ensure!(
        !id.is_nil() && id.to_string() == stem,
        "Native admission metadata identity is not canonical"
    );
    Ok((id, extension))
}

fn entries(directory: &Path, cancel: &CancellationToken) -> Result<BTreeMap<Uuid, Entry>> {
    let mut entries = BTreeMap::<Uuid, Entry>::new();
    for (index, entry) in std::fs::read_dir(directory)?.enumerate() {
        ensure!(
            index < MAX_DIRECTORY_ENTRIES,
            "Native admission storage exceeds its metadata budget"
        );
        checked(cancel)?;
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("Native admission metadata filename is invalid"))?;
        if name == METADATA_LOCK {
            continue;
        }
        let metadata = std::fs::symlink_metadata(entry.path())?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "Native admission storage contains a symbolic link or special file"
        );
        ensure!(
            metadata.len() <= MAX_RECORD_BYTES,
            "Native admission record exceeds 1KiB"
        );
        let (id, kind) = parse_name(&name)?;
        let entry = entries.entry(id).or_default();
        match kind {
            "json" => entry.record = true,
            "lock" => entry.lease = true,
            "partial" => entry.partial = true,
            _ => unreachable!(),
        }
    }
    Ok(entries)
}

/// A lock probe, not a PID check, decides whether a record can be removed.
fn reap_and_sum(directory: &Path, cancel: &CancellationToken) -> Result<(usize, u64, f32)> {
    let mut count = 0usize;
    let mut memory = 0u64;
    let mut cpu = 0f32;
    for (id, entry) in entries(directory, cancel)? {
        checked(cancel)?;
        let lock_path = directory.join(format!("{id}.lock"));
        let record_path = directory.join(format!("{id}.json"));
        let partial_path = directory.join(format!("{id}.partial"));
        ensure!(
            entry.lease || !entry.record,
            "Native admission record has no liveness lease; storage needs inspection"
        );
        if !entry.lease {
            if entry.partial {
                std::fs::remove_file(partial_path)?;
            }
            continue;
        }
        let lease = open_regular(&lock_path, false, false)?;
        match FileExt::try_lock_exclusive(&lease) {
            Ok(()) => {
                // No live holder exists. Contents may be incomplete/corrupt after
                // a process crash; they do not need parsing to reclaim safely.
                if entry.record {
                    std::fs::remove_file(record_path)?;
                }
                if entry.partial {
                    std::fs::remove_file(partial_path)?;
                }
                drop(lease);
                std::fs::remove_file(lock_path)?;
            }
            Err(error) if contended(&error) => {
                ensure!(
                    entry.record && !entry.partial,
                    "Live native admission lease has incomplete metadata"
                );
                let record_file = open_regular(&record_path, false, false)?;
                let mut bytes = Vec::new();
                record_file
                    .take(MAX_RECORD_BYTES + 1)
                    .read_to_end(&mut bytes)?;
                ensure!(
                    bytes.len() as u64 <= MAX_RECORD_BYTES,
                    "Native admission record grew beyond 1KiB"
                );
                let record: Record = serde_json::from_slice(&bytes)
                    .context("Live native admission metadata is corrupt")?;
                ensure!(
                    record.id == id,
                    "Native admission record identity does not match its lease"
                );
                let demand = record.demand()?;
                memory = memory
                    .checked_add(demand.memory_bytes)
                    .context("Native admission memory accounting overflow")?;
                cpu += demand.cpu_share;
                ensure!(cpu.is_finite(), "Native admission CPU accounting overflow");
                count += 1;
                ensure!(
                    count <= MAX_ACTIVE,
                    "Native admission active-record limit exceeded"
                );
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok((count, memory, cpu))
}

struct Pending {
    directory: PathBuf,
    id: Uuid,
    committed: bool,
}
impl Drop for Pending {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        for extension in ["partial", "json"] {
            let _ = std::fs::remove_file(self.directory.join(format!("{}.{}", self.id, extension)));
        }
        // The lock handle is released before this guard in the error path.
        let _ = std::fs::remove_file(self.directory.join(format!("{}.lock", self.id)));
    }
}

fn try_acquire(
    directory: &Path,
    demand: TaskDemand,
    hardware: ResourceSnapshot,
    cancel: &CancellationToken,
) -> Result<Option<SharedResourceLease>> {
    validate_demand(demand)?;
    checked(cancel)?;
    let directory = prepare_directory(directory, cancel)?;
    let _metadata = metadata_lock(&directory, cancel)?;
    let (count, memory, cpu) = reap_and_sum(&directory, cancel)?;
    if count == MAX_ACTIVE || !super::fits(hardware, demand, memory, cpu) {
        return Ok(None);
    }
    checked(cancel)?;
    let id = Uuid::new_v4();
    let mut pending = Pending {
        directory: directory.clone(),
        id,
        committed: false,
    };
    let lease = open_regular(&directory.join(format!("{id}.lock")), false, true)?;
    FileExt::try_lock_exclusive(&lease)?;
    let record = Record {
        format: 1,
        id,
        memory_bytes: demand.memory_bytes,
        cpu_share: demand.cpu_share,
    };
    let bytes = serde_json::to_vec(&record)?;
    ensure!(
        bytes.len() as u64 <= MAX_RECORD_BYTES,
        "Native admission record exceeds 1KiB"
    );
    let partial = directory.join(format!("{id}.partial"));
    let mut file = open_regular(&partial, false, true)?;
    file.write_all(&bytes)?;
    drop(file);
    checked(cancel)?;
    // No fsync is needed for this ephemeral ledger: a power loss also releases
    // every OS reservation. Atomic publication is needed for live readers.
    std::fs::rename(partial, directory.join(format!("{id}.json")))?;
    checked(cancel)?;
    pending.committed = true;
    Ok(Some(SharedResourceLease {
        _lock: lease,
        directory,
        id,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    const MIB: u64 = 1024 * 1024;
    fn hardware() -> ResourceSnapshot {
        ResourceSnapshot {
            total_memory_bytes: 8 * 1024 * MIB,
            available_memory_bytes: 2 * 1024 * MIB,
            cpu_threads: 8,
            cpu_busy_percent: 0.,
        }
    }
    fn directory() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().canonicalize().unwrap().join("admission");
        (temp, path)
    }
    fn demand() -> TaskDemand {
        TaskDemand {
            memory_bytes: 1024 * MIB,
            cpu_share: 1.,
        }
    }

    #[cfg(windows)]
    #[test]
    fn canonical_windows_directory_admission_creates_a_live_reservation() {
        let (_temp, canonical) = directory();
        let ordinary = PathBuf::from(
            canonical
                .to_str()
                .unwrap()
                .strip_prefix(r"\\?\")
                .expect("The native canonical temp path must use a verbatim drive prefix"),
        );
        for path in [canonical.join("verbatim-雪"), ordinary.join("ordinary-雪")] {
            let cancel = CancellationToken::new();
            let lease = try_acquire(&path, demand(), hardware(), &cancel)
                .unwrap_or_else(|error| panic!("Admission for {path:?} failed: {error:#}"))
                .expect("A deterministic available-memory budget must admit the reservation");
            assert_eq!(lease.directory(), path.canonicalize().unwrap());
            assert_eq!(
                reap_and_sum(lease.directory(), &cancel).unwrap(),
                (1, 1024 * MIB, 1.)
            );
            drop(lease);
            assert_eq!(reap_and_sum(&path, &cancel).unwrap(), (0, 0, 0.));
        }
    }

    #[test]
    fn independent_handles_share_accounting_and_drop_is_reaped_by_os_liveness() {
        let (_temp, path) = directory();
        let cancel = CancellationToken::new();
        let first = try_acquire(&path, demand(), hardware(), &cancel)
            .unwrap()
            .unwrap();
        assert!(
            try_acquire(&path, demand(), hardware(), &cancel)
                .unwrap()
                .is_none()
        );
        let record = first.directory().join(format!("{}.json", first.id()));
        assert!(record.exists());
        drop(first);
        let second = try_acquire(&path, demand(), hardware(), &cancel)
            .unwrap()
            .unwrap();
        assert!(!record.exists());
        assert_eq!(reap_and_sum(&path, &cancel).unwrap(), (1, 1024 * MIB, 1.));
        drop(second);
        assert_eq!(reap_and_sum(&path, &cancel).unwrap(), (0, 0, 0.));
    }

    #[test]
    fn cpu_and_available_memory_changes_control_admission_without_profiles() {
        let (_temp, path) = directory();
        let cancel = CancellationToken::new();
        let mut busy = hardware();
        busy.cpu_threads = 2;
        busy.cpu_busy_percent = 100.;
        let first = try_acquire(
            &path,
            TaskDemand {
                memory_bytes: MIB,
                cpu_share: 1.,
            },
            busy,
            &cancel,
        )
        .unwrap()
        .unwrap();
        assert!(
            try_acquire(
                &path,
                TaskDemand {
                    memory_bytes: MIB,
                    cpu_share: 1.
                },
                busy,
                &cancel
            )
            .unwrap()
            .is_none()
        );
        drop(first);
        let mut low = hardware();
        low.available_memory_bytes = 64 * MIB;
        assert!(
            try_acquire(&path, demand(), low, &cancel)
                .unwrap()
                .is_none()
        );
        assert!(
            try_acquire(&path, demand(), hardware(), &cancel)
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn queued_cancellation_leaves_no_live_reservation() {
        let (_temp, path) = directory();
        let cancel = CancellationToken::new();
        let caller = cancel.clone();
        let queued_path = path.clone();
        let task = tokio::spawn(async move {
            acquire(
                queued_path,
                TaskDemand {
                    memory_bytes: u64::MAX,
                    cpu_share: 1.,
                },
                caller,
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancel.cancel();
        assert!(
            tokio::time::timeout(Duration::from_millis(500), task)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        let check_path = path.clone();
        assert_eq!(
            tokio::task::spawn_blocking(move || {
                let canonical = prepare_directory(&check_path, &CancellationToken::new()).unwrap();
                let _lock = metadata_lock(&canonical, &CancellationToken::new()).unwrap();
                reap_and_sum(&canonical, &CancellationToken::new()).unwrap()
            })
            .await
            .unwrap(),
            (0, 0, 0.)
        );
    }

    #[tokio::test]
    async fn cancelled_outer_future_retains_admission_until_blocking_work_really_finishes() {
        let (_temp, path) = directory();
        let lease = std::sync::Arc::new(
            try_acquire(&path, demand(), hardware(), &CancellationToken::new())
                .unwrap()
                .unwrap(),
        );
        let (entered, waiting) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let (finished, done) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(crate::scope_shared(lease.clone(), async move {
            crate::spawn_blocking(move || {
                let _ = entered.send(());
                released.recv_timeout(Duration::from_secs(3)).unwrap();
                let _ = finished.send(());
            })
            .await
            .unwrap();
        }));
        drop(lease);
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .unwrap()
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        // The async task is gone, but its actual native closure still owns the
        // shared lease. A second worker cannot spend that reserved headroom.
        assert!(
            try_acquire(&path, demand(), hardware(), &CancellationToken::new())
                .unwrap()
                .is_none()
        );
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), done)
            .await
            .unwrap()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if try_acquire(&path, demand(), hardware(), &CancellationToken::new())
                .unwrap()
                .is_some()
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Blocking work leaked its admission lease"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[test]
    fn live_corrupt_metadata_fails_closed_but_dead_corrupt_record_is_reclaimed() {
        let (_temp, path) = directory();
        let cancel = CancellationToken::new();
        let lease = try_acquire(&path, demand(), hardware(), &cancel)
            .unwrap()
            .unwrap();
        let record = path.join(format!("{}.json", lease.id()));
        std::fs::write(&record, b"{invalid").unwrap();
        assert!(
            try_acquire(&path, demand(), hardware(), &cancel)
                .unwrap_err()
                .to_string()
                .contains("corrupt")
        );
        drop(lease);
        assert!(
            try_acquire(&path, demand(), hardware(), &cancel)
                .unwrap()
                .is_some()
        );
        assert!(!record.exists());
    }

    #[test]
    fn traversal_unexpected_files_and_oversized_records_are_rejected() {
        let (temp, path) = directory();
        let cancel = CancellationToken::new();
        let traversal = temp
            .path()
            .canonicalize()
            .unwrap()
            .join("admission/../outside");
        assert!(try_acquire(&traversal, demand(), hardware(), &cancel).is_err());
        assert!(!temp.path().join("outside").exists());
        prepare_directory(&path, &cancel).unwrap();
        std::fs::write(path.join("credentials.json"), b"{}").unwrap();
        assert!(try_acquire(&path, demand(), hardware(), &cancel).is_err());
        std::fs::remove_file(path.join("credentials.json")).unwrap();
        std::fs::write(path.join(format!("{}.json", Uuid::new_v4())), vec![0; 1025]).unwrap();
        assert!(try_acquire(&path, demand(), hardware(), &cancel).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn directory_and_metadata_symlinks_never_touch_the_outside_target() {
        let (temp, path) = directory();
        let cancel = CancellationToken::new();
        let outside = temp.path().canonicalize().unwrap().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        assert!(try_acquire(&path, demand(), hardware(), &cancel).is_err());
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        std::fs::remove_file(&path).unwrap();
        prepare_directory(&path, &cancel).unwrap();
        let target = outside.join("private");
        std::fs::write(&target, b"unchanged").unwrap();
        std::os::unix::fs::symlink(&target, path.join(METADATA_LOCK)).unwrap();
        assert!(try_acquire(&path, demand(), hardware(), &cancel).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"unchanged");
    }
}
