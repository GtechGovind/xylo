use super::*;
use std::io::Read;

fn config(root: &Path) -> CapabilityConfig {
    let root = root.canonicalize().unwrap();
    std::fs::create_dir_all(root.join("artifacts")).unwrap();
    std::fs::create_dir_all(root.join("private")).unwrap();
    CapabilityConfig {
        workspace: root.clone(),
        artifacts: root.join("artifacts"),
        management_root: Some(root.join("private")),
        ..Default::default()
    }
}

fn read(input: InputFile) -> String {
    let mut text = String::new();
    { input.file }.read_to_string(&mut text).unwrap();
    text
}

#[test]
fn opens_workspace_and_artifact_files_once() {
    let directory = tempfile::tempdir().unwrap();
    let config = config(directory.path());
    // Joined one name at a time: canonical Windows roots are verbatim
    // (`\\?\C:\...`), where `/` is not a separator.
    let notes = config.workspace.join("docs").join("notes.txt");
    std::fs::create_dir(config.workspace.join("docs")).unwrap();
    std::fs::write(&notes, "notes").unwrap();
    std::fs::write(config.artifacts.join("take.wav"), "audio").unwrap();
    let input = open_input(&config, "docs/./notes.txt").unwrap();
    assert_eq!(input.root, InputRoot::Workspace);
    assert_eq!(input.path, notes);
    assert_eq!(read(input), "notes");
    let absolute = config.artifacts.join("take.wav");
    let input = open_input(&config, &absolute.to_string_lossy()).unwrap();
    assert_eq!(input.root, InputRoot::Artifacts);
    assert_eq!(read(input), "audio");
}

#[cfg(unix)]
#[test]
fn links_traversal_and_private_files_are_refused() {
    let directory = tempfile::tempdir().unwrap();
    let config = config(directory.path());
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), "secret").unwrap();
    std::fs::write(config.workspace.join("inside.txt"), "inside").unwrap();
    std::fs::write(config.workspace.join("private/catalog.json"), "{}").unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("secret"),
        config.workspace.join("file-link"),
    )
    .unwrap();
    // A link that stays inside is read and walked; see the test below.
    std::os::unix::fs::symlink(
        config.workspace.join("inside.txt"),
        config.workspace.join("inner"),
    )
    .unwrap();
    std::os::unix::fs::symlink(outside.path(), config.workspace.join("dir-link")).unwrap();
    std::fs::hard_link(outside.path().join("secret"), config.workspace.join("hard")).unwrap();
    std::fs::create_dir(config.workspace.join("fifo-dir")).unwrap();
    for requested in [
        "file-link",
        "dir-link/secret",
        "hard",
        "../secret",
        "private/catalog.json",
        "fifo-dir",
        "missing.txt",
    ] {
        assert!(open_input(&config, requested).is_err(), "{requested}");
    }
    let elsewhere = outside.path().join("secret");
    assert!(open_input(&config, &elsewhere.to_string_lossy()).is_err());
    assert!(open_input(&config, "inside.txt").is_ok());
    assert_eq!(read(open_input(&config, "inner").unwrap()), "inside");
}

#[cfg(unix)]
#[test]
fn links_are_walked_only_while_they_stay_beneath_the_root() {
    use std::os::unix::fs::symlink;
    let directory = tempfile::tempdir().unwrap();
    let config = config(directory.path());
    let workspace = &config.workspace;
    std::fs::create_dir_all(workspace.join("docs/deep")).unwrap();
    std::fs::write(workspace.join("docs/notes.txt"), "notes").unwrap();
    std::fs::write(workspace.join("private/catalog.json"), "{}").unwrap();
    // Relative targets, including `..` that stays inside.
    symlink("../notes.txt", workspace.join("docs/deep/up")).unwrap();
    symlink("docs", workspace.join("docs-link")).unwrap();
    // A chain through a directory link and a file link.
    symlink("docs-link/deep/up", workspace.join("chain")).unwrap();
    // Escapes and loops.
    symlink("../../outside", workspace.join("docs/escape")).unwrap();
    symlink("loop-b", workspace.join("loop-a")).unwrap();
    symlink("loop-a", workspace.join("loop-b")).unwrap();
    symlink("private/catalog.json", workspace.join("to-private")).unwrap();
    assert_eq!(read(open_input(&config, "docs/deep/up").unwrap()), "notes");
    assert_eq!(
        read(open_input(&config, "docs-link/notes.txt").unwrap()),
        "notes"
    );
    assert_eq!(read(open_input(&config, "chain").unwrap()), "notes");
    // The reported path is where the file really is, so drafts and
    // revisions of one file share a key however it was reached.
    assert_eq!(
        open_input(&config, "chain").unwrap().path,
        workspace.join("docs/notes.txt")
    );
    for refused in ["docs/escape", "loop-a", "to-private"] {
        assert!(open_input(&config, refused).is_err(), "{refused}");
    }
}

