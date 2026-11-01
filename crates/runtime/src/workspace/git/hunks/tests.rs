use super::*;
fn git(root: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
fn fixture() -> (tempfile::TempDir, String, String) {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "--quiet"]);
    let original = (0..30)
        .map(|line| format!("line {line}\n"))
        .collect::<String>();
    std::fs::write(root.path().join("file.txt"), &original).unwrap();
    git(root.path(), &["add", "."]);
    git(
        root.path(),
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "base",
        ],
    );
    let edited = original
        .replace("line 2\n", "first change\n")
        .replace("line 25\n", "second change\n");
    std::fs::write(root.path().join("file.txt"), &edited).unwrap();
    (root, original, edited)
}
async fn request(root: &Path, group: ChangeGroup, operation: Operation) -> Request {
    let diff = super::super::diff(root, Path::new("file.txt"), None, group)
        .await
        .unwrap();
    assert_eq!(parse(&diff).unwrap().len(), 2);
    Request {
        path: "file.txt".into(),
        original: None,
        group,
        hunk: 0,
        digest: digest(&diff),
        operation,
    }
}
#[tokio::test]
async fn reverting_one_hunk_retains_the_other_working_change() {
    let (root, _, edited) = fixture();
    let request = request(root.path(), ChangeGroup::Unstaged, Operation::Revert).await;
    apply(root.path(), request).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(root.path().join("file.txt")).unwrap(),
        edited.replace("first change\n", "line 2\n")
    );
    assert!(git(root.path(), &["diff", "--cached"]).is_empty());
}
#[tokio::test]
async fn staging_and_unstaging_one_hunk_leave_worktree_content_unchanged() {
    let (root, _, edited) = fixture();
    let request = request(root.path(), ChangeGroup::Unstaged, Operation::Stage).await;
    apply(root.path(), request).await.unwrap();
    let staged = git(root.path(), &["diff", "--cached"]);
    assert!(staged.contains("+first change") && !staged.contains("+second change"));
    assert_eq!(
        std::fs::read_to_string(root.path().join("file.txt")).unwrap(),
        edited
    );
    let diff = super::super::diff(
        root.path(),
        Path::new("file.txt"),
        None,
        ChangeGroup::Staged,
    )
    .await
    .unwrap();
    apply(
        root.path(),
        Request {
            path: "file.txt".into(),
            original: None,
            group: ChangeGroup::Staged,
            hunk: 0,
            digest: digest(&diff),
            operation: Operation::Unstage,
        },
    )
    .await
    .unwrap();
    assert!(git(root.path(), &["diff", "--cached"]).is_empty());
    assert_eq!(
        std::fs::read_to_string(root.path().join("file.txt")).unwrap(),
        edited
    );
}
#[tokio::test]
async fn a_stale_preview_and_an_outside_path_are_refused() {
    let (root, _, edited) = fixture();
    let mut request = request(root.path(), ChangeGroup::Unstaged, Operation::Revert).await;
    std::fs::write(
        root.path().join("file.txt"),
        format!("{edited}later edit\n"),
    )
    .unwrap();
    assert!(apply(root.path(), request.clone()).await.is_err());
    request.path = "../outside".into();
    assert!(apply(root.path(), request).await.is_err());
    assert_eq!(
        std::fs::read_to_string(root.path().join("file.txt")).unwrap(),
        format!("{edited}later edit\n")
    );
}

#[tokio::test]
async fn hunk_stage_in_a_selected_subdirectory_updates_the_real_index() {
    let (root, original, edited) = fixture();
    git(root.path(), &["checkout", "--", "file.txt"]);
    std::fs::create_dir(root.path().join("nested")).unwrap();
    std::fs::write(root.path().join("nested/file.txt"), original).unwrap();
    git(root.path(), &["add", "nested"]);
    git(
        root.path(),
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "nested",
        ],
    );
    std::fs::write(root.path().join("nested/file.txt"), edited).unwrap();
    let selected = root.path().join("nested");
    let request = request(&selected, ChangeGroup::Unstaged, Operation::Stage).await;
    apply(&selected, request).await.unwrap();
    let staged = git(root.path(), &["diff", "--cached"]);
    assert!(
        staged.contains("nested/file.txt")
            && staged.contains("+first change")
            && !staged.contains("+second change")
    );
}
#[cfg(unix)]
#[tokio::test]
async fn native_hunk_actions_never_run_repository_filters_or_fsmonitor() {
    let (root, _, _) = fixture();
    let marker = root.path().join("executed");
    let executable = root.path().join("malicious.sh");
    std::fs::write(
        &executable,
        format!("#!/bin/sh\nprintf marker > '{}'\ncat\n", marker.display()),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    git(
        root.path(),
        &["config", "core.fsmonitor", executable.to_str().unwrap()],
    );
    git(
        root.path(),
        &[
            "config",
            "filter.fixture.clean",
            executable.to_str().unwrap(),
        ],
    );
    std::fs::write(root.path().join(".gitattributes"), "*.txt filter=fixture\n").unwrap();
    let request = request(root.path(), ChangeGroup::Unstaged, Operation::Stage).await;
    apply(root.path(), request).await.unwrap();
    assert!(!marker.exists());
}
