//! Handle-relative NT opens shared by the Windows readers.
//!
//! Each open names one component relative to an already open directory
//! handle (`OBJECT_ATTRIBUTES.RootDirectory`) with `OBJ_DONT_REPARSE`, so no
//! path is resolved from a string the caller checked earlier. Callers choose
//! access, sharing and options (always including `FILE_OPEN_REPARSE_POINT`)
//! and decide what the handle they got may be.

use anyhow::{Result, ensure};
use std::{
    ffi::OsStr,
    fmt,
    fs::File,
    mem::size_of,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle},
    },
};
use windows_sys::{
    Wdk::{
        Foundation::OBJECT_ATTRIBUTES,
        Storage::FileSystem::{FILE_OPEN, NtCreateFile},
    },
    Win32::{
        Foundation::{
            HANDLE, INVALID_HANDLE_VALUE, NTSTATUS, OBJ_CASE_INSENSITIVE, OBJ_DONT_REPARSE,
            STATUS_INVALID_PARAMETER, STATUS_OBJECT_NAME_INVALID, STATUS_UNSUCCESSFUL,
            UNICODE_STRING,
        },
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_TAG_INFO, FILE_ID_INFO, FILE_TYPE_DISK,
            FileAttributeTagInfo, FileIdInfo, GetFileInformationByHandle,
            GetFileInformationByHandleEx, GetFileType,
        },
        System::IO::IO_STATUS_BLOCK,
    },
};

/// A file's identity: its volume serial number and 128-bit file id. Names,
/// case, short (8.3) names and links all reach the same identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    volume: u64,
    file: [u8; 16],
}

/// The `NTSTATUS` of an open that returned no usable handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Refused(pub(crate) NTSTATUS);

impl fmt::Display for Refused {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Windows status 0x{:08x}", self.0 as u32)
    }
}

/// Opens the existing object `name` with `NtCreateFile`, relative to
/// `parent` when given (then `name` must be one validated component) or as
/// an absolute NT name otherwise. Never creates or overwrites. Any status
/// other than `STATUS_SUCCESS`, including positive informational ones such
/// as an oplock break in progress, is refused and its handle closed.
///
/// Relative opens add `OBJ_DONT_REPARSE`. Windows before 10 1803 does not
/// know that flag and answers `STATUS_INVALID_PARAMETER`; the open is then
/// retried without it, as std does. `FILE_OPEN_REPARSE_POINT`, which every
/// caller passes, still keeps the single component from being followed.
pub(crate) fn open(
    parent: Option<&File>,
    name: &OsStr,
    access: u32,
    share: u32,
    options: u32,
) -> std::result::Result<File, Refused> {
    let attributes = OBJ_CASE_INSENSITIVE
        | if parent.is_some() {
            OBJ_DONT_REPARSE
        } else {
            0
        };
    match create(parent, name, access, share, options, attributes) {
        Err(Refused(STATUS_INVALID_PARAMETER)) if parent.is_some() => {
            create(parent, name, access, share, options, OBJ_CASE_INSENSITIVE)
        }
        result => result,
    }
}

/// One `NtCreateFile` call with the given object attribute flags.
fn create(
    parent: Option<&File>,
    name: &OsStr,
    access: u32,
    share: u32,
    options: u32,
    flags: u32,
) -> std::result::Result<File, Refused> {
    let mut units: Vec<u16> = name.encode_wide().collect();
    let length = u16::try_from(units.len() * size_of::<u16>())
        .map_err(|_| Refused(STATUS_OBJECT_NAME_INVALID))?;
    let unicode = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: units.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.map_or(std::ptr::null_mut(), |file| file.as_raw_handle()),
        ObjectName: &unicode,
        Attributes: flags,
        ..Default::default()
    };
    let mut io = IO_STATUS_BLOCK::default();
    let mut handle: HANDLE = std::ptr::null_mut();
    // SAFETY: all NT structures and the counted UTF-16 name stay live for
    // the synchronous call; the parent handle is borrowed and a returned
    // handle is owned exactly once below. FILE_OPEN never creates.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            access,
            &attributes,
            &mut io,
            std::ptr::null(),
            0,
            share,
            FILE_OPEN,
            options,
            std::ptr::null(),
            0,
        )
    };
    let file = (!handle.is_null() && handle != INVALID_HANDLE_VALUE)
        // SAFETY: this is a new handle returned by NtCreateFile.
        .then(|| unsafe { File::from_raw_handle(handle) });
    match (status, file) {
        (0, Some(file)) => Ok(file),
        (0, None) => Err(Refused(STATUS_UNSUCCESSFUL)),
        (status, _) => Err(Refused(status)),
    }
}

/// The attributes of an open local-disk object; anything that is not a disk
/// file (a pipe, console or character device) is refused.
pub(crate) fn information(file: &File) -> Result<BY_HANDLE_FILE_INFORMATION> {
    // SAFETY: the File owns a live handle for these read-only queries.
    ensure!(
        unsafe { GetFileType(file.as_raw_handle()) } == FILE_TYPE_DISK,
        "Windows file is not on a local disk"
    );
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the output buffer has the API's exact structure size.
    ensure!(
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } != 0,
        "Windows could not verify file attributes: {}",
        std::io::Error::last_os_error()
    );
    Ok(info)
}

/// The attributes and reparse tag of an open object, from
/// `FileAttributeTagInfo`. The tag is meaningful only when the attributes
/// include `FILE_ATTRIBUTE_REPARSE_POINT`.
pub(crate) fn attribute_tag(file: &File) -> Result<(u32, u32)> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: the output type/size match FileAttributeTagInfo and the handle
    // is live.
    ensure!(
        unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileAttributeTagInfo,
                std::ptr::from_mut(&mut info).cast(),
                size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
            )
        } != 0,
        "Windows could not read file attributes: {}",
        std::io::Error::last_os_error()
    );
    Ok((info.FileAttributes, info.ReparseTag))
}

/// The [`Identity`] of an open file. File systems without stable file ids
/// are refused rather than compared by a zero id.
pub(crate) fn identity(file: &File) -> Result<Identity> {
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

/// Refuses a component that NT would read as more than one plain name:
/// separators, alternate data streams (`:`), wildcards, control characters,
/// trailing dots or spaces (which Win32 strips but NT keeps) and DOS device
/// names.
pub(crate) fn validate_component(name: &OsStr) -> Result<()> {
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
