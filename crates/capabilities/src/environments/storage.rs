//! Small atomic records and explicit cross-process leases. No native command
//! runs while the global metadata lease is held.
use super::*;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    time::Instant,
};

const MAX_PROFILES: usize = 128;
const MAX_RECORDS: usize = 512;
const MAX_OPERATIONS: usize = 1024;
const MAX_DATABASE_BYTES: u64 = 2 * 1024 * 1024;
pub(super) const MAX_RECEIPT_BYTES: u64 = 256 * 1024;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Stored {
    pub record: EnvironmentRecord,
    pub removed: bool,
    pub last_operation: Uuid,
}
#[derive(Serialize, Deserialize)]
struct Database {
    version: u32,
    installation: Uuid,
    records: BTreeMap<Uuid, Stored>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Operation {
    #[serde(default)]
    pub created_at: i64,
    pub digest: String,
    pub expected_revision: u64,
    pub profile: EnvironmentProfile,
    pub previous_container_id: Option<String>,
    pub receipt: EnvironmentReceipt,
}
#[derive(Clone)]
pub(super) struct Manager {
    pub root: PathBuf,
    pub artifacts: PathBuf,
}
pub(super) fn private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "Invalid native environment storage directory"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "Native environment storage belongs to another account"
        );
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
fn open_lock(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    ensure!(
        file.metadata()?.is_file() && !fs::symlink_metadata(path)?.file_type().is_symlink(),
        "Invalid native environment lease"
    );
    Ok(file)
}
pub(super) fn try_lease(file: &File) -> Result<bool> {
    match file.try_lock_exclusive() {
        Ok(()) => Ok(true),
        Err(error)
            if error.kind() == std::io::ErrorKind::WouldBlock
                || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
        {
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}

async fn lease(path: PathBuf, cancel: &CancellationToken) -> Result<File> {
    let file = open_lock(&path)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        ensure!(
            !cancel.is_cancelled(),
            "Environment request cancelled before admission"
        );
        if try_lease(&file)? {
            return Ok(file);
        }
        ensure!(
            Instant::now() < deadline,
            "This environment is still processing another operation"
        );
        tokio::select! {
            _ = cancel.cancelled() => anyhow::bail!("Environment request cancelled before admission"),
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
    }
}
pub(super) fn read_json<T: serde::de::DeserializeOwned>(path: &Path, max: u64) -> Result<T> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() <= max,
        "Invalid or oversized native environment record"
    );
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
pub(super) fn write_json(path: &Path, value: &impl Serialize, max: u64) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() as u64 <= max,
        "Native environment record exceeds its storage budget"
    );
    let parent = path
        .parent()
        .context("Environment record has no directory")?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(&bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}
