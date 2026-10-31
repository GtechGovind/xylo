//! Durable bounded output collection, independent of remote command effects.
//! Operation/profile leases follow actual readers/writers and finalization.
//!
//! The usage ledger bounds output that is still live: admitted captures that
//! have not settled (running, staged, or awaiting the user's reconciliation of
//! an unknown outcome). A settled capture's bytes are ordinary published
//! artifacts, so its reservation is released; every admission also runs a
//! maintenance pass that reclaims reservations without a live entry. A ledger
//! that does not decode is discarded and rewritten empty. Per-operation
//! `intent.json` files remain as deduplication evidence.
use super::{
    capture_fs::{self, Directory},
    storage::{Manager, Operation},
    *,
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    sync::{Arc, Mutex, OnceLock},
};

#[path = "capture_pipes.rs"]
mod pipes;

pub(super) const STREAM_LIMIT: u64 = 16 * 1024 * 1024;
const STORAGE_LIMIT: u64 = 1024 * 1024 * 1024;
const ENTRY_RESERVATION: u64 = 2 * MAX_METADATA as u64 + 4096;
const RESERVATION: u64 = STREAM_LIMIT * 2 + ENTRY_RESERVATION;
const MAX_METADATA: usize = 256 * 1024;
const MAX_USAGE: usize = 2 * 1024 * 1024;
const MAX_LIVE: usize = 1024;

/// Admission refused because unsettled captures already hold the whole output
/// budget. Only reconciling or finishing those operations frees space.
#[derive(Debug)]
pub(super) struct QuotaUnavailable {
    live: usize,
    reserved_bytes: u64,
}
impl std::fmt::Display for QuotaUnavailable {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            output,
            "Command output storage is full: {} unsettled command(s) hold {} MiB of the 1 GiB budget. Review or reconcile them in Environments, then retry. No command was dispatched.",
            self.live,
            self.reserved_bytes.div_ceil(1024 * 1024)
        )
    }
}
impl std::error::Error for QuotaUnavailable {}

fn jobs() -> &'static Mutex<HashMap<PathBuf, usize>> {
    static JOBS: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}
pub(super) fn active(root: &Path) -> usize {
    jobs()
        .lock()
        .map_or(1, |jobs| jobs.get(root).copied().unwrap_or(0))
}
pub(super) struct RunLease {
    _operation: File,
    _profile: File,
    root: PathBuf,
}
impl RunLease {
    pub fn new(root: PathBuf, operation: File, profile: File) -> Result<Arc<Self>> {
        *jobs()
            .lock()
            .map_err(|_| anyhow::anyhow!("Output lifecycle registry unavailable"))?
            .entry(root.clone())
            .or_default() += 1;
        Ok(Arc::new(Self {
            _operation: operation,
            _profile: profile,
            root,
        }))
    }
}
impl Drop for RunLease {
    fn drop(&mut self) {
        if let Ok(mut jobs) = jobs().lock()
            && let Some(count) = jobs.get_mut(&self.root)
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                jobs.remove(&self.root);
            }
        }
    }
}

pub(super) struct CancelOnDrop(pub CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Usage {
    version: u8,
    /// Bytes held by each live capture; keys are always a subset of `pending`.
    reservations: BTreeMap<Uuid, u64>,
    /// Live captures (operation id to profile id) awaiting settlement.
    pending: BTreeMap<Uuid, Uuid>,
    // Bounded original metadata survives a crash after quota admission but
    // before the per-operation files/intent are created. Never contains argv.
    intents: BTreeMap<Uuid, Intent>,
}

impl Usage {
    fn new() -> Self {
        Self {
            version: 1,
            ..Default::default()
        }
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1
                && self.reservations.len() <= MAX_LIVE
                && self.pending.len() <= MAX_LIVE
                && self.intents.len() <= MAX_LIVE,
            "Invalid output storage ledger"
        );
        Ok(())
    }

    /// Maintenance pass: drop reservations that no live capture owns. Returns
    /// how many were reclaimed.
    fn reclaim(&mut self) -> usize {
        let before = self.reservations.len();
        let pending = &self.pending;
        self.reservations.retain(|id, _| pending.contains_key(id));
        self.intents.retain(|id, _| pending.contains_key(id));
        before - self.reservations.len()
    }

    /// Release everything a settled capture held.
    fn settle(&mut self, id: Uuid) -> bool {
        let pending = self.pending.remove(&id).is_some();
        let reserved = self.reservations.remove(&id).is_some();
        self.intents.remove(&id);
        pending || reserved
    }

    fn live_bytes(&self) -> Result<u64> {
        self.reservations.values().try_fold(0u64, |total, bytes| {
            total.checked_add(*bytes).context("Output quota overflow")
        })
    }
}

