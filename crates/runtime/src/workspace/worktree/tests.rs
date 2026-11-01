//! Managed worktree lifecycle regression tests.
use super::*;
use std::process::Command;

#[test]
fn worktree_inspection_lease_child() {
    let Some(workspace) = std::env::var_os("XYLO_TEST_INSPECTION_WORKSPACE") else {
        return;
    };
    let _lease = retain_inspection_workspace(Path::new(&workspace))
        .unwrap()
        .unwrap();
    use std::io::Write;
    println!("inspection lease ready");
    std::io::stdout().flush().unwrap();
    let mut release = String::new();
    std::io::stdin().read_line(&mut release).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn worktree_inspection_lease_blocks_removal_across_processes_aliases_and_owner_exit() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let repo = repo();
    let tree = create(repo.path(), "inspection", "HEAD").await.unwrap();
    std::fs::create_dir(tree.path.join("nested")).unwrap();
    let alias = tempfile::tempdir().unwrap();
    let workspace = alias.path().join("alias");
    std::os::unix::fs::symlink(tree.path.join("nested"), &workspace).unwrap();
    let mut owner = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "workspace::worktree::tests::worktree_inspection_lease_child",
            "--nocapture",
        ])
        .env("XYLO_TEST_INSPECTION_WORKSPACE", &workspace)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(owner.stdout.take().unwrap()).lines();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(line) = lines.next_line().await.unwrap() {
            if line.contains("inspection lease ready") {
                return;
            }
        }
        panic!("Inspection owner exited before acquiring its lease");
    })
    .await
    .unwrap();
    for force in [false, true] {
        let error = remove(&tree.path, force).await.unwrap_err();
        assert!(error.is::<InspectionInUse>(), "{error:#}");
        assert!(tree.path.exists());
    }
    owner
        .stdin
        .take()
        .unwrap()
        .write_all(b"release\n")
        .await
        .unwrap();
    assert!(owner.wait().await.unwrap().success());
    let error = remove(&tree.path, false).await.unwrap_err();
    assert!(
        error.is::<InspectionInUse>(),
        "Host exit is not process-exit proof"
    );
    assert!(tree.path.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn worktree_inspection_lease_rejects_symlink_metadata() {
    let repo = repo();
    let tree = create(repo.path(), "inspection-link", "HEAD")
        .await
        .unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    let locks = tree.repository.join(".git/xylo-worktree-inspections");
    std::fs::create_dir(&locks).unwrap();
    std::os::unix::fs::symlink(outside.path(), locks.join("inspection-link")).unwrap();
    assert!(retain_inspection_workspace(&tree.path).is_err());
    assert!(remove(&tree.path, true).await.is_err());
    assert!(tree.path.exists());
    std::fs::remove_file(locks.join("inspection-link")).unwrap();
    std::fs::hard_link(outside.path(), locks.join("inspection-link")).unwrap();
    assert!(retain_inspection_workspace(&tree.path).is_err());
    assert_eq!(std::fs::metadata(outside.path()).unwrap().len(), 0);
}

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

#[cfg(unix)]
#[test]
fn completed_operation_releases_lock_even_with_an_inherited_descriptor() {
    let repo = repo();
    let operation = lock(repo.path()).unwrap();
    // dup and fork share the same open-file description and flock ownership.
    let inherited = operation.0.try_clone().unwrap();
    assert!(lock(repo.path()).is_err());
    drop(operation);
    let next = lock(repo.path()).expect("Completed operation must release its lock");
    assert!(lock(repo.path()).is_err());
    drop(next);
    drop(inherited);
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
