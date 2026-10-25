//! Handle-relative Windows storage. No namespace lookup is authorized by a
//! canonicalize-then-reopen comparison. Actual Windows execution is qualified
//! separately from cross-target type checks.
use anyhow::{Context, Result, ensure};
use std::{
    ffi::{OsStr, OsString},
    fs::File,
    mem::{offset_of, size_of},
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle},
    },
    path::{Component, Path, Prefix},
};
use windows_sys::{
    Wdk::{
        Foundation::OBJECT_ATTRIBUTES,
        Storage::FileSystem::{
            FILE_COMPLETE_IF_OPLOCKED, FILE_CREATE, FILE_DIRECTORY_FILE,
            FILE_DISPOSITION_INFORMATION, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_IF,
            FILE_OPEN_NO_RECALL, FILE_OPEN_REPARSE_POINT, FILE_RENAME_INFORMATION,
            FILE_SYNCHRONOUS_IO_NONALERT, FileDispositionInformation, FileFsDeviceInformation,
            FileRenameInformation, NtCreateFile, NtQueryVolumeInformationFile,
            NtSetInformationFile,
        },
        System::SystemServices::{FILE_FS_DEVICE_INFORMATION, FILE_REMOTE_DEVICE},
    },
    Win32::{
        Foundation::{
            HANDLE, INVALID_HANDLE_VALUE, OBJ_CASE_INSENSITIVE, OBJ_DONT_REPARSE,
            RtlNtStatusToDosError, UNICODE_STRING,
        },
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_DEVICE, FILE_ATTRIBUTE_DIRECTORY,
            FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DEVICE_DISK, FILE_ID_INFO,
            FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE, FILE_TYPE_DISK, FILE_WRITE_DATA,
            FileIdInfo, GetFileInformationByHandle, GetFileInformationByHandleEx, GetFileType,
            GetFinalPathNameByHandleW, SYNCHRONIZE, VOLUME_NAME_GUID,
        },
        System::IO::IO_STATUS_BLOCK,
    },
};

pub(super) struct Authority {
    handles: Vec<File>,
}

fn component(value: &OsStr) -> Result<()> {
    let units: Vec<_> = value.encode_wide().collect();
    ensure!(
        !units.is_empty()
            && units.len() <= 255
            && !units
                .iter()
                .any(|unit| *unit < 32
                    || matches!(*unit, 34 | 42 | 47 | 58 | 60 | 62 | 63 | 92 | 124))
            && !matches!(units.last(), Some(32 | 46)),
        "Invalid output directory component"
    );
    let stem = value
        .to_string_lossy()
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    ensure!(
        !matches!(
            stem.as_str(),
            "con" | "prn" | "aux" | "nul" | "conin$" | "conout$" | "clock$"
        ) && !stem
            .strip_prefix("com")
            .or_else(|| stem.strip_prefix("lpt"))
            .is_some_and(|suffix| matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )),
        "Output directory device aliases are refused"
    );
    Ok(())
}

fn nt_error(status: i32) -> anyhow::Error {
    std::io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32).into()
}

fn native_open(parent: Option<&File>, name: &OsStr, directory: bool, create: bool) -> Result<File> {
    native_open_mode(parent, name, directory, create, false, create)
}
fn native_open_mode(
    parent: Option<&File>,
    name: &OsStr,
    directory: bool,
    create: bool,
    shared_lock: bool,
    writable: bool,
) -> Result<File> {
    if parent.is_some() {
        component(name)?;
    }
    let mut units: Vec<_> = name.encode_wide().collect();
    let length = u16::try_from(units.len() * 2)?;
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
    // SAFETY: counted name, attributes, parent and output storage live through
    // this synchronous call. FILE_CREATE never opens an existing stream.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            FILE_READ_ATTRIBUTES
                | SYNCHRONIZE
                | if directory {
                    FILE_TRAVERSE
                } else {
                    FILE_READ_DATA | DELETE | if writable { FILE_WRITE_DATA } else { 0 }
                },
            &attributes,
            &mut io,
            std::ptr::null(),
            0,
            if directory {
                FILE_SHARE_READ | FILE_SHARE_WRITE
            } else if create && !shared_lock {
                // Verification opens the published identity while the source
                // handle is still live. Permit delete/rename sharing, but no
                // second writer; parent authority handles fence directory moves.
                FILE_SHARE_READ | FILE_SHARE_DELETE
            } else {
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
            },
            if create {
                if directory { FILE_OPEN_IF } else { FILE_CREATE }
            } else {
                FILE_OPEN
            },
            FILE_OPEN_REPARSE_POINT
                | FILE_SYNCHRONOUS_IO_NONALERT
                | FILE_COMPLETE_IF_OPLOCKED
                | if directory {
                    // NtCreateFile rejects FILE_OPEN_NO_RECALL with
                    // FILE_DIRECTORY_FILE (STATUS_INVALID_PARAMETER).
                    // Reparse/offline checks still apply to the opened handle.
                    FILE_DIRECTORY_FILE
                } else {
                    FILE_NON_DIRECTORY_FILE | FILE_OPEN_NO_RECALL
                },
            std::ptr::null(),
            0,
        )
    };
    let file = if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
        // SAFETY: the returned handle transfers once into File, including on
        // informational/refused results so every returned handle is closed.
        Some(unsafe { File::from_raw_handle(handle) })
    } else {
        None
    };
    if status != 0 {
        return Err(nt_error(status));
    }
    let file = file.context("Windows did not return output storage authority")?;
    attributes_for(&file, directory)?;
    Ok(file)
}

