//! Where the vault key lives, and the record of that choice.
//!
//! A host vault keeps its key in one of two places ([`SecretStorage`]) and
//! records which one in `<data>/secrets.storage`, next to the vault. The
//! record, not the settings database, decides where the key of an existing
//! vault is read, so a reset or replaced database never strands saved
//! secrets. The record is written before a key is created in a new place and
//! only after any vault encrypted with an older key is gone, so whenever a
//! vault file exists the record names the place its key came from.

use super::{
    VaultKey,
    file::{VaultFile, create_private_dir, set_private, sync_dir},
    lock, unreadable,
};
use anyhow::{Context, Result, anyhow, ensure};
use std::{
    fs::File,
    io::{ErrorKind, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use xylo_core::SecretStorage;
use zeroize::Zeroizing;

pub(super) const KEY_FILE: &str = "secrets.key";
pub(super) const STORAGE_FILE: &str = "secrets.storage";
/// Account name of the single keychain item holding the vault key.
const KEYCHAIN_ACCOUNT: &str = "vault-key";

/// The key of one vault instance.
pub(super) enum KeySource {
    /// Supplied by the host to a worker; never read from or written to disk.
    Supplied(VaultKey),
    /// Owned by the host, which reads, creates and replaces it.
    Host(HostKey),
}

impl KeySource {
    /// Returns the key, reading or creating it on first use; see
    /// [`HostKey::load`].
    pub(super) fn load(&self, vault: &VaultFile) -> Result<VaultKey> {
        match self {
            Self::Supplied(key) => Ok(key.clone()),
            Self::Host(host) => host.load(vault),
        }
    }

    /// The host key, or `None` for a key supplied by the host.
    pub(super) fn host(&self) -> Option<&HostKey> {
        match self {
            Self::Supplied(_) => None,
            Self::Host(host) => Some(host),
        }
    }
}

/// A host vault key kept in the key file or the OS keychain.
pub(super) struct HostKey {
    key_file: PathBuf,
    keychain: Keychain,
    /// Where the current key lives. Changes only through [`HostKey::adopt`].
    storage: Mutex<SecretStorage>,
    /// Holding this lock while loading serializes keychain prompts and key
    /// creation within the process.
    cached: Mutex<Option<VaultKey>>,
}

impl HostKey {
    pub(super) fn new(data_dir: &Path, storage: SecretStorage, keychain: Keychain) -> Self {
        Self {
            key_file: data_dir.join(KEY_FILE),
            keychain,
            storage: Mutex::new(storage),
            cached: Mutex::new(None),
        }
    }

    /// Where the current key lives.
    pub(super) fn storage(&self) -> Result<SecretStorage> {
        Ok(*lock(&self.storage)?)
    }

    /// Returns the key, reading or creating it on first use. A new key is
    /// never created while a vault file exists: that vault could not be
    /// decrypted with it, so the caller gets an error instead.
    pub(super) fn load(&self, vault: &VaultFile) -> Result<VaultKey> {
        let mut cached = lock(&self.cached)?;
        if let Some(key) = cached.as_ref() {
            return Ok(key.clone());
        }
        let storage = self.storage()?;
        let key = match self.read(storage)? {
            Some(key) => key,
            None if vault.exists()? => return Err(unreadable(missing_key(storage))),
            None => match storage {
                SecretStorage::LocalFile => publish_key_file(&self.key_file)?,
                SecretStorage::OsKeychain => {
                    let key = VaultKey::generate()?;
                    self.keychain.write(&key)?;
                    key
                }
            },
        };
        *cached = Some(key.clone());
        Ok(key)
    }

    /// Reads the key kept in `storage` without creating one. A key that is
    /// present but corrupt is an [`unreadable`] error.
    pub(super) fn read(&self, storage: SecretStorage) -> Result<Option<VaultKey>> {
        match storage {
            SecretStorage::LocalFile => read_key_file(&self.key_file),
            SecretStorage::OsKeychain => self.keychain.read(),
        }
    }

    /// Writes a fresh key to `storage`, replacing whatever key was there, and
    /// reads it back. Nothing else changes, so a failure here leaves the
    /// current key and vault intact. The caller makes the key current with
    /// [`HostKey::adopt`] once the vault it replaces is gone.
    pub(super) fn create(&self, storage: SecretStorage) -> Result<VaultKey> {
        let key = VaultKey::generate()?;
        match storage {
            SecretStorage::LocalFile => replace_key_file(&self.key_file, &key)?,
            SecretStorage::OsKeychain => self.keychain.write(&key)?,
        }
        let stored = self
            .read(storage)?
            .context("The new vault key could not be read back")?;
        ensure!(
            stored.as_bytes() == key.as_bytes(),
            "The new vault key could not be read back"
        );
        Ok(key)
    }

    /// Makes `storage` the place of the current key. `key` is the key there
    /// when known; otherwise the next load reads (or creates) it.
    pub(super) fn adopt(&self, storage: SecretStorage, key: Option<VaultKey>) -> Result<()> {
        let mut cached = lock(&self.cached)?;
        *lock(&self.storage)? = storage;
        *cached = key;
        Ok(())
    }

    /// Deletes the key kept in `storage`. A missing key is already deleted.
    pub(super) fn delete(&self, storage: SecretStorage) -> Result<()> {
        match storage {
            SecretStorage::LocalFile => remove_key_file(&self.key_file),
            SecretStorage::OsKeychain => self.keychain.delete(),
        }
    }
}

/// The other place a host key can live.
pub(super) fn other(storage: SecretStorage) -> SecretStorage {
    match storage {
        SecretStorage::LocalFile => SecretStorage::OsKeychain,
        SecretStorage::OsKeychain => SecretStorage::LocalFile,
    }
}

fn missing_key(storage: SecretStorage) -> &'static str {
    match storage {
        SecretStorage::LocalFile => {
            "The secret vault key file is missing, so the existing vault cannot be opened"
        }
        SecretStorage::OsKeychain => {
            "The secret vault key is missing from the OS keychain, so the existing vault cannot be opened"
        }
    }
}

/// Where the record in `data_dir` says the vault key lives. `None` when there
/// is no record or it cannot be understood: the key's place is then unknown.
pub(super) fn read_record(data_dir: &Path) -> Result<Option<SecretStorage>> {
    let text = match std::fs::read_to_string(data_dir.join(STORAGE_FILE)) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) if error.kind() == ErrorKind::InvalidData => return Ok(None),
        Err(error) => return Err(error).context("Read where the vault key is stored"),
    };
    Ok(match text.trim() {
        "local-file" => Some(SecretStorage::LocalFile),
        "os-keychain" => Some(SecretStorage::OsKeychain),
        other => {
            tracing::warn!(record = other, "Unrecognized secret storage record");
            None
        }
    })
}

