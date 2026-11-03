use super::*;

fn git(root: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Git fixture: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn real_repository_status_and_diffs_preserve_each_change_group() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    git(root, &["init", "--quiet"]);
    for (name, text) in [
        ("changed.rs", "baseline\n"),
        ("removed.rs", "remove me\n"),
        ("old name.rs", "rename me\n"),
    ] {
        std::fs::write(root.join(name), text).unwrap();
    }
    git(root, &["add", "."]);
    git(
        root,
        &[
            "-c",
            "user.name=Native fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ],
    );
    std::fs::write(root.join("changed.rs"), "staged\n").unwrap();
    git(root, &["add", "changed.rs"]);
    std::fs::write(root.join("changed.rs"), "working\n").unwrap();
    std::fs::remove_file(root.join("removed.rs")).unwrap();
    git(root, &["mv", "old name.rs", "new name.rs"]);
    std::fs::write(root.join("[literal].txt"), "untracked content\n").unwrap();
    std::fs::write(root.join("binary.bin"), [0, 1, 2, 3]).unwrap();
    let state = status(root).await.unwrap();
    let changed = state
        .changes
        .iter()
        .find(|change| change.path == Path::new("changed.rs"))
        .unwrap();
    assert!(changed.in_group(ChangeGroup::Staged) && changed.in_group(ChangeGroup::Unstaged));
    let renamed = state
        .changes
        .iter()
        .find(|change| change.path == Path::new("new name.rs"))
        .unwrap();
    assert_eq!(renamed.original.as_deref(), Some(Path::new("old name.rs")));
    assert!(
        state
            .changes
            .iter()
            .any(|change| change.path == Path::new("removed.rs") && change.worktree == 'D')
    );
    assert!(
        state
            .changes
            .iter()
            .any(|change| change.path == Path::new("[literal].txt")
                && change.in_group(ChangeGroup::Untracked))
    );
    let staged = diff(root, Path::new("changed.rs"), None, ChangeGroup::Staged)
        .await
        .unwrap();
    assert!(staged.contains("+staged") && !staged.contains("+working"));
    let working = diff(root, Path::new("changed.rs"), None, ChangeGroup::Unstaged)
        .await
        .unwrap();
    assert!(working.contains("-staged") && working.contains("+working"));
    let untracked = diff(
        root,
        Path::new("[literal].txt"),
        None,
        ChangeGroup::Untracked,
    )
    .await
    .unwrap();
    assert!(untracked.contains("+untracked content"));
    let rename = diff(
        root,
        &renamed.path,
        renamed.original.as_deref(),
        ChangeGroup::Staged,
    )
    .await
    .unwrap();
    assert!(rename.contains("rename from old name.rs") && rename.contains("rename to new name.rs"));
    let removed = diff(root, Path::new("removed.rs"), None, ChangeGroup::Unstaged)
        .await
        .unwrap();
    assert!(removed.contains("-remove me"));
    let binary = diff(root, Path::new("binary.bin"), None, ChangeGroup::Untracked)
        .await
        .unwrap();
    assert!(binary.contains("Binary files"));
}

#[tokio::test]
async fn nested_project_scopes_paths_diffs_and_renames() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let selected = root.join("project");
    std::fs::create_dir(&selected).unwrap();
    git(root, &["init", "--quiet"]);
    for (name, text) in [
        ("outside.txt", "outside baseline\n"),
        ("incoming.txt", "incoming baseline\n"),
        ("project/changed.rs", "inside baseline\n"),
        ("project/old.rs", "rename content\n"),
        ("project/outgoing.rs", "outgoing baseline\n"),
    ] {
        std::fs::write(root.join(name), text).unwrap();
    }
    git(root, &["add", "."]);
    git(
        root,
        &[
            "-c",
            "user.name=Native fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ],
    );
    std::fs::write(root.join("outside.txt"), "private sibling change\n").unwrap();
    std::fs::write(selected.join("changed.rs"), "inside update\n").unwrap();
    git(root, &["mv", "project/old.rs", "project/new.rs"]);
    git(root, &["mv", "incoming.txt", "project/incoming.txt"]);
    git(root, &["mv", "project/outgoing.rs", "outgoing.rs"]);
    let state = status(&selected).await.unwrap();
    assert!(
        state
            .changes
            .iter()
            .all(|change| !change.path.starts_with("project")
                && change.path != Path::new("outside.txt")),
        "{state:?}"
    );
    let changed = state
        .changes
        .iter()
        .find(|change| change.path == Path::new("changed.rs"))
        .expect("selected-root relative change");
    let patch = diff(
        &selected,
        &changed.path,
        changed.original.as_deref(),
        ChangeGroup::Unstaged,
    )
    .await
    .unwrap();
    assert!(patch.contains("+inside update") && !patch.contains("private sibling change"));
    let renamed = state
        .changes
        .iter()
        .find(|change| change.path == Path::new("new.rs"))
        .unwrap();
    assert_eq!(renamed.original.as_deref(), Some(Path::new("old.rs")));
    let patch = diff(
        &selected,
        &renamed.path,
        renamed.original.as_deref(),
        ChangeGroup::Staged,
    )
    .await
    .unwrap();
    assert!(
        patch.contains("rename from old.rs") && patch.contains("rename to new.rs"),
        "{patch}"
    );
    let incoming = state
        .changes
        .iter()
        .find(|change| change.path == Path::new("incoming.txt"))
        .unwrap();
    assert!(incoming.original.is_none());
    let patch = diff(
        &selected,
        &incoming.path,
        incoming.original.as_deref(),
        ChangeGroup::Staged,
    )
    .await
    .unwrap();
    assert!(
        patch.contains("+incoming baseline") && !patch.contains("rename from"),
        "{patch}"
    );
    let outgoing = state
        .changes
        .iter()
        .find(|change| change.path == Path::new("outgoing.rs"))
        .unwrap();
    assert!(outgoing.original.is_none());
    let patch = diff(&selected, &outgoing.path, None, ChangeGroup::Staged)
        .await
        .unwrap();
    assert!(patch.contains("-outgoing baseline"), "{patch}");
}

#[cfg(windows)]
#[tokio::test]
async fn windows_native_reads_preserve_neutral_config_and_repository_metadata() {
    use std::io::Write;

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    git(root, &["init", "--quiet"]);
    std::fs::write(root.join("tracked.txt"), "staged baseline\n").unwrap();
    git(root, &["add", "tracked.txt"]);
    std::fs::write(root.join("tracked.txt"), "working change\n").unwrap();
    // If repository config/includes were loaded this malformed include would
    // make every Git command fail. Keep executable config out of the overlay.
    std::fs::write(root.join(".git/forbidden-config"), "[invalid\n").unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(root.join(".git/config"))
        .unwrap()
        .write_all(b"\n[include]\npath = forbidden-config\n[core]\nfsmonitor = forbidden-hook\n")
        .unwrap();
    let index = std::fs::read(root.join(".git/index")).unwrap();
    let config = std::fs::read(root.join(".git/config")).unwrap();
    let head = std::fs::read(root.join(".git/HEAD")).unwrap();

    // This is the same entry point as the engine's git_status/git_diff tools.
    for args in [
        vec!["status".into(), "--short".into(), "--branch".into()],
        vec![
            "diff".into(),
            "--no-ext-diff".into(),
            "--no-textconv".into(),
        ],
    ] {
        let result = run_read(root, &args, Duration::from_secs(30), Default::default())
            .await
            .unwrap();
        assert_eq!(result.code, Some(0), "{}", result.stderr);
        assert!(result.stdout.contains("tracked.txt"), "{}", result.stdout);
    }
    // Also exercise the native UI's shared status path.
    let state = status(root).await.unwrap();
    assert!(
        state
            .changes
            .iter()
            .any(|change| change.path == Path::new("tracked.txt"))
    );
    assert_eq!(std::fs::read(root.join(".git/index")).unwrap(), index);
    assert_eq!(std::fs::read(root.join(".git/config")).unwrap(), config);
    assert_eq!(std::fs::read(root.join(".git/HEAD")).unwrap(), head);
    assert!(root.join(".git/objects").is_dir());
    assert!(root.join(".git/refs").is_dir());
}

#[cfg(unix)]
#[tokio::test]
async fn untracked_symlink_diff_cannot_read_outside_project_or_git_metadata() {
    use std::os::unix::fs::symlink;
    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), "outside").unwrap();
    let root = directory.path();
    git(root, &["init", "--quiet"]);
    symlink(outside.path().join("secret"), root.join("outside")).unwrap();
    symlink(root.join(".git/config"), root.join("metadata")).unwrap();
    for name in ["outside", "metadata"] {
        assert!(
            diff(root, Path::new(name), None, ChangeGroup::Untracked)
                .await
                .is_err()
        );
    }
}
