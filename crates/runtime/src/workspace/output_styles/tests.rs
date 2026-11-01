use super::*;

#[test]
fn discovers_both_scopes_and_keeps_builtin_default_empty() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    for (root, body) in [
        (workspace.path(), "# Focus\nProject instructions."),
        (home.path(), "# Focus\nUser instructions."),
    ] {
        std::fs::create_dir_all(root.join(".xylo/output-styles")).unwrap();
        std::fs::write(root.join(".xylo/output-styles/focus.md"), body).unwrap();
    }
    let styles = discover(workspace.path(), home.path()).unwrap();
    assert_eq!(styles.len(), 5);
    assert_eq!(
        selected(workspace.path(), home.path(), "default")
            .unwrap()
            .prompt,
        ""
    );
    assert!(
        selected(workspace.path(), home.path(), "project:focus")
            .unwrap()
            .prompt
            .contains("Project")
    );
    assert!(
        selected(workspace.path(), home.path(), "user:focus")
            .unwrap()
            .prompt
            .contains("User")
    );
    assert!(selected(workspace.path(), home.path(), "project:../outside").is_err());
}

#[test]
fn oversized_or_non_markdown_styles_are_not_loaded() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let directory = workspace.path().join(".xylo/output-styles");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("large.md"), "x".repeat(20_000)).unwrap();
    std::fs::write(directory.join("ignore.txt"), "ignored").unwrap();
    assert_eq!(discover(workspace.path(), home.path()).unwrap().len(), 3);
    assert!(selected(workspace.path(), home.path(), "project:large").is_err());
}

#[cfg(unix)]
#[test]
fn project_style_cannot_read_an_outside_symlink() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(workspace.path().join(".xylo/output-styles")).unwrap();
    std::fs::write(outside.path().join("private.md"), "outside content").unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("private.md"),
        workspace.path().join(".xylo/output-styles/escape.md"),
    )
    .unwrap();
    assert!(selected(workspace.path(), home.path(), "project:escape").is_err());
    assert_eq!(discover(workspace.path(), home.path()).unwrap().len(), 3);
}