/// The ledger, and whether it had to be replaced. A ledger that does not
/// decode or validate is discarded: it reads as empty, and the next locked
/// writer overwrites it.
fn load_usage(directory: &Directory) -> Result<(Usage, bool)> {
    if !directory.exists("usage.json")? {
        return Ok((Usage::new(), false));
    }
    let bytes = directory.read("usage.json", MAX_USAGE)?;
    match serde_json::from_slice::<Usage>(&bytes) {
        Ok(usage) if usage.validate().is_ok() => Ok((usage, false)),
        _ => Ok((Usage::new(), true)),
    }
}
fn read_usage(directory: &Directory) -> Result<Usage> {
    Ok(load_usage(directory)?.0)
}
#[derive(Clone, Serialize, Deserialize)]
struct Intent {
    version: u8,
    operation: Operation,
    artifacts_root: PathBuf,
    dispatched: bool,
    staged: bool,
    published: bool,
    preview_limit: usize,
}
pub(super) struct Session {
    manager: Manager,
    directory: Directory,
    target: Directory,
    intent: Intent,
    stdout: Option<File>,
    stderr: Option<File>,
}

pub(super) fn response_overhead(manager: &Manager, operation: &Operation) -> Result<usize> {
    let mut prospective = operation.receipt.clone();
    let artifacts = [
        ("stdout", EnvironmentOutputStream::Stdout),
        ("stderr", EnvironmentOutputStream::Stderr),
    ]
    .into_iter()
    .map(|(name, stream)| EnvironmentOutputArtifact {
        stream,
        path: manager
            .artifacts
            .join("environments")
            .join(operation.receipt.operation_id.to_string())
            .join(format!("{name}.bin")),
        sha256: "0".repeat(64),
        stored_bytes: STREAM_LIMIT,
        observed_bytes: u64::MAX,
        observed_bytes_exact: false,
        complete: false,
        reason: EnvironmentCaptureReason::Interrupted,
    })
    .collect();
    prospective.output = Some(EnvironmentOutput {
        success: false,
        exit_code: None,
        stdout: String::new(),
        stderr: String::new(),
        truncated: true,
        artifacts,
    });
    Ok(serde_json::to_vec(&prospective)?.len())
}

