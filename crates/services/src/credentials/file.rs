//! On-disk vault format and file I/O.
//!
//! Layout: `HEADER` (magic and format version, also the AEAD associated data),
//! a 24-byte random nonce, then the XChaCha20-Poly1305 ciphertext of a JSON
//! object mapping references to secrets.

use super::{VaultKey, unreadable};
use anyhow::{Context, Result, anyhow};
use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, Payload},
};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{ErrorKind, Read, Write},
    path::{Path, PathBuf},
    time::SystemTime,
};
use zeroize::Zeroizing;

pub(super) const VAULT_FILE: &str = "secrets.vault";
const LOCK_FILE: &str = "secrets.vault.lock";
const HEADER: &[u8; 8] = b"XYLOVLT1";
const NONCE_LEN: usize = 24;
/// Far above any realistic set of keys and tokens; bounds memory on a hostile file.
const MAX_VAULT_BYTES: u64 = 16 * 1024 * 1024;

type Entries = BTreeMap<String, Zeroizing<String>>;

/// Identity of one version of the vault file. Writes replace the file by
/// rename, so the inode changes on every write on Unix; size and modification
/// time cover other platforms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    inode: (u64, u64),
}

impl FileStamp {
    fn of(metadata: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            inode: (metadata.dev(), metadata.ino()),
        }
    }
}

pub(super) struct VaultFile {
    dir: PathBuf,
    path: PathBuf,
    lock_path: PathBuf,
}

/// Exclusive cross-process write lock; released when dropped.
pub(super) struct WriteLock {
    _file: File,
}

impl VaultFile {
    pub(super) fn new(data_dir: &Path) -> Self {
        Self {
            dir: data_dir.to_owned(),
            path: data_dir.join(VAULT_FILE),
            lock_path: data_dir.join(LOCK_FILE),
        }
    }

    /// The data directory holding the vault.
    pub(super) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Deletes the vault file. A missing file is already deleted.
    pub(super) fn remove(&self) -> Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => sync_dir(&self.dir),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("Delete secret vault"),
        }
    }

    pub(super) fn exists(&self) -> Result<bool> {
        Ok(self.stamp()?.is_some())
    }

    pub(super) fn stamp(&self) -> Result<Option<FileStamp>> {
        match std::fs::metadata(&self.path) {
            Ok(metadata) => Ok(Some(FileStamp::of(&metadata))),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("Inspect secret vault"),
        }
    }

    pub(super) fn lock_exclusive(&self) -> Result<WriteLock> {
        create_private_dir(&self.dir)?;
        let file = private_options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.lock_path)
            .context("Open secret vault lock")?;
        file.lock().context("Lock secret vault")?;
        Ok(WriteLock { _file: file })
    }

    /// Reads and decrypts the current file. A missing file is an empty vault;
    /// anything that does not authenticate is an error.
    pub(super) fn read(&self, key: &VaultKey) -> Result<(Option<FileStamp>, Entries)> {
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Ok((None, Entries::new()));
            }
            Err(error) => return Err(error).context("Open secret vault"),
        };
        // Stat the opened handle so the stamp describes exactly what is read.
        let metadata = file.metadata().context("Inspect secret vault")?;
        if metadata.len() > MAX_VAULT_BYTES {
            return Err(unreadable("Secret vault file is too large"));
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take(MAX_VAULT_BYTES + 1)
            .read_to_end(&mut bytes)
            .context("Read secret vault")?;
        Ok((Some(FileStamp::of(&metadata)), decrypt(key, &bytes)?))
    }

    /// Encrypts `entries` and atomically replaces the vault file. The caller
    /// holds the write lock.
    pub(super) fn write(&self, key: &VaultKey, entries: &Entries) -> Result<FileStamp> {
        let bytes = encrypt(key, entries)?;
        create_private_dir(&self.dir)?;
        let mut temp = tempfile::Builder::new()
            .prefix(".secrets.vault.")
            .tempfile_in(&self.dir)
            .context("Create secret vault update")?;
        set_private(temp.as_file())?;
        temp.write_all(&bytes)
            .context("Write secret vault update")?;
        temp.as_file()
            .sync_all()
            .context("Flush secret vault update")?;
        let file = temp
            .persist(&self.path)
            .map_err(|error| anyhow!("Replace secret vault: {}", error.error))?;
        let stamp = FileStamp::of(&std::fs::metadata(&self.path).context("Inspect secret vault")?);
        drop(file);
        sync_dir(&self.dir)?;
        Ok(stamp)
    }
}

fn cipher(key: &VaultKey) -> Result<XChaCha20Poly1305> {
    XChaCha20Poly1305::new_from_slice(key.as_bytes()).map_err(|_| anyhow!("Invalid vault key"))
}

pub(super) fn encrypt(key: &VaultKey, entries: &Entries) -> Result<Vec<u8>> {
    let plain: BTreeMap<&str, &str> = entries
        .iter()
        .map(|(reference, secret)| (reference.as_str(), secret.as_str()))
        .collect();
    let plaintext = Zeroizing::new(serde_json::to_vec(&plain).context("Encode secret vault")?);
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce).map_err(|error| anyhow!("Generate vault nonce: {error}"))?;
    let ciphertext = cipher(key)?
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: &plaintext,
                aad: HEADER,
            },
        )
        .map_err(|_| anyhow!("Encrypt secret vault"))?;
    let mut bytes = Vec::with_capacity(HEADER.len() + NONCE_LEN + ciphertext.len());
    bytes.extend_from_slice(HEADER);
    bytes.extend_from_slice(&nonce);
    bytes.extend_from_slice(&ciphertext);
    Ok(bytes)
}

pub(super) fn decrypt(key: &VaultKey, bytes: &[u8]) -> Result<Entries> {
    let body = bytes.strip_prefix(HEADER.as_slice()).ok_or_else(|| {
        unreadable("Secret vault file is not a Xylo vault or uses an unsupported format")
    })?;
    if body.len() <= NONCE_LEN {
        return Err(unreadable("Secret vault file is truncated"));
    }
    let (nonce, ciphertext) = body.split_at(NONCE_LEN);
    let nonce: [u8; NONCE_LEN] = nonce
        .try_into()
        .map_err(|_| unreadable("Secret vault file is truncated"))?;
    let plaintext = Zeroizing::new(
        cipher(key)?
            .decrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: ciphertext,
                    aad: HEADER,
                },
            )
            .map_err(|_| {
                unreadable(
                    "Secret vault cannot be decrypted: the key is wrong or the file was modified",
                )
            })?,
    );
    let plain: BTreeMap<String, String> = serde_json::from_slice(&plaintext)
        .map_err(|_| unreadable("Secret vault contents are malformed"))?;
    Ok(plain
        .into_iter()
        .map(|(reference, secret)| (reference, Zeroizing::new(secret)))
        .collect())
}

/// Options that create files readable only by the owner.
pub(super) fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

/// Forces owner-only permissions; creation modes are subject to the umask.
pub(super) fn set_private(file: &File) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .context("Restrict secret file permissions")?;
    }
    #[cfg(not(unix))]
    let _ = file;
    Ok(())
}

pub(super) fn create_private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).context("Create secret vault directory")
}

/// Makes a completed rename durable.
pub(super) fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(dir)
        .and_then(|directory| directory.sync_all())
        .context("Flush secret vault directory")?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}