/// Atomically records that the vault key lives in `storage`.
pub(super) fn write_record(data_dir: &Path, storage: SecretStorage) -> Result<()> {
    let text = match storage {
        SecretStorage::LocalFile => "local-file\n",
        SecretStorage::OsKeychain => "os-keychain\n",
    };
    create_private_dir(data_dir)?;
    let mut temp = tempfile::Builder::new()
        .prefix(".secrets.storage.")
        .tempfile_in(data_dir)
        .context("Record where the vault key is stored")?;
    set_private(temp.as_file())?;
    temp.write_all(text.as_bytes())
        .context("Record where the vault key is stored")?;
    temp.as_file()
        .sync_all()
        .context("Record where the vault key is stored")?;
    temp.persist(data_dir.join(STORAGE_FILE))
        .map_err(|error| anyhow!("Record where the vault key is stored: {}", error.error))?;
    sync_dir(data_dir)
}

/// The OS keychain item holding the vault key; the store opens on first use.
pub(super) struct Keychain(Mutex<Option<Arc<keyring_core::CredentialStore>>>);

impl Keychain {
    /// The platform keychain.
    pub(super) fn platform() -> Self {
        Self(Mutex::new(None))
    }

    /// An injected store in place of the platform keychain.
    #[cfg(test)]
    pub(super) fn with_store(store: Arc<keyring_core::CredentialStore>) -> Self {
        Self(Mutex::new(Some(store)))
    }