fn root(manager: &Manager, create: bool) -> Result<Directory> {
    Directory::open(&manager.root)?.child("captures", create)
}
fn lock(directory: &Directory, cancel: &CancellationToken) -> Result<File> {
    let file = directory.lock_file("storage.lock")?;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        ensure!(
            !cancel.is_cancelled(),
            "Output storage admission cancelled before dispatch"
        );
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {}
            Err(error) => return Err(error.into()),
        }
        ensure!(
            std::time::Instant::now() < deadline,
            "Output storage is busy; no command was dispatched"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn update_usage(
    manager: &Manager,
    id: Uuid,
    bytes: u64,
    intent: Option<&Intent>,
    cancel: &CancellationToken,
) -> Result<()> {
    let directory = root(manager, true)?;
    let _lock = lock(&directory, cancel)?;
    let mut usage = read_usage(&directory)?;
    let current = usage.reservations.get(&id).copied();
    if let Some(intent) = intent {
        usage.reclaim();
        ensure!(
            current.is_none() && !usage.pending.contains_key(&id),
            "This output identity already has a retained reservation; check its original receipt"
        );
        let live = usage.live_bytes()?;
        if live
            .checked_add(MAX_USAGE as u64)
            .and_then(|total| total.checked_add(bytes))
            .is_none_or(|total| total > STORAGE_LIMIT)
        {
            return Err(QuotaUnavailable {
                live: usage.pending.len(),
                reserved_bytes: live,
            }
            .into());
        }
        ensure!(
            usage.pending.len() < MAX_LIVE,
            "{MAX_LIVE} commands still have unsettled output; review or reconcile them in Environments before running another command. No command was dispatched."
        );
        usage.pending.insert(id, intent.operation.profile.id);
        usage.intents.insert(id, intent.clone());
    } else {
        ensure!(
            current.is_some_and(|reserved| bytes <= reserved),
            "Output finalization has no matching disk reservation"
        );
    }
    usage.reservations.insert(id, bytes);
    directory.write("usage.json", &serde_json::to_vec(&usage)?, MAX_USAGE)
}

pub(super) fn prepare(
    manager: &Manager,
    operation: &Operation,
    preview_limit: usize,
    cancel: &CancellationToken,
) -> Result<Session> {
    let directory =
        root(manager, true)?.child(&operation.receipt.operation_id.to_string(), true)?;
    ensure!(
        !directory.exists("intent.json")?,
        "Retained output already exists for this identity; check its original result"
    );
    let intent = Intent {
        version: 1,
        operation: operation.clone(),
        artifacts_root: manager.artifacts.clone(),
        dispatched: false,
        staged: false,
        published: false,
        preview_limit,
    };
    update_usage(
        manager,
        operation.receipt.operation_id,
        RESERVATION,
        Some(&intent),
        cancel,
    )?;
    let stdout = directory.file("stdout.part", true)?;
    let stderr = directory.file("stderr.part", true)?;
    let artifacts = Directory::open(&manager.artifacts)?;
    let target = artifacts
        .child("environments", true)?
        .child(&operation.receipt.operation_id.to_string(), true)?;
    ensure!(
        !target.exists("stdout.bin")?
            && !target.exists("stderr.bin")?
            && !target.exists("manifest.json")?,
        "Output destination already exists; no command was dispatched"
    );
    directory.write("intent.json", &serde_json::to_vec(&intent)?, MAX_METADATA)?;
    Ok(Session {
        manager: manager.clone(),
        directory,
        target,
        intent,
        stdout: Some(stdout),
        stderr: Some(stderr),
    })
}

fn valid_intent(manager: &Manager, id: Uuid, intent: &Intent) -> Result<()> {
    ensure!(
        intent.version == 1
            && intent.operation.receipt.operation_id == id
            && intent.operation.profile.id == intent.operation.receipt.environment_id
            && intent.operation.receipt.action == EnvironmentAction::Execute
            && intent.artifacts_root == manager.artifacts
            && (1..=8192).contains(&intent.preview_limit),
        "Retained output belongs to another operation or artifact authority"
    );
    Ok(())
}

/// Lookup for an interrupted operation whose ordinary receipt was not retained.
/// No command, liveness probe, metadata write or artifact publication occurs.
fn read_intent(manager: &Manager, id: Uuid) -> Result<Option<Intent>> {
    let captures = match root(manager, false) {
        Ok(directory) => directory,
        Err(error) if missing(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let operation_directory = captures.child(&id.to_string(), false);
    match operation_directory {
        Ok(directory) if directory.exists("intent.json")? => {
            let intent: Intent =
                serde_json::from_slice(&directory.read("intent.json", MAX_METADATA)?)?;
            valid_intent(manager, id, &intent)?;
            return Ok(Some(intent));
        }
        Ok(_) => {}
        Err(error) if missing(&error) => {}
        Err(error) => return Err(error),
    }
    if !captures.exists("usage.json")? {
        return Ok(None);
    }
    let usage = read_usage(&captures)?;
    let Some(mut intent) = usage.intents.get(&id).cloned() else {
        return Ok(None);
    };
    ensure!(
        usage.reservations.contains_key(&id)
            && usage.pending.get(&id) == Some(&intent.operation.profile.id),
        "Retained output intent has no matching reservation"
    );
    valid_intent(manager, id, &intent)?;
    // A missing file is not completion or non-dispatch evidence. Even this
    // pre-dispatch intent is reconciled conservatively if its separate file
    // disappeared, preserving an explicit review rather than replaying.
    intent.dispatched = true;
    intent.staged = false;
    intent.published = false;
    intent.operation.receipt.output = None;
    Ok(Some(intent))
}

fn missing(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

pub(super) fn retained_operation(manager: &Manager, id: Uuid) -> Result<Option<Operation>> {
    let Some(intent) = read_intent(manager, id)? else {
        return Ok(None);
    };
    let mut operation = intent.operation;
    // Artifact hashes are verified during reconciliation before a terminal
    // output can be returned. A mere metadata lookup never asserts completion.
    operation.receipt.state = if intent.dispatched {
        EnvironmentOperationState::Running
    } else {
        EnvironmentOperationState::Prepared
    };
    operation.receipt.output = None;
    Ok(Some(operation))
}

impl Session {
    fn write_intent(&self) -> Result<()> {
        self.directory.write(
            "intent.json",
            &serde_json::to_vec(&self.intent)?,
            MAX_METADATA,
        )
    }

    pub async fn run(
        mut self,
        spec: process::ProcessSpec,
        preview_limit: usize,
        cancel: CancellationToken,
        lease: Arc<RunLease>,
    ) -> Result<Operation> {
        self.intent.dispatched = true;
        self.intent.operation.receipt.state = EnvironmentOperationState::Running;
        let (mut session, guard) = (self, lease.clone());
        session = xylo_resources::spawn_blocking(move || {
            let _lease = guard;
            session.write_intent()?;
            Ok::<_, anyhow::Error>(session)
        })
        .await??;
        let stdout = session
            .stdout
            .take()
            .context("Missing output stream staging file")?;
        let stderr = session
            .stderr
            .take()
            .context("Missing error stream staging file")?;
        let result = pipes::run(spec, preview_limit, stdout, stderr, cancel, lease.clone()).await;
        let guard = lease;
        xylo_resources::spawn_blocking(move || {
            let _lease = guard;
            let mut session = session;
            let (state, message) = match &result.status {
                pipes::Status::Exited { success: true, .. } => (EnvironmentOperationState::Succeeded, "Command completed.".to_owned()),
                pipes::Status::Exited { code: Some(125 | 255), .. } => (EnvironmentOperationState::OutcomeUnknown, "The client did not confirm the remote outcome; inspect the target before retrying.".into()),
                pipes::Status::Exited { .. } => (EnvironmentOperationState::Failed, "The command failed; inspect its output before trying again.".into()),
                pipes::Status::NotStarted(message) => (EnvironmentOperationState::Cancelled, format!("No command was dispatched. {message}")),
                pipes::Status::Interrupted(message, _) => (EnvironmentOperationState::OutcomeUnknown, format!("{message}. The remote command may have executed and will not be replayed automatically.")),
            };
            session.intent.operation.receipt.state = state;
            session.intent.operation.receipt.message = message;
            session.stage(result)?;
            session.publish()?;
            Ok(session.intent.operation)
        }).await?
    }

    fn stage(&mut self, result: pipes::ResultData) -> Result<()> {
        let (success, exit_code) = match result.status {
            pipes::Status::Exited { success, code } => (success, code),
            _ => (false, None),
        };
        let mut artifacts = Vec::new();
        for (name, stream, progress) in [
            ("stdout", EnvironmentOutputStream::Stdout, result.stdout),
            ("stderr", EnvironmentOutputStream::Stderr, result.stderr),
        ] {
            let mut file = self.directory.file_for_sync(&format!("{name}.part"))?;
            file.sync_all()?;
            let (sha256, stored_bytes) = capture_fs::hash_file(&mut file, STREAM_LIMIT)?;
            artifacts.push(EnvironmentOutputArtifact {
                stream,
                path: self.target.path.join(format!("{name}.bin")),
                sha256,
                stored_bytes,
                observed_bytes: progress.observed.max(stored_bytes),
                observed_bytes_exact: progress.observed_exact,
                complete: progress.reason == EnvironmentCaptureReason::Eof,
                reason: progress.reason,
            });
        }
        self.intent.operation.receipt.output = Some(EnvironmentOutput {
            success,
            exit_code,
            stdout: String::from_utf8_lossy(&result.stdout_preview).into_owned(),
            stderr: String::from_utf8_lossy(&result.stderr_preview).into_owned(),
            truncated: result.truncated,
            artifacts,
        });
        self.intent.staged = true;
        self.write_intent()
    }

    fn publish(&mut self) -> Result<()> {
        let output = self
            .intent
            .operation
            .receipt
            .output
            .as_ref()
            .context("Output checkpoint missing")?;
        ensure!(
            self.intent.staged && output.artifacts.len() == 2,
            "Invalid staged output checkpoint"
        );
        self.target.verify_named()?;
        let mut total = ENTRY_RESERVATION;
        for artifact in &output.artifacts {
            let name = match artifact.stream {
                EnvironmentOutputStream::Stdout => "stdout",
                EnvironmentOutputStream::Stderr => "stderr",
            };
            ensure!(
                artifact.path == self.target.path.join(format!("{name}.bin"))
                    && artifact.stored_bytes <= STREAM_LIMIT
                    && artifact.observed_bytes >= artifact.stored_bytes,
                "Invalid retained output artifact authority"
            );
            self.directory.resume_publication(
                &format!("{name}.part"),
                &self.target,
                &format!("{name}.bin"),
                &artifact.sha256,
                artifact.stored_bytes,
            )?;
            if !self.target.exists(&format!("{name}.bin"))? {
                let source = self.directory.file_for_sync(&format!("{name}.part"))?;
                self.directory.publish(
                    &format!("{name}.part"),
                    &source,
                    &self.target,
                    &format!("{name}.bin"),
                )?;
            }
            let mut published = self.target.file(&format!("{name}.bin"), false)?;
            let (hash, bytes) = capture_fs::hash_file(&mut published, STREAM_LIMIT)?;
            ensure!(
                hash == artifact.sha256 && bytes == artifact.stored_bytes,
                "Retained output hash or length changed"
            );
            total = total
                .checked_add(bytes)
                .context("Output byte counter overflow")?;
        }
        let public = json!({"version":1,"operation_id":self.intent.operation.receipt.operation_id,
            "environment_id":self.intent.operation.receipt.environment_id,"request_sha256":self.intent.operation.digest,
            "artifacts":output.artifacts});
        let bytes = serde_json::to_vec(&public)?;
        let digest = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&bytes));
        self.directory.resume_publication(
            "manifest.part",
            &self.target,
            "manifest.json",
            &digest,
            bytes.len() as u64,
        )?;
        if self.target.exists("manifest.json")? {
            let previous: Value =
                serde_json::from_slice(&self.target.read("manifest.json", MAX_METADATA)?)?;
            ensure!(
                previous == public,
                "Retained output manifest belongs to different content"
            );
        } else {
            ensure!(
                bytes.len() <= MAX_METADATA,
                "Output manifest exceeds its byte budget"
            );
            if self.directory.exists("manifest.part")? {
                ensure!(
                    self.directory.read("manifest.part", MAX_METADATA)? == bytes,
                    "Interrupted output manifest differs from its original result"
                );
            } else {
                let mut file = self.directory.file("manifest.part", true)?;
                file.write_all(&bytes)?;
                file.sync_all()?;
            }
            let file = self.directory.file("manifest.part", false)?;
            self.directory
                .publish("manifest.part", &file, &self.target, "manifest.json")?;
        }
        self.intent.published = true;
        self.write_intent()?;
        update_usage(
            &self.manager,
            self.intent.operation.receipt.operation_id,
            total,
            None,
            &CancellationToken::new(),
        )
    }
}

pub(super) fn recover(manager: &Manager, operation: &Operation) -> Result<Option<Operation>> {
    let Some(retained) = retained_operation(manager, operation.receipt.operation_id)? else {
        return Ok(None);
    };
    ensure!(
        retained.digest == operation.digest && retained.profile.id == operation.profile.id,
        "Retained output digest differs from its original request"
    );
    let directory =
        root(manager, false)?.child(&operation.receipt.operation_id.to_string(), true)?;
    let intent = read_intent(manager, operation.receipt.operation_id)?
        .context("Output intent disappeared during reconciliation")?;
    valid_intent(manager, operation.receipt.operation_id, &intent)?;
    let target = Directory::open(&manager.artifacts)?
        .child("environments", true)?
        .child(&operation.receipt.operation_id.to_string(), true)?;
    if !intent.staged {
        for name in ["stdout.part", "stderr.part"] {
            if !directory.exists(name)? {
                directory.file(name, true)?.sync_all()?;
            }
        }
    }
    let mut session = Session {
        manager: manager.clone(),
        directory,
        target,
        intent,
        stdout: None,
        stderr: None,
    };
    if !session.intent.staged {
        session.intent.operation.receipt.state = if session.intent.dispatched {
            EnvironmentOperationState::OutcomeUnknown
        } else {
            EnvironmentOperationState::Cancelled
        };
        session.intent.operation.receipt.message = if session.intent.dispatched {
            "Recovered retained output from an interrupted client. The remote command outcome remains unknown; it was not replayed."
        } else { "Recovered an output reservation before native dispatch; no command was executed." }.into();
        let mut artifacts = Vec::new();
        let mut previews = Vec::new();
        for (name, stream) in [
            ("stdout", EnvironmentOutputStream::Stdout),
            ("stderr", EnvironmentOutputStream::Stderr),
        ] {
            let mut file = session.directory.file_for_sync(&format!("{name}.part"))?;
            file.sync_all()?;
            let (sha256, stored_bytes) = capture_fs::hash_file(&mut file, STREAM_LIMIT)?;
            file.seek(SeekFrom::Start(0))?;
            let mut preview = Vec::new();
            (&mut file)
                .take(session.intent.preview_limit as u64)
                .read_to_end(&mut preview)?;
            previews.push(String::from_utf8_lossy(&preview).into_owned());
            artifacts.push(EnvironmentOutputArtifact {
                stream,
                path: session.target.path.join(format!("{name}.bin")),
                sha256,
                stored_bytes,
                observed_bytes: stored_bytes,
                observed_bytes_exact: false,
                complete: false,
                reason: EnvironmentCaptureReason::Interrupted,
            });
        }
        session.intent.operation.receipt.output = Some(EnvironmentOutput {
            success: false,
            exit_code: None,
            stdout: previews.remove(0),
            stderr: previews.remove(0),
            truncated: true,
            artifacts,
        });
        session.intent.staged = true;
        session.write_intent()?;
    }
    session.publish()?;
    Ok(Some(session.intent.operation))
}

pub(super) fn artifact_paths(receipt: &EnvironmentReceipt) -> Vec<PathBuf> {
    receipt.output.as_ref().map_or_else(Vec::new, |output| {
        output
            .artifacts
            .iter()
            .map(|artifact| artifact.path.clone())
            .collect()
    })
}

pub(super) fn verify(manager: &Manager, operation: &Operation) -> Result<()> {
    let Some(output) = &operation.receipt.output else {
        return Ok(());
    };
    if output.artifacts.is_empty() {
        return Ok(());
    }
    ensure!(
        output.artifacts.len() == 2,
        "Retained output has an invalid stream inventory"
    );
    let target = Directory::open(&manager.artifacts)?
        .child("environments", false)?
        .child(&operation.receipt.operation_id.to_string(), false)?;
    target.verify_named()?;
    let manifest: Value = serde_json::from_slice(&target.read("manifest.json", MAX_METADATA)?)?;
    ensure!(
        manifest["version"] == json!(1)
            && manifest["operation_id"] == json!(operation.receipt.operation_id)
            && manifest["environment_id"] == json!(operation.receipt.environment_id)
            && manifest["request_sha256"] == json!(operation.digest)
            && manifest["artifacts"] == serde_json::to_value(&output.artifacts)?,
        "Retained output manifest differs from its operation receipt"
    );
    let mut streams = std::collections::BTreeSet::new();
    for artifact in &output.artifacts {
        let name = match artifact.stream {
            EnvironmentOutputStream::Stdout => "stdout.bin",
            EnvironmentOutputStream::Stderr => "stderr.bin",
        };
        ensure!(
            streams.insert(name) && artifact.path == target.path.join(name),
            "Retained output path is outside its operation authority"
        );
        let mut file = target.file(name, false)?;
        let (hash, bytes) = capture_fs::hash_file(&mut file, STREAM_LIMIT)?;
        ensure!(
            hash == artifact.sha256 && bytes == artifact.stored_bytes,
            "Retained output contents changed"
        );
    }
    Ok(())
}

/// Small active index keeps interrupted captures discoverable even if an outer
/// receipt was not committed. Lifetime output history is never scanned here.
pub(super) fn pending_ids(manager: &Manager) -> Result<Vec<Uuid>> {
    let directory = match root(manager, false) {
        Ok(directory) => directory,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(Vec::new());
        }
        Err(error) => return Err(error),
    };
    Ok(read_usage(&directory)?.pending.keys().copied().collect())
}

/// Caller has already synced the terminal ordinary receipt. The capture leaves
/// the live ledger and its reservation is released; published artifacts and
/// the per-operation intent (deduplication evidence) stay on disk.
pub(super) fn settled(manager: &Manager, operation: &Operation) -> Result<()> {
    if operation.receipt.action != EnvironmentAction::Execute
        || !matches!(
            operation.receipt.state,
            EnvironmentOperationState::Succeeded
                | EnvironmentOperationState::Failed
                | EnvironmentOperationState::Cancelled
        )
    {
        return Ok(());
    }
    let directory = match root(manager, false) {
        Ok(directory) => directory,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    if !directory.exists("usage.json")? {
        return Ok(());
    }
    let _lease = lock(&directory, &CancellationToken::new())?;
    let (mut usage, replaced) = load_usage(&directory)?;
    let released = usage.settle(operation.receipt.operation_id);
    if usage.reclaim() > 0 || released || replaced {
        directory.write("usage.json", &serde_json::to_vec(&usage)?, MAX_USAGE)?;
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests;

#[cfg(test)]
mod lock_tests;
