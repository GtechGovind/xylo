//! Imports the production search reader without the full application graph.
//! Cross-target checks qualify types only; the native filesystem tests require
//! execution on Windows and do not establish whole-application qualification.
#[cfg(windows)]
mod index {
    pub(crate) fn check(
        cancel: &tokio_util::sync::CancellationToken,
        project: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            !cancel.is_cancelled() && !project.is_cancelled(),
            "Search cancelled"
        );
        Ok(())
    }
}
#[cfg(windows)]
#[path = "../../../src/workspace/reader.rs"]
mod reader;

#[cfg(windows)]
pub fn open_project_file(
    root: &std::path::Path,
    path: &std::path::Path,
) -> anyhow::Result<std::fs::File> {
    let cancel = tokio_util::sync::CancellationToken::new();
    reader::RootReader::open(root, &cancel, &cancel)?.file(path, &cancel, &cancel)
}

#[cfg(all(test, windows))]
mod tests {
    use std::{
        io::Read,
        os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle},
        path::Path,
    };
    use tokio_util::sync::CancellationToken;

    #[test]
    fn real_nested_unicode_files_open_through_production_reader() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let nested = root.join("nested/你好");
        std::fs::create_dir_all(&nested).unwrap();
        let path = nested.join("evidence.txt");
        std::fs::write(&path, "Local project evidence ✓").unwrap();
        let mut file = super::open_project_file(&root, &path)
            .expect("Directory and regular-file NT opens must accept their distinct option sets");
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        assert_eq!(text, "Local project evidence ✓");
    }

    #[test]
    fn traversal_streams_wrong_types_and_cancellation_remain_refused() {
        let directory = tempfile::tempdir().unwrap();
        let base = directory.path().canonicalize().unwrap();
        let root = base.join("project");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join("folder")).unwrap();
        std::fs::write(base.join("outside.txt"), "outside project").unwrap();
        std::fs::write(root.join("fact.txt"), "inside project").unwrap();
        std::fs::write(root.join("fact.txt:hidden"), "alternate stream").unwrap();
        assert!(super::open_project_file(&root, &root.join("../outside.txt")).is_err());
        assert!(super::open_project_file(&root, &root.join("folder")).is_err());
        assert!(super::open_project_file(&root, &root.join("fact.txt:hidden")).is_err());
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(
            super::reader::RootReader::open(&root, &cancel, &CancellationToken::new())
                .err()
                .unwrap()
                .to_string()
                .contains("cancelled")
        );
    }

    #[test]
    fn an_unprivileged_external_directory_junction_cannot_escape_the_project() {
        let directory = tempfile::tempdir().unwrap();
        let base = directory.path().canonicalize().unwrap();
        let root = base.join("project");
        let outside = base.join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("private.txt"), "outside project").unwrap();
        let link = root.join("external");
        create_junction(&link, &outside);
        let error = super::open_project_file(&root, &link.join("private.txt")).unwrap_err();
        assert!(error.to_string().contains("outside the selected project"));
    }

    // Documented mount-point reparse layout and FSCTL_SET_REPARSE_POINT:
    // https://learn.microsoft.com/en-us/windows-hardware/drivers/ifs/fsctl-set-reparse-point
    // Junctions require no symlink privilege; every object belongs to TempDir.
    fn create_junction(link: &Path, target: &Path) {
        use windows_sys::Win32::{
            Storage::FileSystem::{
                FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
                FILE_SHARE_READ, FILE_SHARE_WRITE,
            },
            System::{
                IO::DeviceIoControl, Ioctl::FSCTL_SET_REPARSE_POINT,
                SystemServices::IO_REPARSE_TAG_MOUNT_POINT,
            },
        };
        std::fs::create_dir(link).unwrap();
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(link)
            .unwrap();
        let target: Vec<_> = target.as_os_str().encode_wide().collect();
        let ordinary = target.strip_prefix(&[92, 92, 63, 92]).unwrap_or(&target);
        let substitute: Vec<_> = "\\??\\"
            .encode_utf16()
            .chain(ordinary.iter().copied())
            .collect();
        let substitute_length = u16::try_from(substitute.len() * 2).unwrap();
        let print_length = u16::try_from(ordinary.len() * 2).unwrap();
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
        buffer.extend_from_slice(&(8 + substitute_length + 2 + print_length + 2).to_le_bytes());
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
        let mut returned = 0;
        // SAFETY: the bounded buffer has the documented mount-point layout;
        // the handle owns a newly created, local fixture directory.
        assert_ne!(
            unsafe {
                DeviceIoControl(
                    handle.as_raw_handle(),
                    FSCTL_SET_REPARSE_POINT,
                    buffer.as_ptr().cast(),
                    buffer.len() as u32,
                    std::ptr::null_mut(),
                    0,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            },
            0,
            "Create local junction fixture: {}",
            std::io::Error::last_os_error()
        );
    }
}
