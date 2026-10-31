//! Opening tool inputs without a check-then-use race.
//!
//! A tool names an input by path, but a path checked and then reopened can
//! be swapped for a symlink in between. [`open_input`] instead opens the
//! input once: it starts from an open handle on the workspace or artifacts
//! root and opens each component relative to its parent with `O_NOFOLLOW`,
//! so the kernel never follows a symlink. A link met on the way is read and
//! its target walked the same way, only while it stays beneath the root (at
//! most 16 links). The walk refuses Xylo's management directory by identity
//! and hands consumers that same handle. The file must be a
//! regular file with a single link, so a hard link cannot make a file from
//! elsewhere look like a project file. Other platforms fall back to a
//! resolve-check-open sequence whose remaining race is documented on it.

use crate::CapabilityConfig;
use anyhow::{Result, bail, ensure};
use std::{
    fs::File,
    path::{Component, Path, PathBuf},
};

/// Longest accepted input path, in bytes.
const MAX_PATH_BYTES: usize = 4096;
/// Deepest accepted input path, in components.
const MAX_DEPTH: usize = 256;

/// Which root an input was opened from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InputRoot {
    /// The selected workspace.
    Workspace,
    /// Xylo's capability artifacts (recordings, exports, generated media).
    Artifacts,
}

/// An input file opened once, with the path it was opened at.
#[derive(Debug)]
pub(crate) struct InputFile {
    /// The opened file; read this, never reopen [`Self::path`].
    pub(crate) file: File,
    /// Where the opened file lies: the canonical root joined with the path
    /// reached after following any links inside it. Used for display,
    /// extensions, draft keys and revision checks.
    pub(crate) path: PathBuf,
    /// The root the input lies under.
    pub(crate) root: InputRoot,
}

/// Opens `raw` (relative to the workspace, or absolute under the workspace
/// or the artifacts root) as described in the module documentation.
pub(crate) fn open_input(config: &CapabilityConfig, raw: &str) -> Result<InputFile> {
    ensure!(
        !raw.is_empty() && raw.len() <= MAX_PATH_BYTES,
        "Input path must contain 1–{MAX_PATH_BYTES} bytes"
    );
    let requested = Path::new(raw);
    let (root, base, relative) = if requested.is_absolute() {
        [
            // Artifacts first: it may lie inside the workspace.
            (InputRoot::Artifacts, &config.artifacts),
            (InputRoot::Workspace, &config.workspace),
        ]
        .into_iter()
        .find_map(|(root, base)| Some((root, base, beneath(requested, base)?)))
        .ok_or_else(|| anyhow::anyhow!("Input is outside the workspace and artifacts roots"))?
    } else {
        (
            InputRoot::Workspace,
            &config.workspace,
            requested.to_path_buf(),
        )
    };
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(name) => parts.push(name.to_owned()),
            Component::CurDir => {}
            _ => bail!("Input paths cannot leave their root"),
        }
    }
    ensure!(
        !parts.is_empty() && parts.len() <= MAX_DEPTH,
        "Input path has an unsupported depth"
    );
    // The roots come from Xylo's own configuration, not from the model, so
    // resolving them once is safe; everything below them is walked by handle.
    let base = std::fs::canonicalize(base)?;
    let management = config
        .management_root
        .as_ref()
        .and_then(|management| std::fs::canonicalize(management).ok());
    let (file, path) = platform::open_beneath(&base, &parts, management.as_deref())?;
    Ok(InputFile { file, path, root })
}

/// `requested` relative to `base`. The prefix may be spelled through links
/// above the root (macOS `/var` is `/private/var`), so the shallowest
/// ancestor of `requested` that resolves to `base` marks where the root
/// ends; everything after it is walked without following links.
fn beneath(requested: &Path, base: &Path) -> Option<PathBuf> {
    if let Ok(relative) = requested.strip_prefix(base) {
        return Some(relative.to_path_buf());
    }
    let resolved = std::fs::canonicalize(base).ok()?;
    let ancestors: Vec<&Path> = requested.ancestors().collect();
    ancestors.into_iter().rev().find_map(|ancestor| {
        (std::fs::canonicalize(ancestor).ok()? == resolved)
            .then(|| requested.strip_prefix(ancestor).ok().map(Path::to_path_buf))
            .flatten()
    })
}

