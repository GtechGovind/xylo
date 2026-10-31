//! No-follow directory authority for operation-owned output, never caller paths.
use anyhow::{Result, ensure};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
};
use uuid::Uuid;

#[cfg(unix)]
#[path = "capture_fs_unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "capture_fs_windows.rs"]
mod platform;

#[derive(Clone)]
pub(super) struct Directory {
    authority: Arc<platform::Authority>,
    pub path: PathBuf,
}

fn basename(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 128
            && name != "."
            && name != ".."
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte)),
        "Invalid output storage entry"
    );
    Ok(())
}

impl Directory {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            authority: Arc::new(platform::Authority::open(path)?),
            path: path.to_path_buf(),
        })
    }

    pub fn child(&self, name: &str, create: bool) -> Result<Self> {
        basename(name)?;
        Ok(Self {
            authority: Arc::new(self.authority.child(name, create)?),
            path: self.path.join(name),
        })
    }

    pub fn file(&self, name: &str, create: bool) -> Result<File> {
        basename(name)?;
        self.authority.file(name, create)
    }

    pub fn file_for_sync(&self, name: &str) -> Result<File> {
        basename(name)?;
        self.authority.file_for_sync(name)
    }

    pub fn lock_file(&self, name: &str) -> Result<File> {
        basename(name)?;
        self.authority.lock_file(name)
    }

    pub fn exists(&self, name: &str) -> Result<bool> {
        match self.file(name, false) {
            Ok(_) => Ok(true),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    pub fn read(&self, name: &str, maximum: usize) -> Result<Vec<u8>> {
        let mut file = self.file(name, false)?;
        let mut bytes = Vec::new();
        (&mut file)
            .take(maximum as u64 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= maximum,
            "Output metadata exceeds its byte budget"
        );
        Ok(bytes)
    }

    pub fn write(&self, name: &str, bytes: &[u8], maximum: usize) -> Result<()> {
        ensure!(
            bytes.len() <= maximum,
            "Output metadata exceeds its byte budget"
        );
        let staging = format!("{}.pending", Uuid::new_v4());
        let mut file = self.file(&staging, true)?;
        let result = (|| {
            file.write_all(bytes)?;
            file.sync_all()?;
            self.authority
                .rename(&file, &staging, &self.authority, name, true)?;
            self.authority.sync()?;
            Ok(())
        })();
        if result.is_err() {
            drop(file);
            let _ = self.authority.unlink(&staging);
        }
        result
    }

    pub fn publish(&self, name: &str, file: &File, target: &Self, target_name: &str) -> Result<()> {
        basename(name)?;
        basename(target_name)?;
        platform::regular(file)?;
        self.authority
            .rename(file, name, &target.authority, target_name, false)?;
        let published = target.file(target_name, false)?;
        ensure!(
            platform::same_file(file, &published)?,
            "Published output identity changed"
        );
        target.authority.sync()?;
        self.authority.sync()?;
        Ok(())
    }

    pub fn verify_named(&self) -> Result<()> {
        let current = Self::open(&self.path)?;
        ensure!(
            self.authority.same(&current.authority)?,
            "Output directory identity changed"
        );
        Ok(())
    }

    /// Recover only the two exact aliases created by Unix no-clobber publish.
    /// No extra link, unrelated destination or changed bytes authorize cleanup.
    pub fn resume_publication(
        &self,
        name: &str,
        target: &Self,
        target_name: &str,
        expected_hash: &str,
        expected_bytes: u64,
    ) -> Result<bool> {
        basename(name)?;
        basename(target_name)?;
        let mut source = match self.authority.recovery_file(name) {
            Ok(file) => file,
            Err(error) if missing(&error) => return Ok(false),
            Err(error) => return Err(error),
        };
        let destination = match target.authority.recovery_file(target_name) {
            Ok(file) => file,
            Err(error) if missing(&error) => {
                platform::regular(&source)?;
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        ensure!(
            platform::same_file(&source, &destination)?,
            "Output destination belongs to unrelated content"
        );
        platform::regular_pair(&source)?;
        platform::regular_pair(&destination)?;
        let (hash, bytes) = hash_file_inner(&mut source, expected_bytes, true)?;
        ensure!(
            hash == expected_hash && bytes == expected_bytes,
            "Interrupted output publication contents changed"
        );
        let current = self.authority.recovery_file(name)?;
        ensure!(
            platform::same_file(&source, &current)?,
            "Output recovery staging identity changed"
        );
        platform::regular_pair(&current)?;
        self.authority.unlink(name)?;
        platform::regular(&destination)?;
        target.authority.sync()?;
        self.authority.sync()?;
        Ok(true)
    }
}

pub(super) fn hash_file(file: &mut File, maximum: u64) -> Result<(String, u64)> {
    hash_file_inner(file, maximum, false)
}

fn missing(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

fn hash_file_inner(file: &mut File, maximum: u64, pair: bool) -> Result<(String, u64)> {
    use sha2::{Digest, Sha256};
    if pair {
        platform::regular_pair(file)?;
    } else {
        platform::regular(file)?;
    }
    ensure!(
        file.metadata()?.len() <= maximum,
        "Retained output exceeds its stream budget"
    );
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut length = 0_u64;
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        length = length.saturating_add(count as u64);
        ensure!(
            length <= maximum,
            "Retained output grew beyond its stream budget"
        );
        hasher.update(&chunk[..count]);
    }
    if pair {
        platform::regular_pair(file)?;
    } else {
        platform::regular(file)?;
    }
    Ok((hex::encode(hasher.finalize()), length))
}
