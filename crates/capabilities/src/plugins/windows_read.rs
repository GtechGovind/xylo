//! Windows file authority is an owned directory handle, not a checked path.
//! Each lookup is one no-follow component relative to that authority. Handles
//! deny write/delete sharing while validation and the final bounded read run.

use super::checked;
use crate::windows_nt::{self, Identity, identity, validate_component};
use anyhow::{Context, Result, ensure};
use std::{
    ffi::{OsStr, OsString},
    fs::File,
    mem::size_of,
    os::windows::{ffi::OsStrExt, io::AsRawHandle},
    path::{Component, Path, Prefix},
};
use tokio_util::sync::CancellationToken;
use windows_sys::{
    Wdk::{
        Storage::FileSystem::{
            FILE_COMPLETE_IF_OPLOCKED, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE,
            FILE_OPEN_NO_RECALL, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT,
            FileFsDeviceInformation, NtQueryVolumeInformationFile,
        },
        System::SystemServices::{FILE_FS_DEVICE_INFORMATION, FILE_REMOTE_DEVICE},
    },
    Win32::{
        Storage::FileSystem::{
            FILE_ATTRIBUTE_DEVICE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_OFFLINE,
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_DEVICE_DISK, FILE_NAME_NORMALIZED,
            FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_READ, FILE_TRAVERSE,
            GetFinalPathNameByHandleW, SYNCHRONIZE, VOLUME_NAME_GUID,
        },
        System::IO::IO_STATUS_BLOCK,
    },
};

#[derive(Clone, Debug)]
struct DrivePath {
    drive: u8,
    parts: Vec<OsString>,
}

struct DirectoryAuthority {
    // Retain ancestors too: checked directories cannot be renamed before the
    // final open, and every subsequent lookup stays anchored to these objects.
    handles: Vec<File>,
}

impl DirectoryAuthority {
    fn directory(&self) -> &File {
        self.handles
            .last()
            .expect("An authority always owns its root")
    }
}

pub(crate) fn open_workspace_read(
    config: &crate::CapabilityConfig,
    raw: &str,
    cancel: &CancellationToken,
) -> Result<File> {
    open_with_before(config, raw, cancel, || Ok(()))
}

fn open_with_before(
    config: &crate::CapabilityConfig,
    raw: &str,
    cancel: &CancellationToken,
    before_final_open: impl FnOnce() -> Result<()>,
) -> Result<File> {
    ensure!(
        raw.len() <= 4096,
        "Workspace file path exceeds its byte budget"
    );
    checked(cancel)?;
    let workspace = drive_path(&config.workspace)?;
    let parts = requested_parts(raw, &workspace)?;
    let private_root = config
        .management_root
        .as_ref()
        .and_then(|root| root.parent())
        .context("Safe plugin reads require the application data directory authority")?;
    let private = open_authority(&drive_path(private_root)?, cancel, None)?;
    let forbidden = identity(private.directory())?;
    let mut directory = open_authority(&workspace, cancel, Some(forbidden))?;
    for part in &parts[..parts.len() - 1] {
        checked(cancel)?;
        let next = open_relative(directory.directory(), part, true)?;
        reject_private(&next, forbidden)?;
        directory.handles.push(next);
    }
    before_final_open()?;
    checked(cancel)?;
    let file = open_relative(directory.directory(), parts.last().unwrap(), false)?;
    checked(cancel)?;
    Ok(file)
}

fn drive_path(path: &Path) -> Result<DrivePath> {
    ensure!(
        path.as_os_str().encode_wide().count() <= 32767,
        "Directory authority path exceeds its budget"
    );
    let mut components = path.components();
    let drive = match components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive.to_ascii_uppercase(),
            _ => anyhow::bail!("Plugin reads require a local disk workspace"),
        },
        _ => anyhow::bail!("Workspace directory authority must be an absolute disk path"),
    };
    ensure!(
        drive.is_ascii_alphabetic() && components.next() == Some(Component::RootDir),
        "Drive-relative directory authorities are not supported"
    );
    let parts = collect_parts(components)?;
    Ok(DrivePath { drive, parts })
}

fn requested_parts(raw: &str, workspace: &DrivePath) -> Result<Vec<OsString>> {
    // Do not let Path::components normalize away explicit traversal/aliases.
    ensure!(
        !raw.is_empty()
            && !raw
                .split(['\\', '/'])
                .any(|part| part.is_empty() || matches!(part, "." | "..")),
        "Workspace file paths cannot contain empty components or traversal"
    );
    let path = Path::new(raw);
    let mut components = path.components();
    let parts = if matches!(components.clone().next(), Some(Component::Prefix(_))) {
        let Some(Component::Prefix(prefix)) = components.next() else {
            unreachable!()
        };
        ensure!(
            matches!(prefix.kind(), Prefix::Disk(drive) if drive.to_ascii_uppercase() == workspace.drive),
            "Device, network and unrelated disk paths are outside workspace access"
        );
        ensure!(
            components.next() == Some(Component::RootDir),
            "Drive-relative file paths are not supported"
        );
        let all = collect_parts(components)?;
        // Exact component matching is deliberately conservative on directories
        // whose Windows case-sensitivity setting may differ from the default.
        ensure!(
            all.starts_with(&workspace.parts),
            "Plugin reads are restricted to the selected workspace"
        );
        all[workspace.parts.len()..].to_vec()
    } else {
        collect_parts(components)?
    };
    ensure!(
        !parts.is_empty() && parts.len() <= 256,
        "Workspace file path has an unsupported depth"
    );
    Ok(parts)
}