fn attributes_for(file: &File, directory: bool) -> Result<()> {
    ensure!(
        unsafe { GetFileType(file.as_raw_handle()) } == FILE_TYPE_DISK,
        "Output storage requires a disk file"
    );
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    ensure!(
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } != 0,
        "Windows could not inspect output storage: {}",
        std::io::Error::last_os_error()
    );
    ensure!(
        info.dwFileAttributes
            & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DEVICE | FILE_ATTRIBUTE_OFFLINE)
            == 0,
        "Output storage refuses reparse points, devices and offline placeholders"
    );
    ensure!(
        (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) == directory,
        "Output storage object has the wrong file type"
    );
    if !directory {
        ensure!(
            info.nNumberOfLinks == 1,
            "Output storage refuses hard-link aliases"
        );
    }
    Ok(())
}

fn identity(file: &File) -> Result<(u64, [u8; 16])> {
    let mut info = FILE_ID_INFO::default();
    ensure!(
        unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileIdInfo,
                std::ptr::from_mut(&mut info).cast(),
                size_of::<FILE_ID_INFO>() as u32,
            )
        } != 0,
        "Windows could not verify output identity: {}",
        std::io::Error::last_os_error()
    );
    ensure!(
        info.FileId.Identifier != [0; 16],
        "Output storage requires a reliable file identity"
    );
    Ok((info.VolumeSerialNumber, info.FileId.Identifier))
}

fn volume_root(file: &File) -> Result<()> {
    let mut io = IO_STATUS_BLOCK::default();
    let mut device = FILE_FS_DEVICE_INFORMATION::default();
    let status = unsafe {
        NtQueryVolumeInformationFile(
            file.as_raw_handle(),
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
        "Output storage requires a verified local disk volume"
    );
    let mut name = [0_u16; 512];
    let length = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            name.as_mut_ptr(),
            name.len() as u32,
            VOLUME_NAME_GUID | FILE_NAME_NORMALIZED,
        )
    } as usize;
    ensure!(
        length > 0 && length < name.len(),
        "Windows could not qualify the output volume root"
    );
    let name = String::from_utf16(&name[..length])?;
    ensure!(
        name.strip_prefix("\\\\?\\Volume{")
            .and_then(|name| name.strip_suffix("}\\"))
            .is_some_and(|guid| guid.len() == 36
                && guid.bytes().enumerate().all(|(index, byte)| {
                    if [8, 13, 18, 23].contains(&index) {
                        byte == b'-'
                    } else {
                        byte.is_ascii_hexdigit()
                    }
                })),
        "Mapped directories cannot serve as output volume roots"
    );
    Ok(())
}

impl Authority {
    fn directory(&self) -> &File {
        self.handles.last().expect("Output authority owns a root")
    }

    pub fn open(path: &Path) -> Result<Self> {
        let mut components = path.components();
        let drive = match components.next() {
            Some(Component::Prefix(prefix)) => match prefix.kind() {
                Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive.to_ascii_uppercase(),
                _ => anyhow::bail!("Output storage requires a local disk path"),
            },
            _ => anyhow::bail!("Output storage requires an absolute disk path"),
        };
        ensure!(
            matches!(components.next(), Some(Component::RootDir)) && drive.is_ascii_alphabetic(),
            "Output storage refuses drive-relative paths"
        );
        let root = native_open(
            None,
            &OsString::from(format!("\\??\\{}:\\", char::from(drive))),
            true,
            false,
        )?;
        volume_root(&root)?;
        let mut authority = Self {
            handles: vec![root],
        };
        let mut depth = 0;
        for component in components {
            let Component::Normal(name) = component else {
                anyhow::bail!("Output storage cannot traverse directories")
            };
            depth += 1;
            ensure!(depth <= 256, "Output authority exceeds its depth budget");
            let next = native_open(Some(authority.directory()), name, true, false)?;
            authority.handles.push(next);
        }
        Ok(authority)
    }