#[cfg(unix)]
#[test]
fn a_component_swapped_for_a_link_after_validation_cannot_redirect_the_open() {
    // The old check canonicalized, then reopened by path. A directory that
    // becomes a link is now refused at the component itself.
    let directory = tempfile::tempdir().unwrap();
    let config = config(directory.path());
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("notes.txt"), "secret").unwrap();
    std::fs::create_dir(config.workspace.join("docs")).unwrap();
    std::fs::write(config.workspace.join("docs/notes.txt"), "notes").unwrap();
    assert_eq!(
        read(open_input(&config, "docs/notes.txt").unwrap()),
        "notes"
    );
    std::fs::rename(
        config.workspace.join("docs"),
        config.workspace.join("moved"),
    )
    .unwrap();
    std::os::unix::fs::symlink(outside.path(), config.workspace.join("docs")).unwrap();
    assert!(open_input(&config, "docs/notes.txt").is_err());
}

/// A `REPARSE_DATA_BUFFER` as `FSCTL_GET_REPARSE_POINT` returns it, with
/// the print name first so the substitute name's offset is exercised.
fn reparse_buffer(tag: u32, flags: Option<u32>, substitute: &str) -> Vec<u8> {
    let print: Vec<u16> = "print".encode_utf16().collect();
    let substitute: Vec<u16> = substitute.encode_utf16().collect();
    let mut names = Vec::new();
    for unit in print.iter().chain(&substitute) {
        names.extend_from_slice(&unit.to_le_bytes());
    }
    let mut data = Vec::new();
    for value in [print.len() * 2, substitute.len() * 2, 0, print.len() * 2] {
        data.extend_from_slice(&(value as u16).to_le_bytes());
    }
    if let Some(flags) = flags {
        data.extend_from_slice(&flags.to_le_bytes());
    }
    data.extend_from_slice(&names);
    let mut buffer = Vec::new();
    buffer.extend_from_slice(&tag.to_le_bytes());
    buffer.extend_from_slice(&(data.len() as u16).to_le_bytes());
    buffer.extend_from_slice(&0_u16.to_le_bytes());
    buffer.extend_from_slice(&data);
    buffer
}

