//! The Windows walk behind [`super::open_input`], with the Unix guarantees.
//!
//! Each component is opened by `NtCreateFile` relative to its parent's
//! handle with `FILE_OPEN_REPARSE_POINT`, so the kernel never follows a
//! symlink or junction. A link (a name-surrogate reparse point) met on the
//! way is read from the handle that opened it (`FSCTL_GET_REPARSE_POINT`)
//! and its target walked the same way, only while it stays beneath the root
//! and for at most 16 links. Other reparse points, such as OneDrive
//! placeholders, are used through that same handle unless reading them
//! would download or recall data. Directories on the walk are held without delete sharing, so none
//! of them can be renamed out from under the walk. The management directory
//! is refused by volume serial number and file id, which no spelling (case,
//! 8.3 short name, link) changes. The input must be a regular local file
//! with one link.

use super::reparse::{self, Kind};
use crate::windows_nt::{self, Identity, Refused};
use anyhow::{Result, anyhow, bail, ensure};
use std::{
    collections::VecDeque,
    ffi::{OsStr, OsString},
    fs::File,
    os::windows::{ffi::OsStringExt, fs::OpenOptionsExt, io::AsRawHandle},
    path::{Component, Path, PathBuf},
};
use windows_sys::{
    Wdk::Storage::FileSystem::{
        FILE_COMPLETE_IF_OPLOCKED, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE,
        FILE_OPEN_NO_RECALL, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT,
    },
    Win32::{
        Foundation::{STATUS_FILE_IS_A_DIRECTORY, STATUS_NOT_A_DIRECTORY},
        Storage::FileSystem::{
            FILE_ATTRIBUTE_DIRECTORY, FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_READ,
            FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            FILE_TRAVERSE, SYNCHRONIZE,
        },
        System::{IO::DeviceIoControl, Ioctl::FSCTL_GET_REPARSE_POINT},
    },
};

/// Most links followed while opening one input.
const MAX_LINKS: usize = 16;
/// `MAXIMUM_REPARSE_DATA_BUFFER_SIZE`: the largest reparse buffer stored.
const MAX_REPARSE_BYTES: usize = 16 * 1024;
/// Sharing for held directories: others may read and add entries, but not
/// rename or delete a directory the walk stands in.
const DIRECTORY_SHARE: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE;
/// Sharing for the input and for links: as permissive as `File::open`, so
/// an editor's atomic save is not blocked while a tool reads.
const OPEN_SHARE: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
/// Options every component open uses.
const COMPONENT_OPTIONS: u32 =
    FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT | FILE_COMPLETE_IF_OPLOCKED;

/// Opens `parts` beneath the canonical directory `base`, one no-reparse
/// component at a time, refusing to pass through `management`. A symlink or
/// junction met on the way is read (never followed by the kernel) and its
/// target is walked the same way, as long as it stays beneath `base`.
pub(super) fn open_beneath(
    base: &Path,
    parts: &[OsString],
    management: Option<&Path>,
) -> Result<(File, PathBuf)> {
    let forbidden = management
        .map(|path| windows_nt::identity(&open_root(path, OPEN_SHARE)?))
        .transpose()?;
    let root = open_root(base, DIRECTORY_SHARE)?;
    reject_private(&root, forbidden)?;
    // Opened directories from the root down; `..` pops, never past root.
    let mut stack = vec![root];
    // The names of `stack[1..]`, so the resolved path can be reported.
    let mut names: Vec<OsString> = Vec::new();
    let mut pending: VecDeque<OsString> = parts.iter().cloned().collect();
    let mut links = 0;
    while let Some(part) = pending.pop_front() {
        if part == ".." {
            ensure!(stack.len() > 1, "Input links cannot leave their root");
            stack.pop();
            names.pop();
            continue;
        }
        if part.is_empty() || part == "." {
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
            Opened::Link(target) => {
                links += 1;
                ensure!(links <= MAX_LINKS, "Input follows too many links");
                let mut next = if target.is_absolute() {
                    stack.truncate(1);
                    names.clear();
                    beneath_root(&target, base)
                        .ok_or_else(|| anyhow!("Input links lead outside its root"))?
                } else {
                    link_parts(target.components())?
                };
                next.extend(pending.drain(..));
                pending = next;
            }
        }
    }
    bail!("Input path names no file")
}

/// What one no-reparse open found.
enum Opened {
    File(File),
    /// The component is a symlink or junction pointing here.
    Link(PathBuf),
}

/// Opens a configured root (or the management directory) by its canonical
/// path. Roots come from Xylo's configuration, as on Unix; only what lies
/// beneath them is walked by handle.
fn open_root(path: &Path, share: u32) -> Result<File> {
    let directory = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(share)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    ensure!(
        windows_nt::information(&directory)?.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0,
        "Input root must be a directory"
    );
    Ok(directory)
}

