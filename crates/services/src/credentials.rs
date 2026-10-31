//! Encrypted secret vault for provider keys, tokens and connector secrets.
//!
//! Every secret lives in one file, `<data>/secrets.vault`, encrypted as a whole
//! with XChaCha20-Poly1305 from the RustCrypto `chacha20poly1305` crate. That
//! crate is maintained, audited and pure Rust, so the default vault works the
//! same on every platform with no OS prompt or service dependency. The extended
//! 192-bit nonce makes a fresh random nonce per write safe without a counter,
//! and the Poly1305 tag (bound to a versioned header as associated data) means
//! a wrong key, truncation or any tampering fails closed instead of yielding
//! garbage.
//!
//! The 32-byte vault key comes from one of three places:
//! - [`SecretStorage::LocalFile`]: `<data>/secrets.key`, created with
//!   owner-only permissions.
//! - [`SecretStorage::OsKeychain`]: a single item in the OS credential store,
//!   read once per vault instance.
//! - [`CredentialVault::unlocked`]: supplied by the host to a worker, which
//!   therefore never opens the keychain or the key file.
//!
//! The host records which of the first two holds the key in
//! `<data>/secrets.storage`, next to the vault (see the `key` module). That
//! record decides where an existing vault's key is read; the storage a caller
//! asks for only applies to a profile with no record yet, and
//! [`CredentialVault::storage`] reports the storage actually in use.
//!
//! [`CredentialVault::change_storage`] starts over in the other storage. A
//! new key is created and read back in the new place before anything is
//! deleted; then the vault file is deleted, the record updated, and the old
//! key removed. Saved secrets are discarded; the user enters them again. A
//! failure before the switch completes leaves the old key in use.
//!
//! [`CredentialVault::open`] (the host) loads the key and reads the vault at
//! once. A vault that can never be read with the available key (a wrong or
//! corrupt key, a missing key next to an existing vault, a damaged or
//! tampered file) is deleted and replaced by an empty one with a new key, and
//! [`CredentialVault::reset_notice`] says so; the user reconnects providers.
//! When there is no record of where the key lives, both places are tried
//! before anything is deleted. Transient failures (a locked keychain, an I/O
//! error) are returned as errors and delete nothing. A worker vault from
//! [`CredentialVault::unlocked`] performs no I/O at construction and never
//! deletes anything: an unreadable vault is an error there.
//!
//! Decrypted entries are cached per instance (shared by clones) and reloaded
//! when the vault file changes on disk. Writes take an exclusive lock on
//! `<data>/secrets.vault.lock`, re-read the file, and replace it atomically, so
//! concurrent writers in the host and workers never lose each other's entries.
//! All operations may block (file locks, keychain dialogs): call them off the
//! UI thread.

mod file;
mod key;

use anyhow::{Context, Result, anyhow, ensure};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
};
use xylo_core::SecretStorage;
use zeroize::Zeroizing;

use file::{FileStamp, VaultFile};
use key::{HostKey, KeySource};

/// Application secrets, stored in the encrypted vault described in the module
/// documentation. Clones share one key and one decrypted cache.
#[derive(Clone)]
pub struct CredentialVault {
    vault: Arc<Vault>,
}

struct Vault {
    file: VaultFile,
    key: KeySource,
    cache: Mutex<Cache>,
    /// Set when opening replaced an unreadable vault.
    reset: std::sync::OnceLock<String>,
    /// The private directory of a [`CredentialVault::for_tests`] vault,
    /// removed when the last clone drops.
    #[cfg(any(test, feature = "test-support"))]
    _scratch: Option<tempfile::TempDir>,
}

/// Decrypted entries and the on-disk version they were read from.
#[derive(Default)]
struct Cache {
    stamp: Option<FileStamp>,
    loaded: bool,
    entries: BTreeMap<String, Zeroizing<String>>,
}

/// The 32-byte key that decrypts the secret vault. The host reads it once and
/// passes it to each worker over the private stdin pipe; it is never logged,
/// persisted in settings or placed in arguments or environment variables.
/// The bytes are zeroed when the last copy is dropped.
#[derive(Clone)]
pub struct VaultKey(Zeroizing<[u8; 32]>);

impl VaultKey {
    /// Wraps raw key bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// The raw key bytes. Do not copy them into unprotected storage.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// A fresh key from the operating system's random source.
    pub(crate) fn generate() -> Result<Self> {
        let mut bytes = Zeroizing::new([0u8; 32]);
        getrandom::fill(bytes.as_mut_slice())
            .map_err(|error| anyhow!("Generate vault key: {error}"))?;
        Ok(Self(bytes))
    }

