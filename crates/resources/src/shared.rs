//! Cross-process admission for heavy native work in the same application profile.
//!
//! Each live reservation owns an OS file lock. The small JSON record is only a
//! demand ledger: process IDs and timestamps never determine liveness. Dropping
//! a lease closes its handle without doing filesystem work on the calling thread.
//!
//! Admission is first come, first served. A caller that cannot start at once
//! takes a queue ticket (`<id>.queuelock` holding an OS lock, `<id>.queue`
//! holding its sequence number and demand). Nobody is admitted while a live
//! ticket with a lower sequence could start once the current reservations
//! end, so a large request cannot be starved by a stream of small ones. A
//! waiter that could not start even then (its demand exceeds what other
//! programs leave free) holds nobody back: later requests that fit run
//! meanwhile, and it keeps its place for when memory frees up. Tickets of
//! crashed or cancelled waiters are reaped by the same lock probe as leases.

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
const MAX_WAITERS: usize = 256;
const MAX_RECORD_BYTES: u64 = 1024;
// Three files per lease (lock, record, partial), two per ticket, one lock.
const MAX_DIRECTORY_ENTRIES: usize = MAX_ACTIVE * 3 + MAX_WAITERS * 2 + 1;
const METADATA_LOCK: &str = "metadata.lock";
/// The metadata lock is only held for short ledger updates; waiting this long
/// means another process is stuck, which the caller should hear about.
const METADATA_WAIT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(250);

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

/// A queued caller's place in line. Dropping it releases the OS lock, after
/// which the next admission attempt by anyone reaps the ticket files.
#[derive(Debug)]
struct Ticket {
    _lock: File,
    id: Uuid,
    sequence: u64,
}

/// Outcome of one admission attempt.
#[derive(Debug)]
enum Attempt {
    Admitted(SharedResourceLease),
    Queued(Ticket),
}