fn reject_private(directory: &File, forbidden: Option<Identity>) -> Result<()> {
    if let Some(forbidden) = forbidden {
        ensure!(
            windows_nt::identity(directory)? != forbidden,
            "Application management files are outside the tool filesystem scope"
        );
    }
    Ok(())
}

/// Opens `name` in `parent` as a directory or as the input file, without
/// following a reparse point. The tag decides what was opened: a name
/// surrogate (symlink, junction) is a link to read and walk; any other
/// reparse point (a OneDrive placeholder, a dedup file) keeps its data under
/// this name, so the same handle is used as an ordinary file or directory.
/// A link of the other kind (a file symlink where a directory is expected,
/// or a directory link as the last component) fails the typed open; it is
/// then reopened untyped, used only if it is a link, and read.
fn open_relative(parent: &File, name: &OsStr, directory: bool) -> Result<Opened> {
    windows_nt::validate_component(name)?;
    let (access, share, options) = if directory {
        (
            FILE_READ_ATTRIBUTES | SYNCHRONIZE | FILE_TRAVERSE,
            DIRECTORY_SHARE,
            FILE_DIRECTORY_FILE,
        )
    } else {
        (
            FILE_GENERIC_READ,
            OPEN_SHARE,
            // NT rejects NO_RECALL with DIRECTORY_FILE; directories that
            // would recall are refused by attribute below.
            FILE_NON_DIRECTORY_FILE | FILE_OPEN_NO_RECALL,
        )
    };
    let file = match windows_nt::open(
        Some(parent),
        name,
        access,
        share,
        options | COMPONENT_OPTIONS,
    ) {
        Ok(file) => file,
        Err(Refused(status))
            if status == STATUS_NOT_A_DIRECTORY || status == STATUS_FILE_IS_A_DIRECTORY =>
        {
            let link = windows_nt::open(
                Some(parent),
                name,
                FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                OPEN_SHARE,
                COMPONENT_OPTIONS,
            )
            .map_err(|refused| anyhow!("Input could not be opened ({refused})"))?;
            let (attributes, tag) = windows_nt::attribute_tag(&link)?;
            ensure!(
                reparse::classify(attributes, tag)? == Kind::Link,
                "{}",
                if directory {
                    "Input path component must be a directory"
                } else {
                    "Input must be a regular file without other links"
                }
            );
            return Ok(Opened::Link(read_link(&link)?));
        }
        Err(refused) => bail!("Input could not be opened ({refused})"),
    };
    let (attributes, tag) = windows_nt::attribute_tag(&file)?;
    if reparse::classify(attributes, tag)? == Kind::Link {
        return Ok(Opened::Link(read_link(&file)?));
    }
    let info = windows_nt::information(&file)?;
    if directory {
        ensure!(
            attributes & FILE_ATTRIBUTE_DIRECTORY != 0,
            "Input path component must be a directory"
        );
    } else {
        ensure!(
            attributes & FILE_ATTRIBUTE_DIRECTORY == 0 && info.nNumberOfLinks == 1,
            "Input must be a regular file without other links"
        );
    }
    Ok(Opened::File(file))
}

/// Reads the target of the reparse point `link` is open on. Reading from the
/// same handle means the link cannot change between being found and read.
fn read_link(link: &File) -> Result<PathBuf> {
    let mut buffer = vec![0_u8; MAX_REPARSE_BYTES];
    let mut returned = 0_u32;
    // SAFETY: link is a live handle, the output buffer is writable for its
    // full length and the call is synchronous (no OVERLAPPED).
    let read = unsafe {
        DeviceIoControl(
            link.as_raw_handle(),
            FSCTL_GET_REPARSE_POINT,
            std::ptr::null(),
            0,
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    ensure!(
        read != 0,
        "Input link could not be read: {}",
        std::io::Error::last_os_error()
    );
    buffer.truncate(returned as usize);
    let target = reparse::target(&buffer)?;
    Ok(PathBuf::from(OsString::from_wide(&target)))
}

/// The components of an absolute link target below `base`, or `None` when
/// it lies elsewhere. Names compare ASCII case-insensitively, as NTFS does
/// by default; whatever matches, the rest is walked from the root handle, so
/// a looser match can only pick another name beneath the root, never leave.
fn beneath_root(target: &Path, base: &Path) -> Option<VecDeque<OsString>> {
    let mut components = target.components();
    for expected in base.components() {
        let actual = components.next()?;
        if !actual
            .as_os_str()
            .eq_ignore_ascii_case(expected.as_os_str())
        {
            return None;
        }
    }
    link_parts(components).ok()
}

/// A relative link target as pending components; a root or drive in it is
/// refused, as it would leave the root.
fn link_parts(components: std::path::Components<'_>) -> Result<VecDeque<OsString>> {
    let mut parts = VecDeque::new();
    for component in components {
        match component {
            Component::Normal(name) => parts.push_back(name.to_owned()),
            Component::ParentDir => parts.push_back(OsString::from("..")),
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => {
                bail!("Input links lead outside its root")
            }
        }
    }
    Ok(parts)
}
