//! Privilege-free links for the read-only, neutral Git metadata view.
use anyhow::{Context, Result, ensure};
use std::{
    os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle},
    path::Path,
};
use windows_sys::Win32::{
    Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    },
    System::IO::DeviceIoControl,
};

pub(super) fn read_metadata_link(source: &Path, target: &Path) -> Result<()> {
    if !source.exists() {
        return Ok(());
    }
    if source.is_dir() {
        junction(source, target)
    } else {
        // Copies also work when the repository and temporary directory are on
        // different volumes, unlike hard links. Never copy executable config.
        std::fs::copy(source, target)
            .with_context(|| format!("Snapshot Git metadata {}", source.display()))?;
        Ok(())
    }
}

fn junction(source: &Path, target: &Path) -> Result<()> {
    // Junctions are directory mount points, not privileged symbolic links.
    // Canonicalization supplies an absolute extended-length local drive path.
    let source = source.canonicalize()?;
    let source: Vec<u16> = source.as_os_str().encode_wide().collect();
    let prefix: Vec<u16> = r"\\?\".encode_utf16().collect();
    ensure!(
        source.starts_with(&prefix)
            && source.len() >= 7
            && ((b'A' as u16..=b'Z' as u16).contains(&source[4])
                || (b'a' as u16..=b'z' as u16).contains(&source[4]))
            && source[5] == b':' as u16
            && source[6] == b'\\' as u16,
        "Native Git metadata junctions require a local drive path"
    );
    let ordinary = &source[4..];
    let substitute: Vec<u16> = "\\??\\"
        .encode_utf16()
        .chain(ordinary.iter().copied())
        .collect();
    // REPARSE_DATA_BUFFER has a maximum size of 16 KiB, including its header.
    let size = 16 + (substitute.len() + 1 + ordinary.len() + 1) * 2;
    ensure!(size <= 16 * 1024, "Git metadata junction path is too long");
    let substitute_length = (substitute.len() * 2) as u16;
    let print_length = (ordinary.len() * 2) as u16;
    let mut buffer = Vec::with_capacity(size);
    // IO_REPARSE_TAG_MOUNT_POINT; FSCTL_SET_REPARSE_POINT below is 0x000900a4.
    buffer.extend_from_slice(&0xa0000003_u32.to_le_bytes());
    buffer.extend_from_slice(&((size - 8) as u16).to_le_bytes());
    buffer.extend_from_slice(&0_u16.to_le_bytes());
    for value in [0, substitute_length, substitute_length + 2, print_length] {
        buffer.extend_from_slice(&value.to_le_bytes());
    }
    for unit in substitute
        .into_iter()
        .chain([0])
        .chain(ordinary.iter().copied())
        .chain([0])
    {
        buffer.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::create_dir(target)?;
    let result = (|| -> std::io::Result<()> {
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(target)?;
        let mut returned = 0;
        // SAFETY: handle owns the newly created directory, opened without
        // following reparse points. buffer is a bounded mount-point buffer and
        // both it and returned stay alive for this synchronous call.
        if unsafe {
            DeviceIoControl(
                handle.as_raw_handle(),
                0x000900a4,
                buffer.as_ptr().cast(),
                buffer.len() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    })();
    if result.is_err() {
        // Remove only our empty directory, never traverse a junction target.
        let _ = std::fs::remove_dir(target);
    }
    result.with_context(|| format!("Link Git metadata directory {}", target.display()))
}