/// Acquire a profile-wide native resource reservation using current machine
/// headroom, first come first served across processes. Directory
/// initialization, metadata and locks stay off async/UI threads. Queued
/// callers recheck every 250 ms and cancel promptly.
pub async fn acquire(
    directory: PathBuf,
    demand: TaskDemand,
    cancel: CancellationToken,
) -> Result<SharedResourceLease> {
    validate_demand(demand)?;
    let child = cancel.child_token();
    let _cancel_on_drop = child.clone().drop_guard();
    let mut ticket = None;
    loop {
        checked(&child)?;
        let task_directory = directory.clone();
        let task_cancel = child.clone();
        let attempt = tokio::task::spawn_blocking(move || {
            let hardware = hardware_snapshot()?;
            try_acquire(&task_directory, demand, hardware, ticket, &task_cancel)
        });
        let attempt = tokio::select! {
            result = attempt => result.context("Native admission worker stopped")??,
            _ = cancel.cancelled() => anyhow::bail!("Native work cancelled while queued"),
        };
        match attempt {
            Attempt::Admitted(lease) => {
                checked(&child)?;
                return Ok(lease);
            }
            Attempt::Queued(queued) => ticket = Some(queued),
        }
        tokio::select! {
            _ = tokio::time::sleep(POLL_INTERVAL) => {},
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
    static SYSTEM: OnceLock<Mutex<std::result::Result<sysinfo::System, String>>> = OnceLock::new();
    let mut system = SYSTEM
        .get_or_init(|| Mutex::new(sysinfo::System::new().map_err(|error| error.to_string())))
        .lock()
        .map_err(|_| anyhow::anyhow!("Native resource monitor unavailable"))?;
    Ok(super::snapshot(super::refreshed(&mut system)?))
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
    let deadline = Instant::now() + METADATA_WAIT;
    loop {
        checked(cancel)?;
        match FileExt::try_lock_exclusive(&file) {
            Ok(()) => return Ok(file),
            Err(error) if contended(&error) => {
                ensure!(
                    Instant::now() < deadline,
                    "Another Xylo process has held the native work queue for over {} seconds; nothing was started. Retry, or restart Xylo if this keeps happening.",
                    METADATA_WAIT.as_secs()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueueRecord {
    id: Uuid,
    sequence: u64,
    memory_bytes: u64,
    cpu_share: f32,
}

impl QueueRecord {
    fn demand(&self) -> Result<TaskDemand> {
        let demand = TaskDemand {
            memory_bytes: self.memory_bytes,
            cpu_share: self.cpu_share,
        };
        validate_demand(demand)?;
        Ok(demand)
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
    queue: bool,
    queue_lock: bool,
}

fn parse_name(name: &str) -> Result<(Uuid, &str)> {
    let (stem, extension) = name
        .rsplit_once('.')
        .context("Native admission storage contains an unexpected filename")?;
    ensure!(
        matches!(
            extension,
            "json" | "lock" | "partial" | "queue" | "queuelock"
        ),
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
            "queue" => entry.queue = true,
            "queuelock" => entry.queue_lock = true,
            _ => unreachable!(),
        }
    }
    Ok(entries)
}

/// A live queue ticket.
#[derive(Debug)]
struct Waiter {
    sequence: u64,
    id: Uuid,
    demand: TaskDemand,
}

impl Waiter {
    fn holds(&self, ticket: &Ticket) -> bool {
        (self.sequence, self.id) == (ticket.sequence, ticket.id)
    }
}

/// Live reservations and queue tickets after reaping dead ones.
#[derive(Debug, Default)]
struct Ledger {
    count: usize,
    memory: u64,
    cpu: f32,
    /// Live queue tickets in queue order.
    waiters: Vec<Waiter>,
}

impl Ledger {
    #[cfg(test)]
    fn totals(&self) -> (usize, u64, f32) {
        (self.count, self.memory, self.cpu)
    }
}

fn read_small(path: &Path) -> Result<Vec<u8>> {
    let file = open_regular(path, false, false)?;
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_RECORD_BYTES,
        "Native admission record grew beyond 1KiB"
    );
    Ok(bytes)
}

/// Reap a dead ticket, or return the live ticket's sequence and demand.
fn reap_ticket(directory: &Path, id: Uuid, entry: &Entry) -> Result<Option<(u64, TaskDemand)>> {
    let lock_path = directory.join(format!("{id}.queuelock"));
    let record_path = directory.join(format!("{id}.queue"));
    if !entry.queue_lock {
        if entry.queue {
            std::fs::remove_file(record_path)?;
        }
        return Ok(None);
    }
    let lock = open_regular(&lock_path, false, false)?;
    match FileExt::try_lock_exclusive(&lock) {
        Ok(()) => {
            if entry.queue {
                std::fs::remove_file(record_path)?;
            }
            drop(lock);
            std::fs::remove_file(lock_path)?;
            Ok(None)
        }
        Err(error) if contended(&error) => {
            // A live waiter writes its record before it ever releases the
            // metadata lock, so a missing record here means tampering.
            ensure!(entry.queue, "Live native queue ticket has no record");
            let record: QueueRecord = serde_json::from_slice(&read_small(&record_path)?)
                .context("Live native queue ticket is corrupt")?;
            ensure!(
                record.id == id,
                "Native queue ticket identity does not match its lock"
            );
            Ok(Some((record.sequence, record.demand()?)))
        }
        Err(error) => Err(error.into()),
    }
}

/// A lock probe, not a PID check, decides whether a record can be removed.
fn reap_and_sum(directory: &Path, cancel: &CancellationToken) -> Result<Ledger> {
    let mut ledger = Ledger::default();
    for (id, entry) in entries(directory, cancel)? {
        checked(cancel)?;
        if entry.queue || entry.queue_lock {
            if let Some((sequence, demand)) = reap_ticket(directory, id, &entry)? {
                ledger.waiters.push(Waiter {
                    sequence,
                    id,
                    demand,
                });
                ensure!(
                    ledger.waiters.len() <= MAX_WAITERS,
                    "Too many native operations are queued; wait for some to finish"
                );
            }
            if !(entry.record || entry.lease || entry.partial) {
                continue;
            }
        }
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
                let record: Record = serde_json::from_slice(&read_small(&record_path)?)
                    .context("Live native admission metadata is corrupt")?;
                ensure!(
                    record.id == id,
                    "Native admission record identity does not match its lease"
                );
                let demand = record.demand()?;
                ledger.memory = ledger
                    .memory
                    .checked_add(demand.memory_bytes)
                    .context("Native admission memory accounting overflow")?;
                ledger.cpu += demand.cpu_share;
                ensure!(
                    ledger.cpu.is_finite(),
                    "Native admission CPU accounting overflow"
                );
                ledger.count += 1;
                ensure!(
                    ledger.count <= MAX_ACTIVE,
                    "Native admission active-record limit exceeded"
                );
            }
            Err(error) => return Err(error.into()),
        }
    }
    ledger
        .waiters
        .sort_unstable_by_key(|waiter| (waiter.sequence, waiter.id));
    Ok(ledger)
}

/// Take a place at the back of the queue. Called with the metadata lock held.
fn enqueue(directory: &Path, ledger: &Ledger, demand: TaskDemand) -> Result<Ticket> {
    ensure!(
        ledger.waiters.len() < MAX_WAITERS,
        "Too many native operations are queued; wait for some to finish"
    );
    let id = Uuid::new_v4();
    let sequence = ledger
        .waiters
        .last()
        .map_or(0, |waiter| waiter.sequence + 1);
    let lock = open_regular(&directory.join(format!("{id}.queuelock")), false, true)?;
    FileExt::try_lock_exclusive(&lock)?;
    let bytes = serde_json::to_vec(&QueueRecord {
        id,
        sequence,
        memory_bytes: demand.memory_bytes,
        cpu_share: demand.cpu_share,
    })?;
    let mut record = open_regular(&directory.join(format!("{id}.queue")), false, true)?;
    record.write_all(&bytes)?;
    Ok(Ticket {
        _lock: lock,
        id,
        sequence,
    })
}

/// Remove an admitted waiter's ticket. Called with the metadata lock held.
fn dequeue(directory: &Path, ticket: Ticket) -> Result<()> {
    let id = ticket.id;
    std::fs::remove_file(directory.join(format!("{id}.queue")))?;
    drop(ticket);
    std::fs::remove_file(directory.join(format!("{id}.queuelock")))?;
    Ok(())
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

/// Whether `demand` would fit once every live reservation ended and the CPU
/// were idle. A waiter for which this is false is held back by other
/// programs, not by the queue, so it does not hold back the waiters behind it.
fn fits_when_idle(hardware: ResourceSnapshot, demand: TaskDemand, ledger: &Ledger) -> bool {
    let idle = ResourceSnapshot {
        available_memory_bytes: hardware
            .available_memory_bytes
            .saturating_add(ledger.memory)
            .min(hardware.total_memory_bytes),
        cpu_busy_percent: 0.,
        ..hardware
    };
    super::fits(idle, demand, 0, 0.)
}

/// One admission attempt. `ticket` is the caller's place in line from an
/// earlier attempt; a caller is admitted only when its demand fits and no
/// live ticket ahead of it could start once the current reservations end
/// (see [`fits_when_idle`]). Otherwise it keeps (or takes) a ticket.
fn try_acquire(
    directory: &Path,
    demand: TaskDemand,
    hardware: ResourceSnapshot,
    ticket: Option<Ticket>,
    cancel: &CancellationToken,
) -> Result<Attempt> {
    validate_demand(demand)?;
    checked(cancel)?;
    let directory = prepare_directory(directory, cancel)?;
    let _metadata = metadata_lock(&directory, cancel)?;
    let ledger = reap_and_sum(&directory, cancel)?;
    let waiting_ahead = ledger
        .waiters
        .iter()
        .take_while(|waiter| ticket.as_ref().is_none_or(|ticket| !waiter.holds(ticket)))
        .any(|waiter| fits_when_idle(hardware, waiter.demand, &ledger));
    if waiting_ahead
        || ledger.count == MAX_ACTIVE
        || !super::fits(hardware, demand, ledger.memory, ledger.cpu)
    {
        return Ok(Attempt::Queued(match ticket {
            Some(ticket) => ticket,
            None => enqueue(&directory, &ledger, demand)?,
        }));
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
    if let Some(ticket) = ticket {
        dequeue(&directory, ticket)?;
    }
    Ok(Attempt::Admitted(SharedResourceLease {
        _lock: lease,
        directory,
        id,
    }))
}

#[cfg(test)]
mod tests;