    fn entry(&self) -> Result<keyring_core::Entry> {
        let store = {
            let mut slot = lock(&self.0)?;
            match slot.as_ref() {
                Some(store) => store.clone(),
                None => slot.insert(platform_store()?).clone(),
            }
        };
        store
            .build(xylo_core::CREDENTIAL_SERVICE, KEYCHAIN_ACCOUNT, None)
            .map_err(|error| anyhow!("Open the vault key in the OS keychain: {error}"))
    }

    fn read(&self) -> Result<Option<VaultKey>> {
        match self.entry()?.get_secret() {
            Ok(bytes) => VaultKey::from_slice(&Zeroizing::new(bytes))
                .map(Some)
                .map_err(|_| unreadable("The vault key in the OS keychain is corrupt")),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(error) => Err(anyhow!("Read the vault key from the OS keychain: {error}")),
        }
    }

    fn write(&self, key: &VaultKey) -> Result<()> {
        self.entry()?
            .set_secret(key.as_bytes())
            .map_err(|error| anyhow!("Save the vault key to the OS keychain: {error}"))
    }

    fn delete(&self) -> Result<()> {
        match self.entry()?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(error) => Err(anyhow!(
                "Remove the vault key from the OS keychain: {error}"
            )),
        }
    }
}

fn remove_key_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => sync_dir(path.parent().context("Vault key path has no directory")?),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("Remove vault key file"),
    }
}

/// A private temporary file in the key's directory holding `key`, flushed.
fn staged_key_file(path: &Path, key: &VaultKey) -> Result<tempfile::NamedTempFile> {
    let dir = path.parent().context("Vault key path has no directory")?;
    create_private_dir(dir)?;
    let mut temp = tempfile::Builder::new()
        .prefix(".secrets.key.")
        .tempfile_in(dir)
        .context("Create vault key file")?;
    set_private(temp.as_file())?;
    temp.write_all(key.as_bytes())
        .context("Write vault key file")?;
    temp.as_file().sync_all().context("Flush vault key file")?;
    Ok(temp)
}

/// Creates the first key file. When two processes race, exactly one key wins
/// and the loser adopts it, so a published key is never replaced here.
fn publish_key_file(path: &Path) -> Result<VaultKey> {
    let key = VaultKey::generate()?;
    let dir = path.parent().context("Vault key path has no directory")?;
    match staged_key_file(path, &key)?.persist_noclobber(path) {
        Ok(_) => {
            sync_dir(dir)?;
            Ok(key)
        }
        Err(error) if error.error.kind() == ErrorKind::AlreadyExists => {
            read_key_file(path)?.context("Vault key file disappeared while it was created")
        }
        Err(error) => Err(anyhow!("Save vault key file: {}", error.error)),
    }
}

/// Atomically replaces the key file with `key`.
fn replace_key_file(path: &Path, key: &VaultKey) -> Result<()> {
    let dir = path.parent().context("Vault key path has no directory")?;
    staged_key_file(path, key)?
        .persist(path)
        .map_err(|error| anyhow!("Save vault key file: {}", error.error))?;
    sync_dir(dir)
}

fn read_key_file(path: &Path) -> Result<Option<VaultKey>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Open vault key file"),
    };
    let mut bytes = Zeroizing::new(Vec::with_capacity(33));
    (&mut file)
        .take(33)
        .read_to_end(&mut bytes)
        .context("Read vault key file")?;
    let key = VaultKey::from_slice(&bytes)
        .map_err(|_| unreadable("The secret vault key file is corrupt"))?;
    set_private(&file)?;
    Ok(Some(key))
}

/// The operating system's credential store.
fn platform_store() -> Result<Arc<keyring_core::CredentialStore>> {
    #[cfg(target_os = "macos")]
    let store: Arc<keyring_core::CredentialStore> =
        apple_native_keyring_store::keychain::Store::new()?;
    #[cfg(target_os = "windows")]
    let store: Arc<keyring_core::CredentialStore> = windows_native_keyring_store::Store::new()?;
    #[cfg(target_os = "linux")]
    let store: Arc<keyring_core::CredentialStore> =
        zbus_secret_service_keyring_store::Store::new()?;
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    let store: Arc<keyring_core::CredentialStore> =
        anyhow::bail!("No OS credential provider is available on this platform");
    Ok(store)
}
