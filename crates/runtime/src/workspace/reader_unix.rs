use anyhow::{Result, ensure};
use std::{
    ffi::{CString, OsStr},
    fs::File,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    },
    path::{Component, Path},
};
use tokio_util::sync::CancellationToken;

pub(super) fn open_root(
    root: &Path,
    cancel: &CancellationToken,
    project: &CancellationToken,
) -> Result<File> {
    ensure!(root.is_absolute(), "Project folder must be absolute");
    let mut parent = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open("/")?;
    for part in root.components() {
        super::super::index::check(cancel, project)?;
        match part {
            Component::RootDir => {}
            Component::Normal(name) => parent = open_relative(&parent, name, true)?,
            _ => anyhow::bail!("Project folder contains an unsupported path component"),
        }
    }
    Ok(parent)
}
pub(super) fn open_relative(parent: &File, name: &OsStr, directory: bool) -> Result<File> {
    let name = CString::new(name.as_bytes())?;
    let flags = libc::O_RDONLY
        | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | libc::O_CLOEXEC
        | if directory { libc::O_DIRECTORY } else { 0 };
    // SAFETY: parent is a live borrowed directory descriptor and name is one
    // NUL-terminated component. The successful descriptor is owned exactly once.
    let descriptor = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    ensure!(
        descriptor >= 0,
        "Project file changed or cannot be read: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: a successful openat returns a new descriptor transferred to File.
    let file = unsafe { File::from_raw_fd(descriptor) };
    let metadata = file.metadata()?;
    ensure!(
        if directory {
            metadata.is_dir()
        } else {
            metadata.is_file()
        },
        "Choose a regular project file"
    );
    Ok(file)
}
