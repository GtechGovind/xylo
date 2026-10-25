use super::*;
use std::io::Read;

struct Fixture {
    _directory: tempfile::TempDir,
    config: crate::CapabilityConfig,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        let management = directory.path().join("profile/native-management");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&management).unwrap();
        let config = crate::CapabilityConfig {
            workspace: workspace.canonicalize().unwrap(),
            management_root: Some(management.canonicalize().unwrap()),
            ..Default::default()
        };
        Self {
            _directory: directory,
            config,
        }
    }

    fn read(&self, raw: &str) -> Result<String> {
        let mut file = open_workspace_read(&self.config, raw, &CancellationToken::new())?;
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        Ok(text)
    }

    fn private(&self) -> &Path {
        self.config
            .management_root
            .as_ref()
            .unwrap()
            .parent()
            .unwrap()
    }
}

#[test]
fn real_local_file_reads_support_unicode_and_canonical_workspace_roots() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.config.workspace.join("notes")).unwrap();
    let file = fixture.config.workspace.join("notes/你好.txt");
    std::fs::write(&file, "Approved project evidence ✓").unwrap();
    assert_eq!(
        fixture.read("notes/你好.txt").unwrap(),
        "Approved project evidence ✓"
    );
    // User input uses ordinary drive paths; internal canonical roots may have
    // the verbatim drive prefix returned by std::fs::canonicalize on Windows.
    let ordinary = file
        .to_str()
        .unwrap()
        .strip_prefix("\\\\?\\")
        .unwrap_or(file.to_str().unwrap());
    assert_eq!(
        fixture.read(ordinary).unwrap(),
        "Approved project evidence ✓"
    );
}

#[test]
fn directory_authorities_and_regular_files_use_compatible_nt_open_modes() {
    let fixture = Fixture::new();
    let nested = fixture.config.workspace.join("authority-child");
    std::fs::create_dir(&nested).unwrap();
    std::fs::write(nested.join("fact.txt"), "retained directory authority").unwrap();
    let cancel = CancellationToken::new();
    let authority = open_authority(
        &drive_path(&fixture.config.workspace).unwrap(),
        &cancel,
        None,
    )
    .expect("Volume and ancestor directories must accept the directory NT open mode");
    let directory = open_relative(authority.directory(), OsStr::new("authority-child"), true)
        .expect("A relative directory must accept the directory NT open mode");
    let mut file = open_relative(&directory, OsStr::new("fact.txt"), false)
        .expect("A regular file must retain its separate NT open mode");
    let mut content = String::new();
    file.read_to_string(&mut content).unwrap();
    assert_eq!(content, "retained directory authority");
    assert!(
        std::fs::rename(&nested, fixture.config.workspace.join("moved")).is_err(),
        "The valid directory open mode must still deny ancestor replacement"
    );
    assert!(
        std::fs::write(nested.join("fact.txt"), "replacement").is_err(),
        "The valid file open mode must still deny concurrent writes"
    );
    drop(file);
    drop(directory);
    drop(authority);
    std::fs::rename(&nested, fixture.config.workspace.join("moved")).unwrap();
}

#[test]
fn namespace_stream_device_and_traversal_aliases_are_refused_before_io() {
    let workspace = drive_path(Path::new("C:\\work")).unwrap();
    for raw in [
        "",
        "..\\private.txt",
        "sub/../file",
        "./file",
        "sub//file",
        "file.txt:secret",
        "file::$DATA",
        "C:file",
        "D:\\work\\file",
        "\\rooted",
        "\\\\server\\share\\file",
        "\\\\?\\C:\\work\\file",
        "\\\\.\\pipe\\test",
        "\\??\\C:\\work\\file",
        "NUL",
        "con.txt",
        "COM1",
        "LPT².txt",
        "CONIN$",
        "name.",
        "name ",
        "file\0tail",
    ] {
        assert!(
            requested_parts(raw, &workspace).is_err(),
            "Accepted unsafe path {raw:?}"
        );
    }
    assert!(requested_parts("sub/file.txt", &workspace).is_ok());
    assert!(requested_parts("C:\\work\\sub\\file.txt", &workspace).is_ok());
    assert!(requested_parts("C:\\work-extra\\file.txt", &workspace).is_err());
    assert!(requested_parts(&format!("{}file", "sub/".repeat(256)), &workspace).is_err());
    assert!(requested_parts(&"x".repeat(256), &workspace).is_err());
    assert!(drive_path(Path::new("\\\\server\\share\\work")).is_err());
    assert!(drive_path(Path::new("C:work")).is_err());
}

#[test]
fn volume_root_qualification_rejects_directory_aliases() {
    let root = "\\\\?\\Volume{12345678-1234-1234-abcd-123456789abc}\\";
    assert!(is_volume_root_name(root));
    assert!(!is_volume_root_name(&format!("{root}private\\")));
    assert!(!is_volume_root_name("\\\\server\\share\\"));
    assert!(!is_volume_root_name("\\\\?\\Volume{not-a-guid}\\"));
}

#[test]
fn private_directory_ancestors_and_hard_link_aliases_cannot_be_read() {
    let fixture = Fixture::new();
    let secret = fixture.private().join("private.txt");
    std::fs::write(&secret, "private profile secret").unwrap();
    std::fs::hard_link(&secret, fixture.config.workspace.join("alias.txt")).unwrap();
    assert!(
        fixture
            .read("alias.txt")
            .unwrap_err()
            .to_string()
            .contains("without links")
    );
    let mut broad = fixture.config.clone();
    broad.workspace = fixture._directory.path().canonicalize().unwrap();
    let error =
        open_workspace_read(&broad, "profile/private.txt", &CancellationToken::new()).unwrap_err();
    assert!(error.to_string().contains("Application data files"));
    let nested = fixture.private().join("nested");
    std::fs::create_dir(&nested).unwrap();
    std::fs::write(nested.join("private.txt"), "private nested secret").unwrap();
    broad.workspace = nested.canonicalize().unwrap();
    assert!(
        open_workspace_read(&broad, "private.txt", &CancellationToken::new())
            .unwrap_err()
            .to_string()
            .contains("Application data files")
    );
}

