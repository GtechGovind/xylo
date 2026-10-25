//! Bind hashing to an opened regular-file descriptor. Unix paths are traversed
//! from an anchored root with no-follow directory handles; a swapped symlink is
//! never followed between scope validation and the read.
use anyhow::{Result, ensure};
use std::{fs::File, path::Path};
#[cfg(unix)]
pub(super) fn open(path: &Path) -> Result<File> {
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        },
        path::Component,
    };
    let parts = path
        .components()
        .filter_map(|part| {
            if let Component::Normal(name) = part {
                Some(name)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    ensure!(
        path.is_absolute() && !parts.is_empty(),
        "Choose an absolute original file location"
    );
    let mut parent = File::open("/")?;
    for (index, part) in parts.iter().enumerate() {
        let name = CString::new(part.as_bytes())?;
        let last = index + 1 == parts.len();
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if last { 0 } else { libc::O_DIRECTORY };
        // SAFETY: parent is an owned open descriptor and name is NUL terminated.
        let descriptor = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        ensure!(
            descriptor >= 0,
            "The original location changed or cannot be read: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: a successful openat returns one newly owned descriptor.
        parent = unsafe { File::from_raw_fd(descriptor) };
    }
    ensure!(
        parent.metadata()?.is_file(),
        "The original must be a regular file"
    );
    Ok(parent)
}
#[cfg(windows)]
pub(super) fn open(path: &Path) -> Result<File> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(0x0020_0000)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.file_attributes() & 0x400 == 0,
        "Choose a regular original file, not a link"
    );
    Ok(file)
}
#[cfg(not(any(unix, windows)))]
pub(super) fn open(_path: &Path) -> Result<File> {
    anyhow::bail!("Original-file verification is unavailable on this platform")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn swapped_symlink_ancestor_is_not_followed() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let project = root.join("project");
        let private = root.join("private");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&private).unwrap();
        std::fs::write(private.join("original.txt"), "private").unwrap();
        let path = project.join("original.txt");
        std::fs::remove_dir(&project).unwrap();
        std::os::unix::fs::symlink(&private, &project).unwrap();
        assert!(open(&path).is_err());
    }
    #[test]
    fn opened_descriptor_keeps_its_bytes_when_the_path_is_replaced() {
        use std::io::Read;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("original.txt");
        std::fs::write(&path, "original").unwrap();
        let mut file = open(&path).unwrap();
        std::fs::rename(&path, root.join("moved.txt")).unwrap();
        std::fs::write(&path, "replacement").unwrap();
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        assert_eq!(text, "original");
    }
}
