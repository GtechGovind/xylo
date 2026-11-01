use super::{ChangeGroup, GitChange, GitPanel, GitStatus, export_file_stem};
use std::path::PathBuf;
#[test]
fn changed_files_keep_both_versions_reachable_without_duplicate_rows() {
    let change = GitChange {
        path: "src/nested/app.rs".into(),
        original: None,
        index: 'M',
        worktree: 'M',
    };
    assert_eq!(
        super::preferred_change_group(&change),
        ChangeGroup::Unstaged
    );
    assert!(change.in_group(ChangeGroup::Staged));
    let panel = GitPanel::prepare(
        "/project".into(),
        GitStatus {
            branch: "main".into(),
            truncated: false,
            changes: vec![change],
        },
    );
    assert_eq!(panel.status.unwrap().changes.len(), 1);
    assert_eq!(
        panel.statuses[&PathBuf::from("/project/src/nested/app.rs")],
        'M'
    );
}
#[test]
fn exported_chat_file_names_drop_characters_file_systems_reject() {
    assert_eq!(
        export_file_stem("# Fix: src/main.rs? \"now\"\n\n## You"),
        "Fix- src-main.rs- -now-"
    );
    assert_eq!(export_file_stem("# ...\n"), "Conversation");
    assert_eq!(export_file_stem("no title"), "Conversation");
    assert_eq!(
        export_file_stem(&format!("# {}", "a".repeat(200))).len(),
        80
    );
}
#[test]
fn a_status_refresh_keeps_the_open_diff_and_one_still_loading() {
    let root = PathBuf::from("/project");
    let status = || GitStatus {
        branch: "main".into(),
        truncated: false,
        changes: vec![GitChange {
            path: "a.rs".into(),
            original: None,
            index: ' ',
            worktree: 'M',
        }],
    };
    let mut current = GitPanel::prepare(root.clone(), status());
    let pending = uuid::Uuid::new_v4();
    current.request = Some(uuid::Uuid::new_v4());
    current.selected = Some(("a.rs".into(), ChangeGroup::Unstaged));
    current.diff_request = Some(pending);
    current.diff_lines = vec!["+new".into()];
    current.take_status(GitPanel::prepare(root, status()));
    assert!(current.request.is_none());
    assert_eq!(current.diff_request, Some(pending));
    assert_eq!(current.diff_lines, ["+new"]);
    assert!(current.selected.is_some());
    assert_eq!(current.status.as_ref().unwrap().changes.len(), 1);
}

#[test]
fn visible_diff_rows_preserve_source_indices_and_hide_only_metadata() {
    let lines = "diff --git a/a b/a\nindex a..b 100644\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n---old\n+++new\n\\ No newline at end of file\ndiff --git a/b b/b\nBinary files a/b and b/b differ".lines().map(str::to_owned).collect::<Vec<_>>();
    assert_eq!(super::visible_diff_indices(&lines), vec![4, 5, 6, 7, 9]);
}
