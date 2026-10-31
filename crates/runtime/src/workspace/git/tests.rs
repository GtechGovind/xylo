use super::*;
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
