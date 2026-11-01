//! Managed worktree lifecycle regression tests.
use super::*;
use std::process::Command;

fn git(root: &Path, args: &[&str]) {
    let result = Command::new("/usr/bin/git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-b", "main"]);
    git(
        dir.path(),
        &["config", "user.email", "test@example.invalid"],
    );
    git(dir.path(), &["config", "user.name", "Test"]);
    std::fs::write(dir.path().join("file.txt"), "base\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-m", "base"]);
    dir
}
#[tokio::test]
async fn lifecycle_preserves_main_and_refuses_dirty_or_unpushed() {
    let repo = repo();
    let tree = create(repo.path(), "test", "HEAD").await.unwrap();
    assert_eq!(tree.branch, "xylo/test");
    assert_eq!(list(repo.path()).await.unwrap().len(), 1);
    assert_eq!(status(&tree.path).await.unwrap().ahead, 0);
    std::fs::write(tree.path.join("file.txt"), "changed\n").unwrap();
    assert!(remove(&tree.path, false).await.is_err());
    assert_eq!(
        std::fs::read_to_string(repo.path().join("file.txt")).unwrap(),
        "base\n"
    );
    git(&tree.path, &["add", "."]);
    git(&tree.path, &["commit", "-m", "work"]);
    assert_eq!(status(&tree.path).await.unwrap().ahead, 1);
    assert!(remove(&tree.path, false).await.is_err());
    remove(&tree.path, true).await.unwrap();
    assert!(!tree.path.exists());
    assert!(list(repo.path()).await.unwrap().is_empty());
}
#[tokio::test]
async fn clean_exit_and_collision_are_safe() {
    let repo = repo();
    for name in ["../outside", "-option", "a/b", ".", "foo.lock", "a b"] {
        assert!(create(repo.path(), name, "HEAD").await.is_err(), "{name}");
    }
    assert!(create(repo.path(), "valid", "--help").await.is_err());
    let tree = create(repo.path(), "valid", "HEAD").await.unwrap();
    assert!(create(repo.path(), "valid", "HEAD").await.is_err());
    assert!(remove(repo.path(), true).await.is_err());
    remove(&tree.path, false).await.unwrap();
    assert!(!tree.path.exists());
}
#[cfg(unix)]
#[tokio::test]
async fn executable_config_and_symlink_roots_are_rejected_or_neutralized() {
    let repo = repo();
    let marker = repo.path().join("EXECUTED");
    let command = format!("touch {}; cat", marker.display());
    std::fs::write(repo.path().join(".gitattributes"), "*.txt filter=evil\n").unwrap();
    git(
        repo.path(),
        &["-c", "core.fsmonitor=false", "add", ".gitattributes"],
    );
    git(
        repo.path(),
        &["-c", "core.fsmonitor=false", "commit", "-m", "attributes"],
    );
    git(repo.path(), &["config", "filter.evil.smudge", &command]);
    git(repo.path(), &["config", "filter.evil.clean", &command]);
    git(repo.path(), &["config", "core.fsmonitor", &command]);
    assert!(
        !marker.exists(),
        "Fixture must not execute configured commands"
    );
    let tree = create(repo.path(), "safe", "HEAD").await.unwrap();
    status(&tree.path).await.unwrap();
    remove(&tree.path, false).await.unwrap();
    assert!(!marker.exists());
    let outside = tempfile::tempdir().unwrap();
    std::fs::remove_dir(repo.path().join(".xylo/worktrees")).unwrap();
    std::os::unix::fs::symlink(outside.path(), repo.path().join(".xylo/worktrees")).unwrap();
    assert!(create(repo.path(), "escape", "HEAD").await.is_err());
}

#[tokio::test]
async fn sync_reports_conflicts_and_ignored_files_block_cleanup() {
    let repo = repo();
    let tree = create(repo.path(), "merge", "HEAD").await.unwrap();
    std::fs::write(tree.path.join("file.txt"), "worktree\n").unwrap();
    git(&tree.path, &["add", "file.txt"]);
    git(&tree.path, &["commit", "-m", "worktree"]);
    std::fs::write(repo.path().join("file.txt"), "main\n").unwrap();
    git(repo.path(), &["add", "file.txt"]);
    git(repo.path(), &["commit", "-m", "main"]);
    let result = sync_with_base(&tree.path, "main").await.unwrap();
    assert!(!result.merged);
    assert_eq!(result.conflicts, vec!["file.txt"]);
    assert!(remove(&tree.path, false).await.is_err());
    let clean = create(repo.path(), "ignored", "main").await.unwrap();
    std::fs::write(clean.path.join(".gitignore"), "cache/\n").unwrap();
    git(&clean.path, &["add", ".gitignore"]);
    git(&clean.path, &["commit", "-m", "ignore"]);
    std::fs::create_dir(clean.path.join("cache")).unwrap();
    std::fs::write(clean.path.join("cache/private.txt"), "retain me").unwrap();
    assert!(!status(&clean.path).await.unwrap().changed_files.is_empty());
    assert!(remove(&clean.path, false).await.is_err());
}