    /// Parses exactly 32 key bytes; any other length is corrupt key material.
    pub(crate) fn from_slice(bytes: &[u8]) -> Result<Self> {
        ensure!(bytes.len() == 32, "Vault key has the wrong length");
        let mut key = Zeroizing::new([0u8; 32]);
        key.copy_from_slice(bytes);
        Ok(Self(key))
    }
}

impl std::fmt::Debug for VaultKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VaultKey([REDACTED])")
    }
}

impl From<&VaultKey> for xylo_core::WorkerCredential {
    fn from(key: &VaultKey) -> Self {
        xylo_core::WorkerCredential(Some(hex::encode(key.as_bytes())))
    }
}

impl TryFrom<&xylo_core::WorkerCredential> for VaultKey {
    type Error = anyhow::Error;
    fn try_from(credential: &xylo_core::WorkerCredential) -> Result<Self> {
        let text = credential.0.as_deref().context("Vault key is missing")?;
        let bytes = Zeroizing::new(hex::decode(text).context("Vault key is malformed")?);
        Self::from_slice(&bytes)
    }
}

impl CredentialVault {
    /// Opens the application's secret vault in the host process.
    ///
    /// The vault key is read from the place the vault's record names;
    /// `storage` is used only when there is no record yet. The key is loaded
    /// (or created) and the vault read immediately. An unreadable vault is
    /// replaced by an empty one; see [`CredentialVault::reset_notice`].
    pub fn open(data_dir: &Path, storage: SecretStorage) -> Result<Self> {
        Self::open_host(data_dir, storage, key::Keychain::platform())
    }

    fn open_host(data_dir: &Path, storage: SecretStorage, keychain: key::Keychain) -> Result<Self> {
        let vault = Self::host(data_dir, storage, keychain);
        vault.vault.open(storage)?;
        Ok(vault)
    }

    /// Discards the vault and starts an empty one with a new key in
    /// `storage`, whatever the record says. For a host whose vault cannot be
    /// opened at all when the user chooses a different storage; saved secrets
    /// are lost. The old key is removed when its place can be reached.
    pub fn start_over(data_dir: &Path, storage: SecretStorage) -> Result<Self> {
        Self::start_over_host(data_dir, storage, key::Keychain::platform())
    }

    fn start_over_host(
        data_dir: &Path,
        storage: SecretStorage,
        keychain: key::Keychain,
    ) -> Result<Self> {
        let vault = Self::host(data_dir, storage, keychain);
        vault.vault.start_over(storage)?;
        Ok(vault)
    }

    /// Where the record in `data_dir` says the vault key lives, or `None`
    /// when there is no usable record. Reads only the record.
    pub fn recorded_storage(data_dir: &Path) -> Result<Option<SecretStorage>> {
        key::read_record(data_dir)
    }

    /// Where this host vault's key lives, or `None` for a worker vault whose
    /// key was supplied.
    pub fn storage(&self) -> Option<SecretStorage> {
        self.vault.key.host().and_then(|host| host.storage().ok())
    }

    /// Set when [`CredentialVault::open`] found saved keys it could never read
    /// and replaced them with an empty vault. The host shows it once.
    pub fn reset_notice(&self) -> Option<String> {
        self.vault.reset.get().cloned()
    }

    /// Switches the vault key to the place `storage` names (the key file in
    /// `data_dir`, or the OS keychain item) by starting over: a new key is
    /// created there and read back, then the vault file is deleted, the
    /// record updated and the old key removed. Every saved secret is
    /// discarded. Asking for the storage already in use changes nothing. On
    /// error the old key stays in use. Only a host vault opened with
    /// [`CredentialVault::open`] can do this; `data_dir` must be the
    /// directory it was opened with. Workers holding the old key must be
    /// restarted.
    pub fn change_storage(&self, data_dir: &Path, storage: SecretStorage) -> Result<()> {
        self.vault.change_storage(data_dir, storage)
    }

    /// Opens the vault with a key the host supplied. Never touches the OS
    /// credential store or the key file.
    pub fn unlocked(data_dir: &Path, key: VaultKey) -> Result<Self> {
        Ok(Self::vault(data_dir, KeySource::Supplied(key)))
    }

    /// The key a worker needs to open this vault, loading or creating it if
    /// this instance has not used it yet.
    pub fn export_key(&self) -> Result<VaultKey> {
        self.vault.key.load(&self.vault.file)
    }

    /// Returns the secret stored under `reference`, if any.
    pub fn get(&self, reference: &str) -> Result<Option<String>> {
        validate_reference(reference)?;
        self.vault.get(reference)
    }

    /// Stores `secret` under `reference`, replacing any previous value.
    pub fn set(&self, reference: &str, secret: &str) -> Result<()> {
        validate_reference(reference)?;
        ensure!(!secret.is_empty(), "Secret cannot be empty");
        self.vault.update(|entries| {
            entries.insert(reference.to_owned(), Zeroizing::new(secret.to_owned()));
        })
    }