fn utf16(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

#[test]
fn reparse_targets_parse_symlinks_and_junctions_only() {
    use reparse::{MOUNT_POINT, SYMLINK, target};
    let relative = reparse_buffer(SYMLINK, Some(1), "..\\notes.txt");
    assert_eq!(target(&relative).unwrap(), utf16("..\\notes.txt"));
    let absolute = reparse_buffer(SYMLINK, Some(0), "\\??\\C:\\work\\notes.txt");
    assert_eq!(
        target(&absolute).unwrap(),
        utf16("\\\\?\\C:\\work\\notes.txt")
    );
    let junction = reparse_buffer(MOUNT_POINT, None, "\\??\\C:\\work\\docs\\");
    assert_eq!(target(&junction).unwrap(), utf16("\\\\?\\C:\\work\\docs\\"));
    // Volume mounts parse, and are refused later as outside every root.
    let volume = reparse_buffer(MOUNT_POINT, None, "\\??\\Volume{0}\\");
    assert_eq!(target(&volume).unwrap(), utf16("\\\\?\\Volume{0}\\"));
    for refused in [
        // Absolute targets must be NT `\??\` names.
        reparse_buffer(SYMLINK, Some(0), "C:\\work\\notes.txt"),
        reparse_buffer(MOUNT_POINT, None, "\\\\server\\share"),
        // Only symlinks and junctions are walked: another surrogate (a WSL
        // symlink) and a non-surrogate cloud placeholder are not.
        reparse_buffer(0xA000_001D, None, "\\??\\C:\\work"),
        reparse_buffer(0x9000_601A, None, "\\??\\C:\\work"),
        reparse_buffer(SYMLINK, Some(1), ""),
    ] {
        assert!(target(&refused).is_err());
    }
    for length in [0, 4, 8, 12, relative.len() - 1] {
        assert!(target(&relative[..length]).is_err(), "{length}");
    }
    let mut odd = relative.clone();
    odd[10] = 1;
    assert!(target(&odd).is_err());
}

#[test]
fn only_name_surrogates_are_links_and_unavailable_data_is_refused() {
    use reparse::{Kind, MOUNT_POINT, SYMLINK, classify};
    const DIRECTORY: u32 = 0x10;
    const REPARSE: u32 = 0x400;
    const OFFLINE: u32 = 0x1000;
    const RECALL_ON_DATA_ACCESS: u32 = 0x40_0000;
    const RECALL_ON_OPEN: u32 = 0x4_0000;
    const PINNED: u32 = 0x8_0000;
    const CLOUD: u32 = 0x9000_001A;
    const DEDUP: u32 = 0x8000_0013;
    assert_eq!(classify(REPARSE, SYMLINK).unwrap(), Kind::Link);
    assert_eq!(
        classify(REPARSE | DIRECTORY, MOUNT_POINT).unwrap(),
        Kind::Link
    );
    // A WSL symlink is a surrogate; reading its target refuses it later.
    assert_eq!(classify(REPARSE, 0xA000_001D).unwrap(), Kind::Link);
    // Hydrated OneDrive files and folders and dedup files hold their data.
    for (attributes, tag) in [
        (REPARSE | PINNED, CLOUD),
        (REPARSE | DIRECTORY | RECALL_ON_OPEN, CLOUD),
        (REPARSE, DEDUP),
        (0, 0),
        (DIRECTORY, 0),
        // A tag without the attribute is no reparse point.
        (0, SYMLINK),
    ] {
        assert_eq!(
            classify(attributes, tag).unwrap(),
            Kind::Plain,
            "{attributes:x} {tag:x}"
        );
    }
    // Online-only data would be downloaded or recalled by the read.
    for (attributes, tag) in [
        (REPARSE | RECALL_ON_DATA_ACCESS, CLOUD),
        (REPARSE | OFFLINE, 0x8000_0012),
        (OFFLINE, 0),
        (0x40, 0),
        (REPARSE | RECALL_ON_DATA_ACCESS, SYMLINK),
    ] {
        assert!(classify(attributes, tag).is_err(), "{attributes:x} {tag:x}");
    }
}

/// Puts a third-party, non-surrogate reparse tag on `path`, as a sync or
/// archive filter would, or returns false where this runner may not.
#[cfg(windows)]
fn third_party_tag(path: &Path, directory: bool) -> bool {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::{
        Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT},
        System::{IO::DeviceIoControl, Ioctl::FSCTL_SET_REPARSE_POINT},
    };
    let handle = std::fs::OpenOptions::new()
        .read(true)
        .write(!directory)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .unwrap();
    // REPARSE_GUID_DATA_BUFFER: tag, data length, reserved, GUID, data.
    let mut buffer = Vec::new();
    buffer.extend_from_slice(&0x0000_7A1Fu32.to_le_bytes());
    buffer.extend_from_slice(&4u16.to_le_bytes());
    buffer.extend_from_slice(&0u16.to_le_bytes());
    buffer.extend_from_slice(&[0x5A; 16]);
    buffer.extend_from_slice(b"xylo");
    let mut returned = 0u32;
    // SAFETY: a live handle and an input buffer valid for its length.
    let set = unsafe {
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
    };
    if set == 0 {
        eprintln!(
            "Could not set a reparse tag here: {}",
            std::io::Error::last_os_error()
        );
    }
    set != 0
}

/// Makes a directory junction; unlike symlinks it needs no privilege.
#[cfg(windows)]
fn junction(link: &Path, target: &Path) {
    let target = target.to_str().unwrap();
    let target = target.strip_prefix("\\\\?\\").unwrap_or(target);
    let link = link.to_str().unwrap();
    let link = link.strip_prefix("\\\\?\\").unwrap_or(link);
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J", link, target])
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "mklink /J {link} {target}");
}

