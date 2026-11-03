use super::*;

#[cfg(windows)]
#[test]
fn windows_read_metadata_needs_no_symlinks_and_cleanup_preserves_source() {
    let source = tempfile::tempdir().unwrap();
    let overlay = tempfile::tempdir().unwrap();
    std::fs::create_dir(source.path().join("objects")).unwrap();
    std::fs::write(source.path().join("objects/keep"), b"object data").unwrap();
    std::fs::write(source.path().join("index"), b"original index").unwrap();

    // No Developer Mode/privilege probe or skipped failure: these operations
    // must succeed under an ordinary Windows token.
    read_metadata_link(
        &source.path().join("objects"),
        &overlay.path().join("objects"),
    )
    .unwrap();
    read_metadata_link(&source.path().join("index"), &overlay.path().join("index")).unwrap();
    read_metadata_link(
        &source.path().join("absent"),
        &overlay.path().join("absent"),
    )
    .unwrap();
    assert!(!overlay.path().join("absent").exists());
    assert_eq!(
        std::fs::read(overlay.path().join("objects/keep")).unwrap(),
        b"object data"
    );
    assert!(
        !std::fs::symlink_metadata(overlay.path().join("index"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    // A file snapshot must not alias the repository index through a hard link.
    std::fs::write(overlay.path().join("index"), b"private snapshot").unwrap();
    assert_eq!(
        std::fs::read(source.path().join("index")).unwrap(),
        b"original index"
    );
    overlay.close().unwrap();
    assert_eq!(
        std::fs::read(source.path().join("objects/keep")).unwrap(),
        b"object data"
    );
}

#[test]
fn porcelain_keeps_staged_unstaged_rename_and_untracked() {
    let records =
        parse_status("MM src/a.rs\0R  new name.rs\0old name.rs\0?? new.txt\0 D gone.rs\0").unwrap();
    assert_eq!(records.len(), 4);
    assert!(records[0].in_group(ChangeGroup::Staged));
    assert!(records[0].in_group(ChangeGroup::Unstaged));
    assert_eq!(records[1].original, Some(PathBuf::from("old name.rs")));
    assert!(records[2].in_group(ChangeGroup::Untracked));
    assert_eq!(records[3].worktree, 'D');
}
#[test]
fn paths_reject_escape_and_metadata() {
    for path in ["../escape", "/absolute", ".git/config", "src/../../escape"] {
        assert!(validate_path(Path::new(path)).is_err());
    }
    assert!(validate_path(Path::new("src/safe name.rs")).is_ok());
}
