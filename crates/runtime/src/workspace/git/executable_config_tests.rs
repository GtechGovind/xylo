use super::*;
use std::os::unix::fs::PermissionsExt;
fn fixture_git(root: &Path, args: &[&str]) {
    let result = std::process::Command::new(trusted_git(root).unwrap())
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
#[tokio::test]
async fn native_views_never_run_clean_or_process_filters_from_any_attribute_source() {
    for process in [false, true] {
        for info in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            fixture_git(root, &["init", "--quiet"]);
            std::fs::write(root.join("tracked.txt"), "old\n").unwrap();
            fixture_git(root, &["add", "tracked.txt"]);
            fixture_git(
                root,
                &[
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "commit",
                    "-qm",
                    "base",
                ],
            );
            let marker = root.join("filter-executed");
            let script = root.join("filter.sh");
            std::fs::write(
                &script,
                format!(
                    "#!/bin/sh\nprintf invoked > '{}'\n{}\n",
                    marker.display(),
                    if process { "exit 1" } else { "cat" }
                ),
            )
            .unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            fixture_git(
                root,
                &[
                    "config",
                    if process {
                        "filter.fixture.process"
                    } else {
                        "filter.fixture.clean"
                    },
                    script.to_str().unwrap(),
                ],
            );
            std::fs::write(
                root.join(if info {
                    ".git/info/attributes"
                } else {
                    ".gitattributes"
                }),
                "tracked.txt filter=fixture\n",
            )
            .unwrap();
            std::fs::write(root.join("tracked.txt"), "new\n").unwrap();
            status(root).await.unwrap();
            let diff = diff(root, Path::new("tracked.txt"), None, ChangeGroup::Unstaged)
                .await
                .unwrap();
            assert!(diff.contains("+new"));
            assert!(
                !marker.exists(),
                "Native Git executed process={process}, info-attributes={info}"
            );
        }
    }
}