/// Makes a file or directory symlink, or returns false where creating one
/// needs Developer Mode or a privilege this runner lacks.
#[cfg(windows)]
fn symlink(target: impl AsRef<Path>, link: &Path, directory: bool) -> bool {
    let made = if directory {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    };
    match made {
        Ok(()) => true,
        // ERROR_PRIVILEGE_NOT_HELD
        Err(error) if error.raw_os_error() == Some(1314) => {
            eprintln!("Symlink creation needs Developer Mode here; junction cases still run");
            false
        }
        Err(error) => panic!("Could not create symlink fixture: {error}"),
    }
}

#[cfg(windows)]
#[test]
fn windows_links_traversal_and_private_files_are_refused() {
    let directory = tempfile::tempdir().unwrap();
    let config = config(directory.path());
    let workspace = &config.workspace;
    let outside = tempfile::tempdir().unwrap();
    let outside_path = outside.path().canonicalize().unwrap();
    std::fs::write(outside_path.join("secret"), "secret").unwrap();
    std::fs::write(workspace.join("inside.txt"), "inside").unwrap();
    std::fs::write(workspace.join("private").join("catalog.json"), "{}").unwrap();
    std::fs::write(workspace.join("inside.txt:stream"), "stream").unwrap();
    junction(&workspace.join("dir-link"), &outside_path);
    junction(&workspace.join("private-link"), &workspace.join("private"));
    std::fs::hard_link(outside_path.join("secret"), workspace.join("hard")).unwrap();
    std::fs::create_dir(workspace.join("a-dir")).unwrap();
    let mut refused = vec![
        "dir-link/secret",
        "dir-link",
        "private-link/catalog.json",
        "hard",
        "../secret",
        "private/catalog.json",
        // Other spellings of the management directory reach its identity.
        "PRIVATE/catalog.json",
        "inside.txt:stream",
        "inside.txt.",
        "a-dir",
        "missing.txt",
    ];
    if symlink(
        outside_path.join("secret"),
        &workspace.join("file-link"),
        false,
    ) {
        refused.push("file-link");
        assert!(symlink(
            workspace.join("inside.txt"),
            &workspace.join("inner"),
            false
        ));
        assert_eq!(read(open_input(&config, "inner").unwrap()), "inside");
    }
    for requested in refused {
        assert!(open_input(&config, requested).is_err(), "{requested}");
    }
    let elsewhere = outside_path.join("secret");
    assert!(open_input(&config, &elsewhere.to_string_lossy()).is_err());
    assert_eq!(read(open_input(&config, "inside.txt").unwrap()), "inside");
    // An ordinary drive path names the same file as the verbatim root.
    let ordinary = workspace.join("inside.txt");
    let ordinary = ordinary.to_str().unwrap();
    let ordinary = ordinary.strip_prefix("\\\\?\\").unwrap_or(ordinary);
    assert_eq!(read(open_input(&config, ordinary).unwrap()), "inside");
}

