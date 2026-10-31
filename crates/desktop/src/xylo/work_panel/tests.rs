use super::{ChangeGroup, GitChange, GitPanel, GitRow, GitStatus, export_file_stem, group_id};
use std::{collections::BTreeSet, path::PathBuf};
#[test]
fn changes_share_folder_structure_and_staged_unstaged_overlap() {
    let root = PathBuf::from("/project");
    let status = GitStatus {
        branch: "main".into(),
        truncated: false,
        changes: vec![
            GitChange {
                path: "src/nested/app.rs".into(),
                original: None,
                index: 'M',
                worktree: 'M',
            },
            GitChange {
                path: "new.txt".into(),
                original: None,
                index: '?',
                worktree: '?',
            },
        ],
    };
    let mut panel = GitPanel::prepare(root.clone(), status, BTreeSet::new());
    assert_eq!(panel.trees.len(), 3);
    assert_eq!(panel.statuses[&root.join("src/nested/app.rs")], 'M');
    assert_eq!(panel.rows.iter().filter(|row| matches!(row, GitRow::File(_,change,2) if change.path == std::path::Path::new("src/nested/app.rs"))).count(),2);
    panel
        .collapsed
        .insert((group_id(ChangeGroup::Staged), "src".into()));
    panel.rebuild();
    assert!(
        !panel
            .rows
            .iter()
            .any(|row| matches!(row, GitRow::File(ChangeGroup::Staged, _, _)))
    );
    assert!(
        panel
            .rows
            .iter()
            .any(|row| matches!(row, GitRow::File(ChangeGroup::Unstaged, _, _)))
    );
    assert_eq!(panel.trees.len(), 3, "collapse preserves prepared data");
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
    let mut current = GitPanel::prepare(root.clone(), status(), BTreeSet::new());
    let pending = uuid::Uuid::new_v4();
    current.request = Some(uuid::Uuid::new_v4());
    current.selected = Some(("a.rs".into(), ChangeGroup::Unstaged));
    current.diff_request = Some(pending);
    current.diff_lines = vec!["+new".into()];
    current.take_tree(GitPanel::prepare(root, status(), BTreeSet::new()));
    assert!(current.request.is_none());
    assert_eq!(current.diff_request, Some(pending));
    assert_eq!(current.diff_lines, ["+new"]);
    assert!(current.selected.is_some());
    assert_eq!(current.rows.len(), 2);
}
