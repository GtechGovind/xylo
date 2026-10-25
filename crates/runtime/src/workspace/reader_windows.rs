//! Relative-handle NT opens: namespace checks never reopen a validated string.
//! Compile qualification is separate from actual Windows filesystem testing.
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
            NtCreateFile,
        },
    },
    Win32::{
        Foundation::{
            HANDLE, INVALID_HANDLE_VALUE, OBJ_CASE_INSENSITIVE, OBJ_DONT_REPARSE, UNICODE_STRING,
        },
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DEVICE, FILE_ATTRIBUTE_DIRECTORY,
            FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_READ_ATTRIBUTES,
            FILE_READ_DATA, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE,
            FILE_TYPE_DISK, GetFileInformationByHandle, GetFileType, SYNCHRONIZE,
        },
        System::IO::IO_STATUS_BLOCK,
    },
};

pub(super) fn open_root(
    root: &Path,
    cancel: &CancellationToken,
    project: &CancellationToken,
) -> Result<File> {
    let mut parts = root.components();
    let drive = match parts.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
            _ => anyhow::bail!("Project search requires a local disk folder on Windows"),
        },
        _ => anyhow::bail!("Project folder must be absolute"),
    };
    ensure!(
        drive.is_ascii_alphabetic() && parts.next() == Some(Component::RootDir),
        "Project folder must use an absolute disk path"
    );
    let mut directory = native_open(
        None,
        &OsString::from(format!("\\??\\{}:\\", char::from(drive))),
        true,
    )?;
    for part in parts {
        super::super::index::check(cancel, project)?;
        let Component::Normal(name) = part else {
            anyhow::bail!("Project folder cannot contain traversal")
        };
        directory = open_relative(&directory, name, true)?;
    }
    Ok(directory)
}
pub(super) fn open_relative(parent: &File, name: &OsStr, directory: bool) -> Result<File> {
    let words: Vec<_> = name.encode_wide().collect();
    ensure!(
        !words.is_empty()
            && !words
                .iter()
                .any(|word| *word == 0 || matches!(*word, 47 | 58 | 92))
            && name != "."
            && name != "..",
        "Project file contains an unsupported path component"
    );
    native_open(Some(parent), name, directory)
}
fn native_open(parent: Option<&File>, name: &OsStr, directory: bool) -> Result<File> {
    let mut words: Vec<_> = name.encode_wide().collect();
    let length = u16::try_from(words.len() * size_of::<u16>())?;
    let unicode = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: words.as_mut_ptr(),
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
    // SAFETY: the counted UTF-16 name, object attributes and borrowed parent stay
    // live throughout the synchronous call. FILE_OPEN never creates or writes.
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
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
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
    let file = if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
        // SAFETY: this is a newly owned handle returned by NtCreateFile.
        Some(unsafe { File::from_raw_handle(handle) })
    } else {
        None
    };
    ensure!(
        status == 0,
        "Project file changed or cannot be read (Windows status 0x{:08x})",
        status as u32
    );
    let file = file.context("Windows did not return a file handle")?;
    // SAFETY: File owns a live handle and info has the exact documented layout.
    ensure!(
        unsafe { GetFileType(file.as_raw_handle()) } == FILE_TYPE_DISK,
        "Project search requires regular disk files"
    );
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the output pointer names the exact Win32 result structure.
    ensure!(
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } != 0,
        "Windows could not verify this project file"
    );
    ensure!(
        info.dwFileAttributes
            & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DEVICE | FILE_ATTRIBUTE_OFFLINE)
            == 0,
        "Project search cannot follow links or offline placeholders"
    );
    ensure!(
        (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) == directory,
        "Project file type changed while opening"
    );
    Ok(file)
}