#[cfg(windows)]
#[test]
fn windows_links_are_walked_only_while_they_stay_beneath_the_root() {
    let directory = tempfile::tempdir().unwrap();
    let config = config(directory.path());
    let workspace = &config.workspace;
    let docs = workspace.join("docs");
    let notes = docs.join("notes.txt");
    std::fs::create_dir_all(docs.join("deep")).unwrap();
    std::fs::write(&notes, "notes").unwrap();
    // Junctions always hold absolute targets, here inside the root, and a
    // junction to a junction.
    junction(&workspace.join("docs-junction"), &docs);
    junction(
        &workspace.join("chain-junction"),
        &workspace.join("docs-junction"),
    );
    for requested in ["docs-junction/notes.txt", "chain-junction/notes.txt"] {
        let input = open_input(&config, requested).unwrap();
        // The reported path is where the file really is.
        assert_eq!(input.path, notes, "{requested}");
        assert_eq!(read(input), "notes");
    }
    assert!(open_input(&config, "docs-junction/../docs/notes.txt").is_err());
    if !symlink("..\\notes.txt", &docs.join("deep").join("up"), false) {
        return;
    }
    // Relative targets, including `..` that stays inside.
    assert!(symlink("docs", &workspace.join("docs-link"), true));
    assert!(symlink(
        "docs-link\\deep\\up",
        &workspace.join("chain"),
        false
    ));
    // A directory symlink as the last component, and a file symlink where
    // a directory is expected, are read without a typed open succeeding.
    assert!(symlink(
        "docs\\notes.txt",
        &workspace.join("dir-typed"),
        true
    ));
    assert!(symlink("docs", &workspace.join("file-typed"), false));
    // Escapes and loops.
    assert!(symlink("..\\..\\outside", &docs.join("escape"), false));
    assert!(symlink("loop-b", &workspace.join("loop-a"), false));
    assert!(symlink("loop-a", &workspace.join("loop-b"), false));
    assert!(symlink(
        "private\\catalog.json",
        &workspace.join("to-private"),
        false
    ));
    assert!(symlink("\\rooted", &workspace.join("rooted"), false));
    std::fs::write(workspace.join("private").join("catalog.json"), "{}").unwrap();
    assert_eq!(read(open_input(&config, "docs/deep/up").unwrap()), "notes");
    assert_eq!(
        read(open_input(&config, "docs-link/notes.txt").unwrap()),
        "notes"
    );
    assert_eq!(open_input(&config, "chain").unwrap().path, notes);
    assert_eq!(read(open_input(&config, "dir-typed").unwrap()), "notes");
    assert_eq!(
        read(open_input(&config, "file-typed/notes.txt").unwrap()),
        "notes"
    );
    for refused in ["docs/escape", "loop-a", "to-private", "rooted"] {
        assert!(open_input(&config, refused).is_err(), "{refused}");
    }
}

#[cfg(windows)]
#[test]
fn windows_a_component_swapped_for_a_junction_cannot_redirect_the_open() {
    let directory = tempfile::tempdir().unwrap();
    let config = config(directory.path());
    let outside = tempfile::tempdir().unwrap();
    let outside_path = outside.path().canonicalize().unwrap();
    std::fs::write(outside_path.join("notes.txt"), "secret").unwrap();
    let docs = config.workspace.join("docs");
    std::fs::create_dir(&docs).unwrap();
    std::fs::write(docs.join("notes.txt"), "notes").unwrap();
    assert_eq!(
        read(open_input(&config, "docs/notes.txt").unwrap()),
        "notes"
    );
    std::fs::rename(&docs, config.workspace.join("moved")).unwrap();
    junction(&docs, &outside_path);
    assert!(open_input(&config, "docs/notes.txt").is_err());
}

#[cfg(windows)]
#[test]
fn windows_an_open_input_does_not_block_an_atomic_save() {
    let directory = tempfile::tempdir().unwrap();
    let config = config(directory.path());
    let docs = config.workspace.join("docs");
    std::fs::create_dir(&docs).unwrap();
    std::fs::write(docs.join("notes.txt"), "notes").unwrap();
    let input = open_input(&config, "docs/notes.txt").unwrap();
    // An editor's atomic save renames over the file a tool still reads.
    std::fs::write(docs.join("next.txt"), "next").unwrap();
    std::fs::rename(docs.join("next.txt"), docs.join("notes.txt")).unwrap();
    assert_eq!(read(input), "notes");
    assert_eq!(read(open_input(&config, "docs/notes.txt").unwrap()), "next");
}

#[cfg(windows)]
#[test]
fn windows_non_link_reparse_points_are_read_through_the_same_handle() {
    let directory = tempfile::tempdir().unwrap();
    let config = config(directory.path());
    let synced = config.workspace.join("synced");
    std::fs::create_dir(&synced).unwrap();
    let notes = synced.join("notes.txt");
    std::fs::write(&notes, "synced notes").unwrap();
    if !third_party_tag(&notes, false) || !third_party_tag(&synced, true) {
        return;
    }
    // Like a hydrated OneDrive file in a OneDrive folder: the data is under
    // the name, so neither reparse point is followed or refused.
    let input = open_input(&config, "synced/notes.txt").unwrap();
    assert_eq!(input.path, notes);
    assert_eq!(read(input), "synced notes");
}