    pub fn child(&self, name: &str, create: bool) -> Result<Self> {
        let file = native_open(Some(self.directory()), OsStr::new(name), true, create)?;
        let mut handles = self
            .handles
            .iter()
            .map(File::try_clone)
            .collect::<std::io::Result<Vec<_>>>()?;
        handles.push(file);
        Ok(Self { handles })
    }

    pub fn file(&self, name: &str, create: bool) -> Result<File> {
        native_open(Some(self.directory()), OsStr::new(name), false, create)
    }

    pub fn file_for_sync(&self, name: &str) -> Result<File> {
        // FlushFileBuffers requires write access. Only private staging uses
        // this handle; artifact/hash verification remains read-only.
        native_open_mode(
            Some(self.directory()),
            OsStr::new(name),
            false,
            false,
            false,
            true,
        )
    }

    pub fn lock_file(&self, name: &str) -> Result<File> {
        match native_open_mode(
            Some(self.directory()),
            OsStr::new(name),
            false,
            true,
            true,
            true,
        ) {
            Ok(file) => Ok(file),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists) =>
            {
                native_open_mode(
                    Some(self.directory()),
                    OsStr::new(name),
                    false,
                    false,
                    true,
                    true,
                )
            }
            Err(error) => Err(error),
        }
    }

    pub fn recovery_file(&self, name: &str) -> Result<File> {
        self.file(name, false)
    }

    pub fn sync(&self) -> Result<()> {
        // FlushFileBuffers is not supported for ordinary directory handles.
        // File bytes are synced; atomic NT rename is used for metadata/commit.
        Ok(())
    }

    pub fn same(&self, other: &Self) -> Result<bool> {
        Ok(identity(self.directory())? == identity(other.directory())?)
    }

    pub fn rename(
        &self,
        file: &File,
        _from: &str,
        target: &Self,
        to: &str,
        replace: bool,
    ) -> Result<()> {
        regular(file)?;
        component(OsStr::new(to))?;
        let name: Vec<_> = OsStr::new(to).encode_wide().collect();
        let size = size_of::<FILE_RENAME_INFORMATION>() + name.len() * 2;
        // u64 storage guarantees pointer alignment on the supported Windows
        // architectures; the generated struct supplies ABI-correct offsets.
        let mut storage = vec![0_u64; size.div_ceil(8)];
        let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
        unsafe {
            (*info).Anonymous.ReplaceIfExists = replace;
            (*info).RootDirectory = target.directory().as_raw_handle();
            (*info).FileNameLength = (name.len() * 2) as u32;
            let destination = storage
                .as_mut_ptr()
                .cast::<u8>()
                .add(offset_of!(FILE_RENAME_INFORMATION, FileName))
                .cast::<u16>();
            std::ptr::copy_nonoverlapping(name.as_ptr(), destination, name.len());
        }
        let mut io = IO_STATUS_BLOCK::default();
        let status = unsafe {
            NtSetInformationFile(
                file.as_raw_handle(),
                &mut io,
                info.cast(),
                size as u32,
                FileRenameInformation,
            )
        };
        if status != 0 {
            return Err(nt_error(status));
        }
        regular(file)?;
        Ok(())
    }

    pub fn unlink(&self, name: &str) -> Result<()> {
        let file = self.file(name, false)?;
        regular(&file)?;
        let information = FILE_DISPOSITION_INFORMATION { DeleteFile: true };
        let mut io = IO_STATUS_BLOCK::default();
        let status = unsafe {
            NtSetInformationFile(
                file.as_raw_handle(),
                &mut io,
                std::ptr::from_ref(&information).cast(),
                size_of::<FILE_DISPOSITION_INFORMATION>() as u32,
                FileDispositionInformation,
            )
        };
        if status != 0 {
            return Err(nt_error(status));
        }
        Ok(())
    }
}

pub(super) fn regular(file: &File) -> Result<()> {
    attributes_for(file, false)
}

