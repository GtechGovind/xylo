//! Read-only Git inspection for the native workspace; never runs on the UI thread.
use anyhow::{Result, ensure};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeGroup {
    Staged,
    Unstaged,
    Untracked,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitChange {
    pub path: PathBuf,
    pub original: Option<PathBuf>,
    pub index: char,
    pub worktree: char,
}
#[derive(Clone, Debug)]
pub struct GitStatus {
    pub branch: String,
    pub changes: Vec<GitChange>,
    pub truncated: bool,
}
impl GitChange {
    pub fn in_group(&self, group: ChangeGroup) -> bool {
        match group {
            ChangeGroup::Staged => self.index != ' ' && self.index != '?',
            ChangeGroup::Unstaged => self.worktree != ' ' && self.worktree != '?',
            ChangeGroup::Untracked => self.index == '?',
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn porcelain_keeps_staged_unstaged_rename_and_untracked() {
        let records =
            parse_status("MM src/a.rs\0R  new name.rs\0old name.rs\0?? new.txt\0 D gone.rs\0")
                .unwrap();
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
}

fn validate_path(path: &Path) -> Result<()> {
    use std::path::Component;
    ensure!(
        !path.as_os_str().is_empty()
            && path
                .components()
                .all(|part| matches!(part, Component::Normal(name) if name != ".git")),
        "Choose a file inside the project"
    );
    Ok(())
}
fn parse_status(raw: &str) -> Result<Vec<GitChange>> {
    ensure!(
        !raw.contains('\u{fffd}'),
        "Git returned a filename that cannot be represented safely as UTF-8"
    );
    let mut fields = raw.split_terminator('\0');
    let mut changes = Vec::new();
    while let Some(record) = fields.next() {
        ensure!(
            record.len() >= 4 && record.as_bytes()[2] == b' ',
            "Invalid Git status record"
        );
        let index = record.as_bytes()[0] as char;
        let worktree = record.as_bytes()[1] as char;
        let path = PathBuf::from(&record[3..]);
        validate_path(&path)?;
        let original = if matches!(index, 'R' | 'C') || matches!(worktree, 'R' | 'C') {
            let original = PathBuf::from(
                fields
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("Incomplete Git rename"))?,
            );
            validate_path(&original)?;
            Some(original)
        } else {
            None
        };
        changes.push(GitChange {
            path,
            original,
            index,
            worktree,
        });
    }
    Ok(changes)
}
async fn run(root: &Path, args: &[&str]) -> Result<crate::process::ProcessResult> {
    let mut arguments = vec![
        "--literal-pathspecs".to_owned(),
        "-c".to_owned(),
        "core.quotePath=false".to_owned(),
    ];
    arguments.extend(args.iter().map(|arg| (*arg).to_owned()));
    crate::process::run(
        "git",
        &arguments,
        root,
        Duration::from_secs(15),
        Default::default(),
    )
    .await
}
pub async fn status(root: &Path) -> Result<GitStatus> {
    let output = crate::process::require_success(
        run(
            root,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--",
                ".",
            ],
        )
        .await?,
    )?;
    // A truncated NUL record (including a rename pair) is never presented as complete.
    ensure!(
        !output.truncated,
        "Git status exceeds the inspection limit; narrow the project or ignore generated files"
    );
    // Porcelain v1 paths are repository-root-relative even when Git runs in a
    // selected subdirectory. Scope the pathspec above, then rebase every path
    // before the UI joins it to the selected root or asks for its diff.
    let prefix =
        crate::process::require_success(run(root, &["rev-parse", "--show-prefix"]).await?)?;
    ensure!(
        !prefix.truncated && !prefix.stdout.contains('\u{fffd}'),
        "Git returned an invalid project prefix"
    );
    let prefix = Path::new(prefix.stdout.trim_end_matches(['\r', '\n']));
    let changes = parse_status(&output.stdout)?
        .into_iter()
        .filter_map(|mut change| {
            let selected_path = change
                .path
                .strip_prefix(prefix)
                .ok()
                .filter(|path| !path.as_os_str().is_empty())
                .map(Path::to_path_buf);
            let selected_original = change
                .original
                .as_ref()
                .and_then(|original| original.strip_prefix(prefix).ok())
                .filter(|path| !path.as_os_str().is_empty())
                .map(Path::to_path_buf);
            if let Some(path) = selected_path {
                change.path = path;
                change.original = selected_original;
                // A rename entering the selected root is an addition in this view.
                if change.original.is_none() {
                    if matches!(change.index, 'R' | 'C') {
                        change.index = 'A';
                    }
                    if matches!(change.worktree, 'R' | 'C') {
                        change.worktree = 'A';
                    }
                }
            } else if let Some(original) = selected_original {
                // A rename leaving it is a deletion, without exposing its sibling.
                change.path = original;
                change.original = None;
                if matches!(change.index, 'R' | 'C') {
                    change.index = 'D';
                }
                if matches!(change.worktree, 'R' | 'C') {
                    change.worktree = 'D';
                }
            } else {
                return None;
            }
            Some(change)
        })
        .collect();
    let branch = crate::process::require_success(run(root, &["branch", "--show-current"]).await?)?
        .stdout
        .trim()
        .to_owned();
    Ok(GitStatus {
        branch: if branch.is_empty() {
            "Detached HEAD".into()
        } else {
            branch
        },
        changes,
        truncated: false,
    })
}
pub async fn diff(
    root: &Path,
    path: &Path,
    original: Option<&Path>,
    group: ChangeGroup,
) -> Result<String> {
    validate_path(path)?;
    if let Some(original) = original {
        validate_path(original)?;
    }
    let path = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("File name is not UTF-8"))?;
    let mut args = vec![
        "diff",
        "--relative",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
    ];
    if group == ChangeGroup::Untracked {
        let canonical_root = tokio::fs::canonicalize(root).await?;
        let canonical = tokio::fs::canonicalize(root.join(path)).await?;
        ensure!(
            canonical.starts_with(&canonical_root)
                && !canonical
                    .strip_prefix(&canonical_root)?
                    .components()
                    .any(|part| part.as_os_str() == ".git"),
            "File is outside the project"
        );
        args.extend(["--no-index", "--", "/dev/null", path]);
    } else {
        if group == ChangeGroup::Staged {
            args.push("--cached");
        }
        args.extend(["--", path]);
        if let Some(original) = original {
            args.push(
                original
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("File name is not UTF-8"))?,
            );
        }
    }
    let result = run(root, &args).await?;
    ensure!(
        result.code == Some(0) || (group == ChangeGroup::Untracked && result.code == Some(1)),
        "Could not read Git diff: {}",
        result.stderr
    );
    let mut patch = result.stdout;
    if result.truncated {
        patch.push_str("\n… Diff truncated at 512 KB. Open the file to inspect further.\n");
    }
    if patch.is_empty() {
        patch = "No textual changes.".into();
    }
    Ok(patch)
}

#[cfg(test)]
mod repository_tests {
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
        assert!(
            rename.contains("rename from old name.rs") && rename.contains("rename to new name.rs")
        );
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
}