#[cfg(unix)]
mod platform {
    use anyhow::{Result, anyhow, bail, ensure};
    use std::{
        collections::VecDeque,
        ffi::{CString, OsStr, OsString},
        fs::File,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::{
                ffi::{OsStrExt, OsStringExt},
                fs::MetadataExt,
                fs::OpenOptionsExt,
            },
        },
        path::{Component, Path, PathBuf},
    };

    /// Most links followed while opening one input.
    const MAX_LINKS: usize = 16;

    /// Opens `parts` beneath the canonical directory `base`, one no-follow
    /// component at a time, refusing to pass through `management`. A symlink
    /// met on the way is read (never followed by the kernel) and its target
    /// is walked the same way, as long as it stays beneath `base`.
    pub(super) fn open_beneath(
        base: &Path,
        parts: &[OsString],
        management: Option<&Path>,
    ) -> Result<(File, PathBuf)> {
        let forbidden = management
            .and_then(|path| std::fs::metadata(path).ok())
            .map(|metadata| (metadata.dev(), metadata.ino()));
        let root = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(base)?;
        reject_private(&root, forbidden)?;
        // Opened directories from the root down; `..` pops, never past root.
        let mut stack = vec![root];
        // The names of `stack[1..]`, so the resolved path can be reported.
        let mut names: Vec<OsString> = Vec::new();
        let mut pending: VecDeque<OsString> = parts.iter().cloned().collect();
        let mut links = 0;
        while let Some(part) = pending.pop_front() {
            if part.as_bytes() == b".." {
                ensure!(stack.len() > 1, "Input links cannot leave their root");
                stack.pop();
                names.pop();
                continue;
            }
            if part.is_empty() || part.as_bytes() == b"." {
                continue;
            }
            let last = pending.is_empty();
            let Some(parent) = stack.last() else {
                bail!("Input path left its root");
            };
            match open_relative(parent, &part, !last)? {
                Opened::File(file) if last => {
                    let path = names
                        .iter()
                        .chain(std::iter::once(&part))
                        .fold(base.to_path_buf(), |path, name| path.join(name));
                    return Ok((file, path));
                }
                Opened::File(directory) => {
                    reject_private(&directory, forbidden)?;
                    stack.push(directory);
                    names.push(part);
                }
                Opened::Link => {
                    links += 1;
                    ensure!(links <= MAX_LINKS, "Input follows too many links");
                    let target = read_link(parent, &part)?;
                    let relative = if target.is_absolute() {
                        stack.truncate(1);
                        names.clear();
                        target
                            .strip_prefix(base)
                            .map_err(|_| anyhow!("Input links lead outside its root"))?
                            .to_path_buf()
                    } else {
                        target
                    };
                    let mut next = VecDeque::new();
                    for component in relative.components() {
                        match component {
                            Component::Normal(name) => next.push_back(name.to_owned()),
                            Component::ParentDir => next.push_back(OsString::from("..")),
                            Component::CurDir => {}
                            Component::RootDir | Component::Prefix(_) => {
                                bail!("Input links lead outside its root")
                            }
                        }
                    }
                    next.extend(pending.drain(..));
                    pending = next;
                }
            }
        }
        bail!("Input path names no file")
    }

    /// What one no-follow open found.
    enum Opened {
        File(File),
        /// The component is a symlink; read it with [`read_link`].
        Link,
    }

    fn read_link(parent: &File, name: &OsStr) -> Result<PathBuf> {
        let name = CString::new(name.as_bytes())?;
        let mut buffer = vec![0u8; libc::PATH_MAX as usize];
        // SAFETY: parent is a live directory handle, name is NUL-terminated
        // and buffer is writable for its full length.
        let length = unsafe {
            libc::readlinkat(
                parent.as_raw_fd(),
                name.as_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            )
        };
        ensure!(
            length >= 0,
            "Input link could not be read: {}",
            std::io::Error::last_os_error()
        );
        buffer.truncate(length as usize);
        ensure!(!buffer.is_empty(), "Input link is empty");
        Ok(PathBuf::from(OsString::from_vec(buffer)))
    }

    /// Whether `name` in `parent` is a symlink, without following it.
    fn is_link(parent: &File, name: &CString) -> bool {
        // SAFETY: an all-zero stat is a valid output buffer for fstatat.
        let mut status: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: parent is a live directory handle and name is NUL-terminated.
        let result = unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                name.as_ptr(),
                &mut status,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        result == 0 && (status.st_mode & libc::S_IFMT) == libc::S_IFLNK
    }

    fn reject_private(directory: &File, forbidden: Option<(u64, u64)>) -> Result<()> {
        let metadata = directory.metadata()?;
        ensure!(
            forbidden != Some((metadata.dev(), metadata.ino())),
            "Application management files are outside the tool filesystem scope"
        );
        Ok(())
    }

    fn open_relative(parent: &File, name: &OsStr, directory: bool) -> Result<Opened> {
        let name = CString::new(name.as_bytes())?;
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | libc::O_CLOEXEC
            | if directory { libc::O_DIRECTORY } else { 0 };
        // SAFETY: parent is a live directory handle and name is a
        // NUL-terminated single component; a returned descriptor is owned
        // exactly once below.
        let descriptor = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if descriptor < 0 {
            let error = std::io::Error::last_os_error();
            if is_link(parent, &name) {
                return Ok(Opened::Link);
            }
            bail!("Input could not be opened: {error}");
        }
        // SAFETY: openat returned a new descriptor that nothing else owns.
        let file = unsafe { File::from_raw_fd(descriptor) };
        let metadata = file.metadata()?;
        if directory {
            ensure!(
                metadata.is_dir(),
                "Input path component must be a directory"
            );
        } else {
            ensure!(
                metadata.is_file() && metadata.nlink() == 1,
                "Input must be a regular file without other links"
            );
            // O_NONBLOCK only kept a FIFO from blocking the open; reads of
            // the regular file stay blocking.
            // SAFETY: fcntl on a descriptor this function owns.
            let status = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
            ensure!(status >= 0, "Input status could not be read");
            // SAFETY: as above; only the O_NONBLOCK bit changes.
            let cleared =
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, status & !libc::O_NONBLOCK) };
            ensure!(cleared >= 0, "Input status could not be changed");
        }
        Ok(Opened::File(file))
    }
}

