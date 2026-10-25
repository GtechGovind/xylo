use anyhow::{Result, ensure};
use std::{
    ffi::{CString, OsStr},
    fs::File,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, PermissionsExt},
        },
    },
    path::{Component, Path},
};

pub(super) struct Authority {
    // Keep every ancestor handle. All later IO uses the final handle, even if
    // an entry is concurrently renamed; no checked pathname is reopened.
    handles: Vec<File>,
}

fn owned_directory(file: &File, private: bool) -> Result<()> {
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_dir() && metadata.uid() == unsafe { libc::geteuid() },
        "Output storage must be a directory owned by this account"
    );
    ensure!(
        metadata.permissions().mode() & if private { 0o077 } else { 0o022 } == 0,
        "Output storage directory permissions are unsafe"
    );
    Ok(())
}

fn name(value: &OsStr) -> Result<CString> {
    ensure!(
        !value.as_bytes().is_empty() && !value.as_bytes().contains(&b'/'),
        "Invalid output path component"
    );
    Ok(CString::new(value.as_bytes())?)
}

fn open_relative(parent: &File, component: &OsStr, directory: bool, create: bool) -> Result<File> {
    let component = name(component)?;
    if directory && create {
        let result = unsafe { libc::mkdirat(parent.as_raw_fd(), component.as_ptr(), 0o700) };
        if result == 0 {
            // Sync the new child name in its parent before any durable ledger
            // can point at data below it. Leaf-file fsync alone is insufficient.
            parent.sync_all()?;
        } else {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(error.into());
            }
        }
    }
    let flags = libc::O_NOFOLLOW
        | libc::O_CLOEXEC
        | libc::O_NONBLOCK
        | if directory {
            libc::O_RDONLY | libc::O_DIRECTORY
        } else {
            libc::O_RDWR
        }
        | if create && !directory {
            libc::O_CREAT | libc::O_EXCL
        } else {
            0
        };
    let descriptor = unsafe { libc::openat(parent.as_raw_fd(), component.as_ptr(), flags, 0o600) };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: the new descriptor is transferred into exactly one File.
    let file = unsafe { File::from_raw_fd(descriptor) };
    if directory {
        ensure!(
            file.metadata()?.is_dir(),
            "Output authority component is not a directory"
        );
    } else {
        regular(&file)?;
    }
    Ok(file)
}

impl Authority {
    fn directory(&self) -> &File {
        self.handles.last().expect("Output authority has a root")
    }

    pub fn open(path: &Path) -> Result<Self> {
        ensure!(
            path.is_absolute(),
            "Output storage requires an absolute application path"
        );
        use std::os::unix::fs::OpenOptionsExt;
        let root = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open("/")?;
        let mut authority = Self {
            handles: vec![root],
        };
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(part) => {
                    let next = open_relative(authority.directory(), part, true, false)?;
                    authority.handles.push(next);
                }
                _ => anyhow::bail!("Output authority cannot contain traversal or symbolic links"),
            }
        }
        owned_directory(authority.directory(), false)?;
        Ok(authority)
    }

    pub fn child(&self, name: &str, create: bool) -> Result<Self> {
        let file = open_relative(self.directory(), OsStr::new(name), true, create)?;
        owned_directory(&file, true)?;
        let mut handles = self
            .handles
            .iter()
            .map(File::try_clone)
            .collect::<std::io::Result<Vec<_>>>()?;
        handles.push(file);
        Ok(Self { handles })
    }

    pub fn file(&self, name: &str, create: bool) -> Result<File> {
        open_relative(self.directory(), OsStr::new(name), false, create)
    }

    pub fn file_for_sync(&self, name: &str) -> Result<File> {
        self.file(name, false)
    }

    pub fn lock_file(&self, name: &str) -> Result<File> {
        match self.file(name, true) {
            Ok(file) => Ok(file),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists) =>
            {
                self.file(name, false)
            }
            Err(error) => Err(error),
        }
    }

    pub fn recovery_file(&self, value: &str) -> Result<File> {
        let value = name(OsStr::new(value))?;
        let descriptor = unsafe {
            libc::openat(
                self.directory().as_raw_fd(),
                value.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if descriptor < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let file = unsafe { File::from_raw_fd(descriptor) };
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file()
                && (1..=2).contains(&metadata.nlink())
                && metadata.uid() == unsafe { libc::geteuid() },
            "Output recovery refuses unknown aliases"
        );
        Ok(file)
    }

    pub fn sync(&self) -> Result<()> {
        self.directory().sync_all()?;
        Ok(())
    }

    pub fn same(&self, other: &Self) -> Result<bool> {
        let a = self.directory().metadata()?;
        let b = other.directory().metadata()?;
        Ok(a.dev() == b.dev() && a.ino() == b.ino())
    }

    pub fn rename(
        &self,
        file: &File,
        from: &str,
        target: &Self,
        to: &str,
        replace: bool,
    ) -> Result<()> {
        regular(file)?;
        let current = self.file(from, false)?;
        ensure!(
            same_file(file, &current)?,
            "Output staging identity changed"
        );
        let from = name(OsStr::new(from))?;
        let to = name(OsStr::new(to))?;
        if replace {
            let result = unsafe {
                libc::renameat(
                    self.directory().as_raw_fd(),
                    from.as_ptr(),
                    target.directory().as_raw_fd(),
                    to.as_ptr(),
                )
            };
            ensure!(
                result == 0,
                "Output metadata publication failed: {}",
                std::io::Error::last_os_error()
            );
        } else {
            // link+unlink is the portable no-clobber publication primitive. A
            // crash between the two is reconciled by matching inode authority.
            let result = unsafe {
                libc::linkat(
                    self.directory().as_raw_fd(),
                    from.as_ptr(),
                    target.directory().as_raw_fd(),
                    to.as_ptr(),
                    0,
                )
            };
            if result != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            // Check the linked identity before deleting any staging name.
            // App-owned directories and the operation lease exclude competing
            // app mutations; Unix has no general unlink-by-open-handle syscall
            // against a malicious process with this account's write authority.
            let published = target.recovery_file(to.to_str()?)?;
            ensure!(
                same_file(file, &published)?,
                "Output publication identity changed"
            );
            regular_pair(file)?;
            regular_pair(&published)?;
            let result = unsafe { libc::unlinkat(self.directory().as_raw_fd(), from.as_ptr(), 0) };
            ensure!(
                result == 0,
                "Output staging cleanup is incomplete: {}",
                std::io::Error::last_os_error()
            );
        }
        regular(file)?;
        Ok(())
    }

    pub fn unlink(&self, value: &str) -> Result<()> {
        let value = name(OsStr::new(value))?;
        let result = unsafe { libc::unlinkat(self.directory().as_raw_fd(), value.as_ptr(), 0) };
        if result != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
}

pub(super) fn regular(file: &File) -> Result<()> {
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.nlink() == 1 && metadata.uid() == unsafe { libc::geteuid() },
        "Output storage requires an owned regular file without hard links"
    );
    Ok(())
}

pub(super) fn same_file(a: &File, b: &File) -> Result<bool> {
    let a = a.metadata()?;
    let b = b.metadata()?;
    Ok(a.dev() == b.dev() && a.ino() == b.ino())
}

pub(super) fn regular_pair(file: &File) -> Result<()> {
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.nlink() == 2 && metadata.uid() == unsafe { libc::geteuid() },
        "Interrupted publication requires exactly its two owned aliases"
    );
    Ok(())
}
