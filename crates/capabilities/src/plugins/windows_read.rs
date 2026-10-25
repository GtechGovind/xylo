//! Windows file authority is an owned directory handle, not a checked path.
//! Each lookup is one no-follow component relative to that authority. Handles
//! deny write/delete sharing while validation and the final bounded read run.

use super::checked;
use anyhow::{Context, Result, ensure};
use std::{
    ffi::{OsStr, OsString},
    fs::File,
    mem::size_of,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle},
    },
    path::{Component, Path, Prefix},
};
use tokio_util::sync::CancellationToken;
use windows_sys::{
    Wdk::{
        Foundation::OBJECT_ATTRIBUTES,
        Storage::FileSystem::{
            FILE_COMPLETE_IF_OPLOCKED, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
            FILE_OPEN_NO_RECALL, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT,
            FileFsDeviceInformation, NtCreateFile, NtQueryVolumeInformationFile,
        },
        System::SystemServices::{FILE_FS_DEVICE_INFORMATION, FILE_REMOTE_DEVICE},
    },
    Win32::{
        Foundation::{
            HANDLE, INVALID_HANDLE_VALUE, OBJ_CASE_INSENSITIVE, OBJ_DONT_REPARSE, UNICODE_STRING,
        },
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DEVICE, FILE_ATTRIBUTE_DIRECTORY,
            FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DEVICE_DISK, FILE_ID_INFO,
            FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_READ,
            FILE_TRAVERSE, FILE_TYPE_DISK, FileIdInfo, GetFileInformationByHandle,
            GetFileInformationByHandleEx, GetFileType, GetFinalPathNameByHandleW, SYNCHRONIZE,
            VOLUME_NAME_GUID,
        },
        System::IO::IO_STATUS_BLOCK,
    },
};

#[derive(Clone, Debug)]
struct DrivePath {
    drive: u8,
    parts: Vec<OsString>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity {
    volume: u64,
    file: [u8; 16],
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

fn validate_component(name: &OsStr) -> Result<()> {
    let units: Vec<_> = name.encode_wide().collect();
    ensure!(
        !units.is_empty()
            && units.len() <= 255
            && !units
                .iter()
                .any(|unit| *unit < 32
                    || matches!(*unit, 34 | 42 | 47 | 58 | 60 | 62 | 63 | 92 | 124))
            && !matches!(units.last(), Some(32 | 46)),
        "Workspace names cannot contain streams, devices, separators or trailing spaces/dots"
    );
    let stem: String = units
        .iter()
        .take_while(|unit| **unit != 46)
        .map(|unit| char::from_u32(u32::from(*unit)).unwrap_or('\u{fffd}'))
        .collect::<String>()
        .to_ascii_lowercase();
    let numbered_device = stem
        .strip_prefix("com")
        .or_else(|| stem.strip_prefix("lpt"))
        .is_some_and(|number| {
            matches!(
                number,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        });
    ensure!(
        !matches!(
            stem.as_str(),
            "con" | "prn" | "aux" | "nul" | "conin$" | "conout$" | "clock$"
        ) && !numbered_device,
        "Workspace device names are not supported"
    );
    Ok(())
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
    let mut units: Vec<_> = name.encode_wide().collect();
    let length = u16::try_from(units.len() * size_of::<u16>())?;
    let unicode = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: units.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.map_or(std::ptr::null_mut(), |file| file.as_raw_handle()),
        ObjectName: &unicode,
        Attributes: OBJ_CASE_INSENSITIVE
            | if parent.is_some() {
                OBJ_DONT_REPARSE
            } else {
                0
            },
        ..Default::default()
    };
    let mut io = IO_STATUS_BLOCK::default();
    let mut handle: HANDLE = std::ptr::null_mut();
    // SAFETY: all NT structures and the counted UTF-16 name remain live for the
    // synchronous call; the parent handle is borrowed, output ownership moves
    // immediately into File. No create/overwrite disposition is permitted.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            FILE_READ_ATTRIBUTES
                | SYNCHRONIZE
                | if directory {
                    FILE_TRAVERSE
                } else {
                    FILE_READ_DATA
                },
            &attributes,
            &mut io,
            std::ptr::null(),
            0,
            FILE_SHARE_READ,
            FILE_OPEN,
            FILE_OPEN_REPARSE_POINT
                | FILE_OPEN_NO_RECALL
                | FILE_SYNCHRONOUS_IO_NONALERT
                | FILE_COMPLETE_IF_OPLOCKED
                | if directory {
                    FILE_DIRECTORY_FILE
                } else {
                    FILE_NON_DIRECTORY_FILE
                },
            std::ptr::null(),
            0,
        )
    };
    // Positive informational NT results (including oplock break in progress)
    // are refused too. If an owned handle was returned, close it on refusal.
    let file = if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
        // SAFETY: this is a new owned handle returned by NtCreateFile.
        Some(unsafe { File::from_raw_handle(handle) })
    } else {
        None
    };
    ensure!(
        status == 0,
        "Workspace file could not be opened safely (Windows status 0x{:08x})",
        status as u32
    );
    let file = file.context("Windows did not return a file authority")?;
    validate_handle(&file, directory)?;
    Ok(file)
}

fn validate_handle(file: &File, directory: bool) -> Result<()> {
    // SAFETY: the File owns a live handle for these read-only queries.
    ensure!(
        unsafe { GetFileType(file.as_raw_handle()) } == FILE_TYPE_DISK,
        "Plugin reads require a local disk file"
    );
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the output buffer has the API's exact structure size.
    ensure!(
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } != 0,
        "Windows could not verify file attributes: {}",
        std::io::Error::last_os_error()
    );
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

fn identity(file: &File) -> Result<Identity> {
    let mut info = FILE_ID_INFO::default();
    // SAFETY: the output type/size match FileIdInfo and the handle is live.
    ensure!(
        unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileIdInfo,
                std::ptr::from_mut(&mut info).cast(),
                size_of::<FILE_ID_INFO>() as u32,
            )
        } != 0,
        "Windows could not verify directory identity: {}",
        std::io::Error::last_os_error()
    );
    ensure!(
        info.FileId.Identifier != [0; 16],
        "Filesystem does not provide a reliable directory identity"
    );
    Ok(Identity {
        volume: info.VolumeSerialNumber,
        file: info.FileId.Identifier,
    })
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
#[path = "windows_read_tests.rs"]
mod tests;