/// Fallback for platforms without the Unix relative no-follow walk: resolve
/// the path, check its scope, then open it.
///
/// This keeps a check-then-open race: between `canonicalize` and the open, a
/// process that can write inside the workspace could swap a component for a
/// link to a file outside the roots. Closing it needs handle-relative opens
/// like `plugins/windows_read.rs`, adapted to two roots and tested on
/// Windows.
#[cfg(not(unix))]
mod platform {
    use anyhow::{Result, ensure};
    use std::{
        ffi::OsString,
        fs::File,
        path::{Path, PathBuf},
    };

    pub(super) fn open_beneath(
        base: &Path,
        parts: &[OsString],
        management: Option<&Path>,
    ) -> Result<(File, PathBuf)> {
        let joined = parts
            .iter()
            .fold(base.to_path_buf(), |path, part| path.join(part));
        let canonical = std::fs::canonicalize(joined)?;
        ensure!(
            canonical.starts_with(base),
            "Input is outside the workspace and artifacts roots"
        );
        ensure!(
            !management.is_some_and(|management| canonical.starts_with(management)),
            "Application management files are outside the tool filesystem scope"
        );
        ensure!(canonical.is_file(), "Input must be a regular file");
        let file = File::open(&canonical)?;
        Ok((file, canonical))
    }
}

#[cfg(test)]
mod tests;