fn collect_parts(components: std::path::Components<'_>) -> Result<Vec<OsString>> {
    let mut parts = Vec::new();
    for component in components {
        let Component::Normal(name) = component else {
            anyhow::bail!("Workspace file paths cannot traverse outside their directory authority");
        };
        validate_component(name)?;
        parts.push(name.to_owned());
        ensure!(
            parts.len() <= 256,
            "Workspace file path has an unsupported depth"
        );
    }
    Ok(parts)
}

fn open_authority(
    path: &DrivePath,
    cancel: &CancellationToken,
    forbidden: Option<Identity>,
) -> Result<DirectoryAuthority> {
    checked(cancel)?;
    // The only absolute NT name constructed by this service is a single drive
    // root from a validated ASCII drive letter. The held result is certified as
    // an actual local volume root, refusing network and SUBST directory aliases.
    let root_name = OsString::from(format!("\\??\\{}:\\", char::from(path.drive)));
    let root = native_open(None, &root_name, true)?;
    qualify_volume_root(&root)?;
    if let Some(forbidden) = forbidden {
        reject_private(&root, forbidden)?;
    }
    let mut authority = DirectoryAuthority {
        handles: vec![root],
    };
    for part in &path.parts {
        checked(cancel)?;
        let next = open_relative(authority.directory(), part, true)?;
        if let Some(forbidden) = forbidden {
            reject_private(&next, forbidden)?;
        }
        authority.handles.push(next);
    }
    Ok(authority)
}

fn open_relative(parent: &File, name: &OsStr, directory: bool) -> Result<File> {
    validate_component(name)?;
    native_open(Some(parent), name, directory)
}

fn native_open(parent: Option<&File>, name: &OsStr, directory: bool) -> Result<File> {
    let file = windows_nt::open(
        parent,
        name,
        FILE_READ_ATTRIBUTES
            | SYNCHRONIZE
            | if directory {
                FILE_TRAVERSE
            } else {
                FILE_READ_DATA
            },
        FILE_SHARE_READ,
        FILE_OPEN_REPARSE_POINT
            | FILE_SYNCHRONOUS_IO_NONALERT
            | FILE_COMPLETE_IF_OPLOCKED
            | if directory {
                FILE_DIRECTORY_FILE
            } else {
                // NT rejects NO_RECALL with DIRECTORY_FILE. Keep it for
                // regular files; directory handles still reject offline
                // objects and reparse points during attribute validation.
                FILE_NON_DIRECTORY_FILE | FILE_OPEN_NO_RECALL
            },
    )
    .map_err(|refused| anyhow::anyhow!("Workspace file could not be opened safely ({refused})"))?;
    validate_handle(&file, directory)?;
    Ok(file)
}

fn validate_handle(file: &File, directory: bool) -> Result<()> {
    let info = windows_nt::information(file)?;
    ensure!(
        info.dwFileAttributes
            & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DEVICE | FILE_ATTRIBUTE_OFFLINE)
            == 0,
        "Plugin reads cannot follow links, devices or offline placeholders"
    );
    ensure!(
        (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) == directory,
        "Workspace file type does not match its authority"
    );
    if !directory {
        ensure!(
            info.nNumberOfLinks == 1,
            "Plugin reads require a regular workspace file without links"
        );
    }
    Ok(())
}

fn reject_private(directory: &File, forbidden: Identity) -> Result<()> {
    ensure!(
        identity(directory)? != forbidden,
        "Application data files are outside plugin read access"
    );
    Ok(())
}

fn qualify_volume_root(root: &File) -> Result<()> {
    let mut io = IO_STATUS_BLOCK::default();
    let mut device = FILE_FS_DEVICE_INFORMATION::default();
    // SAFETY: the fixed buffer and size match FileFsDeviceInformation.
    let status = unsafe {
        NtQueryVolumeInformationFile(
            root.as_raw_handle(),
            &mut io,
            std::ptr::from_mut(&mut device).cast(),
            size_of::<FILE_FS_DEVICE_INFORMATION>() as u32,
            FileFsDeviceInformation,
        )
    };
    ensure!(
        status == 0
            && device.DeviceType == FILE_DEVICE_DISK
            && device.Characteristics & FILE_REMOTE_DEVICE == 0,
        "Plugin reads require a verified local disk volume"
    );
    let mut name = [0_u16; 512];
    // SAFETY: root is held throughout this bounded, read-only name query. The
    // result qualifies this same root; it is never used to reopen anything.
    let length = unsafe {
        GetFinalPathNameByHandleW(
            root.as_raw_handle(),
            name.as_mut_ptr(),
            name.len() as u32,
            VOLUME_NAME_GUID | FILE_NAME_NORMALIZED,
        )
    } as usize;
    ensure!(
        length > 0 && length < name.len(),
        "Windows could not verify the workspace volume root"
    );
    let name = String::from_utf16(&name[..length])?;
    ensure!(
        is_volume_root_name(&name),
        "Mapped directories cannot serve as plugin volume roots"
    );
    Ok(())
}

fn is_volume_root_name(name: &str) -> bool {
    name.strip_prefix("\\\\?\\Volume{")
        .and_then(|name| name.strip_suffix("}\\"))
        .is_some_and(|guid| {
            guid.len() == 36
                && guid.bytes().enumerate().all(|(index, byte)| {
                    if [8, 13, 18, 23].contains(&index) {
                        byte == b'-'
                    } else {
                        byte.is_ascii_hexdigit()
                    }
                })
        })
}

#[cfg(test)]
#[path = "windows_read/tests.rs"]
mod tests;