pub(super) fn same_file(a: &File, b: &File) -> Result<bool> {
    Ok(identity(a)? == identity(b)?)
}

pub(super) fn regular_pair(_file: &File) -> Result<()> {
    anyhow::bail!("Windows output publication never authorizes hard-link alias cleanup")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn nested_directory_authority_keeps_ancestor_rename_fences() {
        let temporary = tempfile::tempdir().unwrap();
        let nested = temporary.path().join("ancestor/child");
        std::fs::create_dir_all(&nested).unwrap();
        let root = Authority::open(temporary.path()).unwrap();
        let relative = root
            .child("ancestor", false)
            .unwrap()
            .child("child", false)
            .unwrap();
        let absolute = Authority::open(&nested).unwrap();
        assert!(relative.same(&absolute).unwrap());
        assert!(absolute.child("..", false).is_err());
        absolute
            .file("captured.bin", true)
            .unwrap()
            .write_all(b"bounded output")
            .unwrap();
        let moved = temporary.path().join("moved");
        assert!(std::fs::rename(temporary.path().join("ancestor"), &moved).is_err());
        assert_eq!(
            std::fs::read(nested.join("captured.bin")).unwrap(),
            b"bounded output"
        );
        drop(absolute);
        drop(relative);
        std::fs::rename(temporary.path().join("ancestor"), &moved).unwrap();
        assert_eq!(
            std::fs::read(moved.join("child/captured.bin")).unwrap(),
            b"bounded output"
        );
    }

    #[test]
    fn published_stream_can_be_verified_while_original_handle_is_live() {
        let temporary = tempfile::tempdir().unwrap();
        let authority = Authority::open(temporary.path()).unwrap();
        let source = authority.child("private", true).unwrap();
        let target = authority.child("public", true).unwrap();
        let mut file = source.file("stream.part", true).unwrap();
        file.write_all(b"native captured bytes").unwrap();
        file.sync_all().unwrap();
        source
            .rename(&file, "stream.part", &target, "stream.bin", false)
            .unwrap();
        let mut published = target.file("stream.bin", false).unwrap();
        assert!(same_file(&file, &published).unwrap());
        let mut bytes = Vec::new();
        published.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"native captured bytes");
    }

    #[test]
    fn atomic_publication_never_replaces_unrelated_existing_output() {
        let temporary = tempfile::tempdir().unwrap();
        let authority = Authority::open(temporary.path()).unwrap();
        let source = authority.child("private", true).unwrap();
        let target = authority.child("public", true).unwrap();
        let mut original = source.file("stream.part", true).unwrap();
        original.write_all(b"new bytes").unwrap();
        target
            .file("stream.bin", true)
            .unwrap()
            .write_all(b"existing bytes")
            .unwrap();
        assert!(
            source
                .rename(&original, "stream.part", &target, "stream.bin", false)
                .is_err()
        );
        assert_eq!(
            std::fs::read(temporary.path().join("public/stream.bin")).unwrap(),
            b"existing bytes"
        );
    }

    #[test]
    fn existing_private_staging_reopens_with_flush_authority() {
        let temporary = tempfile::tempdir().unwrap();
        let authority = Authority::open(temporary.path()).unwrap();
        let data = authority.child("data", true).unwrap();
        data.file("stream.part", true)
            .unwrap()
            .write_all(b"retained prefix")
            .unwrap();
        data.file_for_sync("stream.part")
            .unwrap()
            .sync_all()
            .unwrap();
    }

    #[test]
    fn hardlinked_files_are_never_output_authorities() {
        let temporary = tempfile::tempdir().unwrap();
        let authority = Authority::open(temporary.path()).unwrap();
        let data = authority.child("data", true).unwrap();
        data.file("one.bin", true)
            .unwrap()
            .write_all(b"original")
            .unwrap();
        std::fs::hard_link(
            temporary.path().join("data/one.bin"),
            temporary.path().join("data/two.bin"),
        )
        .unwrap();
        assert!(data.file("one.bin", false).is_err());
        assert!(data.recovery_file("two.bin").is_err());
    }

    #[test]
    fn lease_handles_allow_a_second_open_instead_of_a_sharing_error() {
        let temporary = tempfile::tempdir().unwrap();
        let authority = Authority::open(temporary.path()).unwrap();
        let first = authority.lock_file("storage.lock").unwrap();
        let second = authority.lock_file("storage.lock").unwrap();
        assert!(same_file(&first, &second).unwrap());
    }
}