impl Manager {
    /// Read-only feature lookup for journal recovery. Unused capabilities must
    /// not create storage or initialize a catalogue while restoring a turn.
    pub fn existing(config: &CapabilityConfig) -> Result<Option<Self>> {
        let Some(root) = &config.management_root else {
            return Ok(None);
        };
        ensure!(
            root.is_absolute(),
            "Environment storage must use an absolute path"
        );
        let root = root.join("environments");
        if !root.exists() {
            return Ok(None);
        }
        let metadata = fs::symlink_metadata(&root)?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "Invalid environment storage directory"
        );
        let manager = Self {
            root: root.canonicalize()?,
            artifacts: config.artifacts.clone(),
        };
        if !manager.root.join("profiles.json").exists() {
            return Ok(None);
        }
        manager.database()?;
        Ok(Some(manager))
    }
    pub async fn open(config: &CapabilityConfig, cancel: &CancellationToken) -> Result<Self> {
        let management = config
            .management_root
            .as_ref()
            .context("Native environment management is not configured")?;
        ensure!(
            management.is_absolute(),
            "Native management storage must be an absolute path"
        );
        private_directory(management)?;
        let root = management.join("environments");
        private_directory(&root)?;
        private_directory(&root.join("operations"))?;
        private_directory(&root.join("leases"))?;
        private_directory(&root.join("operation-leases"))?;
        ensure!(
            config.artifacts.is_absolute(),
            "Environment output requires an absolute artifact directory"
        );
        if !config.artifacts.exists() {
            let parent = config
                .artifacts
                .parent()
                .context("Artifact directory has no parent")?;
            let name = config
                .artifacts
                .file_name()
                .and_then(|name| name.to_str())
                .context("Artifact directory name must be UTF-8")?;
            capture_fs::Directory::open(parent)?.child(name, true)?;
        }
        capture_fs::Directory::open(&config.artifacts)?;
        let artifacts = config.artifacts.canonicalize()?;
        let management = management.canonicalize()?;
        ensure!(
            !artifacts.starts_with(&management) && !management.starts_with(&artifacts),
            "Environment management and output artifact directories must be separate"
        );
        let manager = Self {
            root: root.canonicalize()?,
            artifacts,
        };
        let _lease = manager.database_lease(cancel).await?;
        if !manager.root.join("profiles.json").exists() {
            manager.write_database(&Database {
                version: 1,
                installation: Uuid::new_v4(),
                records: BTreeMap::new(),
            })?;
        }
        manager.database()?;
        Ok(manager)
    }
    fn database(&self) -> Result<Database> {
        let database: Database = read_json(&self.root.join("profiles.json"), MAX_DATABASE_BYTES)?;
        ensure!(
            database.version == 1
                && !database.installation.is_nil()
                && database.records.len() <= MAX_RECORDS,
            "Unsupported or oversized native environment database"
        );
        for (id, stored) in &database.records {
            ensure!(
                *id == stored.record.profile.id && stored.record.revision > 0,
                "Inconsistent native environment identity"
            );
        }
        Ok(database)
    }
    fn write_database(&self, database: &Database) -> Result<()> {
        write_json(
            &self.root.join("profiles.json"),
            database,
            MAX_DATABASE_BYTES,
        )
    }
    async fn database_lease(&self, cancel: &CancellationToken) -> Result<File> {
        lease(self.root.join("database.lock"), cancel).await
    }
    pub async fn profile_lease(&self, id: Uuid, cancel: &CancellationToken) -> Result<File> {
        lease(self.root.join("leases").join(format!("{id}.lock")), cancel).await
    }
    pub fn try_profile_lease(&self, id: Uuid) -> Result<Option<File>> {
        let file = open_lock(&self.root.join("leases").join(format!("{id}.lock")))?;
        Ok(try_lease(&file)?.then_some(file))
    }
    pub async fn operation_lease(&self, id: Uuid, cancel: &CancellationToken) -> Result<File> {
        ensure!(!id.is_nil(), "Choose a valid request identity");
        lease(
            self.root
                .join("operation-leases")
                .join(format!("{id}.lock")),
            cancel,
        )
        .await
    }
    pub fn try_operation_lease(&self, id: Uuid) -> Result<Option<File>> {
        ensure!(!id.is_nil(), "Choose a valid request identity");
        let file = open_lock(
            &self
                .root
                .join("operation-leases")
                .join(format!("{id}.lock")),
        )?;
        Ok(try_lease(&file)?.then_some(file))
    }
    pub fn cancelled(&self, id: Uuid) -> Result<Option<EnvironmentReceipt>> {
        archive::cancelled(self, id)
    }
    pub async fn cancel_undispatched(
        &self,
        receipt: &EnvironmentReceipt,
        cancel: &CancellationToken,
    ) -> Result<EnvironmentReceipt> {
        let _lease = self.database_lease(cancel).await?;
        archive::cancel_undispatched(self, receipt)
    }
    pub async fn installation(&self, cancel: &CancellationToken) -> Result<Uuid> {
        let _lease = self.database_lease(cancel).await?;
        Ok(self.database()?.installation)
    }
    pub async fn records(&self, cancel: &CancellationToken) -> Result<Vec<EnvironmentRecord>> {
        let _lease = self.database_lease(cancel).await?;
        Ok(self
            .database()?
            .records
            .into_values()
            .filter(|stored| !stored.removed)
            .map(|stored| stored.record)
            .collect())
    }
    pub async fn stored(&self, id: Uuid, cancel: &CancellationToken) -> Result<Option<Stored>> {
        let _lease = self.database_lease(cancel).await?;
        Ok(self.database()?.records.remove(&id))
    }
    pub async fn record(&self, id: Uuid, cancel: &CancellationToken) -> Result<EnvironmentRecord> {
        self.stored(id, cancel)
            .await?
            .filter(|stored| !stored.removed)
            .map(|stored| stored.record)
            .context("This environment profile no longer exists")
    }
    pub async fn commit(
        &self,
        record: EnvironmentRecord,
        removed: bool,
        operation: Uuid,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let _lease = self.database_lease(cancel).await?;
        let mut database = self.database()?;
        ensure!(
            database.records.contains_key(&record.profile.id)
                || database.records.len() < MAX_RECORDS,
            "Native environment profile history is full; existing profiles are preserved"
        );
        ensure!(
            removed
                || database
                    .records
                    .get(&record.profile.id)
                    .is_some_and(|stored| !stored.removed)
                || database
                    .records
                    .values()
                    .filter(|stored| !stored.removed)
                    .count()
                    < MAX_PROFILES,
            "At most 128 native environment profiles can be saved"
        );
        database.records.insert(
            record.profile.id,
            Stored {
                record,
                removed,
                last_operation: operation,
            },
        );
        self.write_database(&database)
    }
    pub fn operation(&self, id: Uuid) -> Result<Option<Operation>> {
        let path = self.root.join("operations").join(format!("{id}.json"));
        if !path.exists() {
            return match archive::operation(self, id)? {
                Some(operation) => Ok(Some(operation)),
                None => capture::retained_operation(self, id),
            };
        }
        let operation: Operation = match read_json(&path, MAX_RECEIPT_BYTES) {
            Ok(operation) => operation,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                // Archival publishes and syncs its complete indexed copy before
                // removing the active file. A reader may have observed the old
                // directory entry immediately before that removal.
                return match archive::operation(self, id)? {
                    Some(operation) => Ok(Some(operation)),
                    None => capture::retained_operation(self, id),
                };
            }
            Err(error) => return Err(error),
        };
        ensure!(
            operation.receipt.operation_id == id
                && operation.receipt.environment_id == operation.profile.id,
            "Inconsistent native environment operation identity"
        );
        Ok(Some(operation))
    }
    pub async fn write_operation(
        &self,
        operation: &Operation,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let _lease = self.database_lease(cancel).await?;
        ensure!(
            self.cancelled(operation.receipt.operation_id)?.is_none(),
            "This request was cancelled before dispatch; use a new identity only for an intentional new action"
        );
        let path = self
            .root
            .join("operations")
            .join(format!("{}.json", operation.receipt.operation_id));
        if let Some(previous) = self.operation(operation.receipt.operation_id)? {
            ensure!(
                previous.digest == operation.digest && previous.profile.id == operation.profile.id,
                "An environment operation identity was reused for different arguments"
            );
        }
        if !path.exists() {
            let mut files =
                fs::read_dir(self.root.join("operations"))?.collect::<std::io::Result<Vec<_>>>()?;
            let mut total = files.iter().try_fold(0u64, |total, file| {
                Ok::<_, anyhow::Error>(total.saturating_add(file.metadata()?.len()))
            })?;
            // Keep ordinary admission/listing independent of lifetime history.
            // Only unresolved operations may grow beyond this small window.
            if files.len() >= 64 || total + MAX_RECEIPT_BYTES > 64 * 1024 * 1024 {
                archive::maintain(self, 32, cancel)?;
                files = fs::read_dir(self.root.join("operations"))?
                    .collect::<std::io::Result<Vec<_>>>()?;
                total = files.iter().try_fold(0u64, |total, file| {
                    Ok::<_, anyhow::Error>(total.saturating_add(file.metadata()?.len()))
                })?;
            }
            ensure!(
                files.len() < MAX_OPERATIONS,
                "Native environment receipts are full of unresolved operations; inspect their outcomes before continuing"
            );
            ensure!(
                total + MAX_RECEIPT_BYTES <= 64 * 1024 * 1024,
                "Native environment receipt storage is full; existing outcomes remain preserved"
            );
        }
        write_json(&path, operation, MAX_RECEIPT_BYTES)
    }
    pub async fn maintain(&self, limit: usize, cancel: &CancellationToken) -> Result<Value> {
        let _lease = self.database_lease(cancel).await?;
        archive::maintain(self, limit, cancel)
    }
    pub fn operations(&self, id: Uuid) -> Result<Vec<Operation>> {
        Ok(self
            .all_operations()?
            .into_iter()
            .filter(|operation| operation.profile.id == id)
            .collect())
    }
    pub fn all_operations(&self) -> Result<Vec<Operation>> {
        let mut result = Vec::new();
        for file in fs::read_dir(self.root.join("operations"))?.take(MAX_OPERATIONS + 1) {
            let file = file?;
            if file.path().extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let operation: Operation = read_json(&file.path(), MAX_RECEIPT_BYTES)?;
            result.push(operation);
        }
        let present: std::collections::BTreeSet<_> = result
            .iter()
            .map(|operation| operation.receipt.operation_id)
            .collect();
        for id in capture::pending_ids(self)? {
            if !present.contains(&id)
                && let Some(operation) = self.operation(id)?
            {
                result.push(operation);
            }
        }
        ensure!(
            result.len() <= MAX_OPERATIONS,
            "Native environment operation count exceeds its bound"
        );
        Ok(result)
    }
}

#[cfg(test)]
mod lease_tests {
    use super::*;

    #[tokio::test]
    async fn a_live_environment_lease_remains_held_while_a_waiting_caller_cancels() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("operation.lock");
        let owner = open_lock(&path).unwrap();
        assert!(try_lease(&owner).unwrap());
        assert!(!try_lease(&open_lock(&path).unwrap()).unwrap());
        let cancel = CancellationToken::new();
        let waiting = lease(path.clone(), &cancel);
        tokio::pin!(waiting);
        tokio::select! {
            result = &mut waiting => panic!("A live lease must keep the caller queued: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(30)) => {},
        }
        cancel.cancel();
        let error = tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .expect("A queued lease must observe cancellation")
            .unwrap_err();
        assert!(error.to_string().contains("cancelled before admission"));
        assert!(!try_lease(&open_lock(&path).unwrap()).unwrap());
        drop(owner);
        drop(lease(path, &CancellationToken::new()).await.unwrap());
    }
}