    /// Removes the secret under `reference`. Removing an absent entry is not
    /// an error.
    pub fn delete(&self, reference: &str) -> Result<()> {
        validate_reference(reference)?;
        self.vault.update(|entries| {
            entries.remove(reference);
        })
    }

    fn host(data_dir: &Path, storage: SecretStorage, keychain: key::Keychain) -> Self {
        Self::vault(
            data_dir,
            KeySource::Host(HostKey::new(data_dir, storage, keychain)),
        )
    }

    fn vault(data_dir: &Path, key: KeySource) -> Self {
        Self {
            vault: Arc::new(Vault {
                file: VaultFile::new(data_dir),
                key,
                cache: Mutex::new(Cache::default()),
                reset: std::sync::OnceLock::new(),
                #[cfg(any(test, feature = "test-support"))]
                _scratch: None,
            }),
        }
    }

    /// A worker-style vault for tests: a fixed key and a private temporary
    /// directory that the vault owns and removes when its last clone drops.
    /// It never touches the OS keychain or a key file.
    ///
    /// # Panics
    /// When the temporary directory cannot be created.
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_tests() -> Self {
        let scratch = tempfile::tempdir().expect("create test vault directory");
        Self {
            vault: Arc::new(Vault {
                file: VaultFile::new(scratch.path()),
                key: KeySource::Supplied(VaultKey::from_bytes([0x5a; 32])),
                cache: Mutex::new(Cache::default()),
                reset: std::sync::OnceLock::new(),
                _scratch: Some(scratch),
            }),
        }
    }

    /// A host vault whose key lives in an injected keychain store. Unlike
    /// [`CredentialVault::open`] it does no I/O until first use, so tests can
    /// observe keychain access from the first operation.
    #[cfg(test)]
    pub(crate) fn with_keychain_store(
        data_dir: &Path,
        store: Arc<keyring_core::CredentialStore>,
    ) -> Self {
        Self::host(
            data_dir,
            SecretStorage::OsKeychain,
            key::Keychain::with_store(store),
        )
    }

    /// [`CredentialVault::open`] with an injected keychain store.
    #[cfg(test)]
    pub(crate) fn open_with_keychain_store(
        data_dir: &Path,
        storage: SecretStorage,
        store: Arc<keyring_core::CredentialStore>,
    ) -> Result<Self> {
        Self::open_host(data_dir, storage, key::Keychain::with_store(store))
    }

    /// [`CredentialVault::start_over`] with an injected keychain store.
    #[cfg(test)]
    pub(crate) fn start_over_with_keychain_store(
        data_dir: &Path,
        storage: SecretStorage,
        store: Arc<keyring_core::CredentialStore>,
    ) -> Result<Self> {
        Self::start_over_host(data_dir, storage, key::Keychain::with_store(store))
    }
}

/// The notice shown once after an unreadable vault was replaced.
const RESET_NOTICE: &str =
    "Saved keys could not be read and were removed. Reconnect your providers.";

impl Vault {
    fn host_key(&self) -> Result<&HostKey> {
        self.key
            .host()
            .context("Only the host can change where the vault key is stored")
    }

    /// Finds the key that opens the existing vault: the one in the recorded
    /// place, or with no record the one in `requested` and then the other
    /// place. Without a vault file there is nothing to strand, so the key is
    /// read or created in the recorded (else requested) place. A vault no
    /// candidate key can ever read is deleted and an empty one started.
    fn open(&self, requested: SecretStorage) -> Result<()> {
        let host = self.host_key()?;
        let mut cache = lock(&self.cache)?;
        let _guard = self.file.lock_exclusive()?;
        let dir = self.file.dir();
        let recorded = key::read_record(dir)?;
        let preferred = recorded.unwrap_or(requested);
        let record = |storage| {
            if recorded == Some(storage) {
                Ok(())
            } else {
                key::write_record(dir, storage)
            }
        };
        if !self.file.exists()? {
            // Recorded before the key is created, so any vault written with
            // that key is always paired with the right record.
            record(preferred)?;
            host.adopt(preferred, None)?;
            host.load(&self.file)?;
            return Ok(());
        }
        let candidates = match recorded {
            Some(storage) => vec![storage],
            None => vec![requested, key::other(requested)],
        };
        let mut reason = None;
        for storage in candidates {
            match self.try_key(host, storage)? {
                Ok(key) => {
                    record(storage)?;
                    return host.adopt(storage, Some(key));
                }
                Err(error) => reason = Some(error),
            }
        }
        if let Some(reason) = reason {
            tracing::warn!("Replacing an unreadable secret vault: {reason:#}");
        }
        self.file.remove()?;
        *cache = Cache::default();
        let key = host.create(preferred)?;
        record(preferred)?;
        host.adopt(preferred, Some(key))?;
        let _ = self.reset.set(RESET_NOTICE.into());
        Ok(())
    }