#[test]
fn final_file_swap_after_directory_validation_cannot_return_private_bytes() {
    let fixture = Fixture::new();
    let fact = fixture.config.workspace.join("fact.txt");
    let secret = fixture.private().join("private.txt");
    std::fs::write(&fact, "authorized file").unwrap();
    std::fs::write(&secret, "private profile secret").unwrap();
    let error = open_with_before(
        &fixture.config,
        "fact.txt",
        &CancellationToken::new(),
        || {
            std::fs::remove_file(&fact)?;
            std::fs::hard_link(&secret, &fact)?;
            Ok(())
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("without links"));
    assert!(!error.to_string().contains("private profile secret"));
}

#[test]
fn held_directories_fence_ancestor_swaps_and_final_handle_fences_writes() {
    let fixture = Fixture::new();
    let parent = fixture.config.workspace.join("subdir");
    std::fs::create_dir(&parent).unwrap();
    let fact = parent.join("fact.txt");
    std::fs::write(&fact, "authorized original").unwrap();
    let mut file = open_with_before(
        &fixture.config,
        "subdir/fact.txt",
        &CancellationToken::new(),
        || {
            assert!(
                std::fs::rename(&parent, fixture.config.workspace.join("moved")).is_err(),
                "An authority directory must deny rename sharing"
            );
            Ok(())
        },
    )
    .unwrap();
    assert!(std::fs::write(&fact, "replacement").is_err());
    assert!(std::fs::rename(&fact, parent.join("moved.txt")).is_err());
    let mut text = String::new();
    file.read_to_string(&mut text).unwrap();
    assert_eq!(text, "authorized original");
    drop(file);
    std::fs::write(&fact, "replacement after lease").unwrap();
    assert_eq!(
        fixture.read("subdir/fact.txt").unwrap(),
        "replacement after lease"
    );
}

#[test]
fn cancellation_before_or_after_directory_validation_never_returns_a_handle() {
    let fixture = Fixture::new();
    std::fs::write(fixture.config.workspace.join("fact.txt"), "valid file").unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(
        open_workspace_read(&fixture.config, "fact.txt", &cancel)
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
    let cancel = CancellationToken::new();
    assert!(
        open_with_before(&fixture.config, "fact.txt", &cancel, || {
            cancel.cancel();
            Ok(())
        })
        .unwrap_err()
        .to_string()
        .contains("cancelled")
    );
    assert!(
        fixture.read("fact.txt").is_ok(),
        "Cancellation must close temporary directory authorities"
    );
}

#[test]
fn actual_junctions_symlinks_streams_and_wrong_types_are_refused() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.private().join("private.txt"),
        "private profile secret",
    )
    .unwrap();
    let junction = fixture.config.workspace.join("linked");
    create_junction(&junction, fixture.private()).unwrap();
    assert!(fixture.read("linked/private.txt").is_err());
    let mut alias = fixture.config.clone();
    alias.workspace = junction.clone();
    assert!(open_workspace_read(&alias, "private.txt", &CancellationToken::new()).is_err());
    let link = fixture.config.workspace.join("symlink.txt");
    match std::os::windows::fs::symlink_file(fixture.private().join("private.txt"), &link) {
        Ok(()) => assert!(fixture.read("symlink.txt").is_err()),
        Err(error) if error.raw_os_error() == Some(1314) => eprintln!(
            "File-symlink creation requires Developer Mode or privilege on this runner; junction denial remains exercised"
        ),
        Err(error) => panic!("Could not create symlink fixture: {error}"),
    }
    let fact = fixture.config.workspace.join("fact.txt");
    std::fs::write(&fact, "authorized").unwrap();
    std::fs::write(
        fixture.config.workspace.join("fact.txt:private"),
        "stream secret",
    )
    .unwrap();
    assert!(fixture.read("fact.txt:private").is_err());
    assert!(fixture.read("linked").is_err());
    std::fs::create_dir(fixture.config.workspace.join("directory")).unwrap();
    assert!(fixture.read("directory").is_err());
    // Remove the junction itself before TempDir cleanup; never traverse target.
    std::fs::remove_dir(junction).unwrap();
}

/// Real NTFS mount-point fixture using the documented reparse buffer and native
/// FSCTL. Creating directory junctions does not require symlink privileges.
fn create_junction(link: &Path, target: &Path) -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::{
        Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
            FILE_SHARE_WRITE,
        },
        System::{
            IO::DeviceIoControl, Ioctl::FSCTL_SET_REPARSE_POINT,
            SystemServices::IO_REPARSE_TAG_MOUNT_POINT,
        },
    };
    std::fs::create_dir(link)?;
    let handle = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(link)?;
    let target: Vec<u16> = target.as_os_str().encode_wide().collect();
    let ordinary = if target.starts_with(&[92, 92, 63, 92]) {
        &target[4..]
    } else {
        &target[..]
    };
    let substitute: Vec<u16> = "\\??\\"
        .encode_utf16()
        .chain(ordinary.iter().copied())
        .collect();
    let substitute_length = u16::try_from(substitute.len() * 2)?;
    let print_length = u16::try_from(ordinary.len() * 2)?;
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
    // SAFETY: buffer is an exact bounded mount-point reparse buffer and handle
    // owns the newly created, ordinary directory within this test fixture.
    ensure!(
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
        } != 0,
        "Could not create junction fixture: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}