    /// The key in `storage` when it opens the vault file. `Ok(Err(reason))`
    /// means it never will (missing, corrupt or wrong key, or a damaged
    /// file); a transient failure is an error.
    fn try_key(
        &self,
        host: &HostKey,
        storage: SecretStorage,
    ) -> Result<std::result::Result<VaultKey, anyhow::Error>> {
        let attempt = host
            .read(storage)
            .and_then(|key| key.ok_or_else(|| unreadable(format!("No vault key in {storage:?}"))))
            .and_then(|key| self.file.read(&key).map(|_| key));
        match attempt {
            Ok(key) => Ok(Ok(key)),
            Err(error) if is_unreadable(&error) => Ok(Err(error)),
            Err(error) => Err(error),
        }
    }

    /// Deletes the vault and starts an empty one with a new key in `storage`.
    fn start_over(&self, storage: SecretStorage) -> Result<()> {
        let host = self.host_key()?;
        let mut cache = lock(&self.cache)?;
        let _guard = self.file.lock_exclusive()?;
        self.file.remove()?;
        *cache = Cache::default();
        let key = host.create(storage)?;
        key::write_record(self.file.dir(), storage)?;
        host.adopt(storage, Some(key))?;
        remove_old_key(host, key::other(storage));
        Ok(())
    }

    fn change_storage(&self, data_dir: &Path, storage: SecretStorage) -> Result<()> {
        ensure!(
            self.file.dir() == data_dir,
            "The vault was opened from a different data folder"
        );
        let host = self.host_key()?;
        let current = host.storage()?;
        if current == storage {
            return Ok(());
        }
        let mut cache = lock(&self.cache)?;
        let _guard = self.file.lock_exclusive()?;
        // Nothing is deleted until the new key is known to work.
        let key = host.create(storage)?;
        self.file.remove()?;
        *cache = Cache::default();
        // Until the record is written the old key stays current, so a
        // failure leaves the record, the key and the setting in agreement.
        key::write_record(data_dir, storage)?;
        host.adopt(storage, Some(key))?;
        remove_old_key(host, current);
        Ok(())
    }

    fn get(&self, reference: &str) -> Result<Option<String>> {
        let key = self.key.load(&self.file)?;
        let mut cache = lock(&self.cache)?;
        self.refresh(&mut cache, &key, false)?;
        Ok(cache
            .entries
            .get(reference)
            .map(|secret| secret.as_str().to_owned()))
    }

    /// Read-modify-write under the cross-process lock. The file is always
    /// re-read first so a write from another process is never lost.
    fn update(&self, change: impl FnOnce(&mut BTreeMap<String, Zeroizing<String>>)) -> Result<()> {
        let key = self.key.load(&self.file)?;
        let mut cache = lock(&self.cache)?;
        let _guard = self.file.lock_exclusive()?;
        self.refresh(&mut cache, &key, true)?;
        let mut entries = cache.entries.clone();
        change(&mut entries);
        let stamp = self.file.write(&key, &entries)?;
        cache.entries = entries;
        cache.stamp = Some(stamp);
        cache.loaded = true;
        Ok(())
    }

    fn refresh(&self, cache: &mut Cache, key: &VaultKey, force: bool) -> Result<()> {
        if !force && cache.loaded && cache.stamp == self.file.stamp()? {
            return Ok(());
        }
        let (stamp, entries) = self.file.read(key)?;
        cache.stamp = stamp;
        cache.entries = entries;
        cache.loaded = true;
        Ok(())
    }
}

/// Vault data that exists but can never be read with the available key, as
/// opposed to a transient failure such as a locked keychain or an I/O error.
#[derive(Debug)]
struct Unreadable(String);

impl std::fmt::Display for Unreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unreadable {}

fn unreadable(message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(Unreadable(message.into()))
}

fn is_unreadable(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<Unreadable>())
}

fn validate_reference(reference: &str) -> Result<()> {
    ensure!(
        !reference.trim().is_empty() && reference.len() <= 256,
        "Invalid credential reference"
    );
    Ok(())
}

/// Deletes a key that no longer encrypts anything. The switch away from it is
/// already complete, so a failure is only logged: the stale key decrypts no
/// vault and is replaced if its place is chosen again.
fn remove_old_key(host: &HostKey, storage: SecretStorage) {
    if let Err(error) = host.delete(storage) {
        tracing::warn!("Remove the previous vault key: {error:#}");
    }
}

fn lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
    mutex
        .lock()
        .map_err(|_| anyhow!("Credential vault lock poisoned"))
}

#[cfg(test)]
mod tests;
