//! Read-only Git inspection for the native workspace; never runs on the UI thread.
use anyhow::{Context, Result, ensure};
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
fn trusted_git(root: &Path) -> Result<PathBuf> {
    let root = root.canonicalize()?;
    #[cfg(unix)]
    let candidates = [
        PathBuf::from("/usr/bin/git"),
        PathBuf::from("/opt/homebrew/bin/git"),
        PathBuf::from("/usr/local/bin/git"),
    ];
    #[cfg(windows)]
    let candidates = [
        PathBuf::from(r"C:\Program Files\Git\cmd\git.exe"),
        PathBuf::from(r"C:\Program Files\Git\bin\git.exe"),
    ];
    for candidate in candidates {
        if let Ok(candidate) = candidate.canonicalize() {
            if candidate.is_file() && !candidate.starts_with(&root) {
                return Ok(candidate);
            }
        }
    }
    anyhow::bail!(
        "Install a trusted system Git outside the selected project to enable native inspection"
    )
}
fn bounded_text(path: &Path, limit: usize) -> Result<String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= limit,
        "Git metadata exceeds inspection budget"
    );
    Ok(String::from_utf8(bytes).context("Git metadata is not UTF-8")?)
}
fn resolve_git_directory(root: &Path) -> Result<(PathBuf, PathBuf)> {
    for parent in root.ancestors() {
        let entry = parent.join(".git");
        if entry.is_dir() {
            return Ok((parent.to_owned(), entry.canonicalize()?));
        }
        if entry.is_file() {
            let value = bounded_text(&entry, 4096)?;
            let value = value
                .strip_prefix("gitdir:")
                .context("Invalid Git directory pointer")?
                .trim();
            return Ok((parent.to_owned(), parent.join(value).canonicalize()?));
        }
    }
    anyhow::bail!("Selected project is not a Git worktree")
}
fn metadata_link(source: &Path, target: &Path) -> Result<()> {
    if !source.exists() {
        return Ok(());
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(source, target)?;
    #[cfg(windows)]
    if source.is_dir() {
        std::os::windows::fs::symlink_dir(source, target)?
    } else {
        std::os::windows::fs::symlink_file(source, target)?
    }
    Ok(())
}
/// A native read view never loads repository executable configuration. Merely
/// disabling external diff/textconv is insufficient: clean/process filters and
/// info/attributes can execute commands too. Only Git data is linked into this
/// neutral metadata snapshot; global/user config and environment are absent.
pub async fn run_read(
    root: &Path,
    args: &[String],
    timeout: Duration,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<crate::process::ProcessResult> {
    ensure!(
        matches!(
            args.first().map(String::as_str),
            Some("status" | "diff" | "branch" | "rev-parse")
        ),
        "Unsupported native Git read command"
    );
    let root = root.canonicalize()?;
    let git = trusted_git(&root)?;
    let (worktree, directory) = resolve_git_directory(&root)?;
    let common = if directory.join("commondir").is_file() {
        directory
            .join(bounded_text(&directory.join("commondir"), 4096)?.trim())
            .canonicalize()?
    } else {
        directory.clone()
    };
    let overlay = tempfile::tempdir()?;
    std::fs::write(
        overlay.path().join("HEAD"),
        bounded_text(&directory.join("HEAD"), 4096)?,
    )?;
    metadata_link(&directory.join("index"), &overlay.path().join("index"))?;
    for name in ["objects", "refs", "packed-refs", "shallow"] {
        metadata_link(&common.join(name), &overlay.path().join(name))?;
    }
    let original = bounded_text(&common.join("config"), 1024 * 1024)?;
    let mut section = String::new();
    let mut object_format = "sha1";
    let mut options = Vec::new();
    for line in original.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            section = line.to_ascii_lowercase();
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim().trim_matches('"');
            if section == "[extensions]" && key == "objectformat" {
                ensure!(
                    matches!(value, "sha1" | "sha256"),
                    "Unsupported Git object format"
                );
                object_format = value;
            }
            if section == "[core]"
                && matches!(
                    key.as_str(),
                    "ignorecase" | "filemode" | "precomposeunicode" | "symlinks"
                )
                && matches!(value, "true" | "false")
            {
                options.push(format!("\t{key} = {value}\n"));
            }
        }
    }
    let mut config = format!(
        "[core]\nrepositoryformatversion = {}\nbare = false\nfsmonitor = false\n",
        if object_format == "sha256" { 1 } else { 0 }
    );
    for option in options {
        config.push_str(&option)
    }
    if object_format == "sha256" {
        config.push_str("[extensions]\nobjectformat = sha256\n");
    }
    std::fs::write(overlay.path().join("config"), config)?;
    // Ignore patterns are data; do not import info/attributes or hook programs.
    if common.join("info/exclude").is_file()
        && !std::fs::symlink_metadata(common.join("info/exclude"))?
            .file_type()
            .is_symlink()
    {
        std::fs::create_dir(overlay.path().join("info"))?;
        std::fs::write(
            overlay.path().join("info/exclude"),
            bounded_text(&common.join("info/exclude"), 1024 * 1024)?,
        )?;
    }
    let mut command = tokio::process::Command::new(git);
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("HOME", overlay.path())
        .env("GIT_DIR", overlay.path())
        .env("GIT_COMMON_DIR", overlay.path())
        .env("GIT_WORK_TREE", worktree)
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1");
    #[cfg(windows)]
    for name in ["SYSTEMROOT", "WINDIR"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.args([
        "--no-pager",
        "--no-optional-locks",
        "--literal-pathspecs",
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.quotePath=false",
    ]);
    if matches!(args.first().map(String::as_str), Some("status" | "diff")) {
        command
            .arg(&args[0])
            .arg("--ignore-submodules=all")
            .args(&args[1..]);
    } else {
        command.args(args);
    }

    crate::process::run_prepared(command, &root, timeout, cancel).await
}
async fn run(root: &Path, args: &[&str]) -> Result<crate::process::ProcessResult> {
    run_read(
        root,
        &args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>(),
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

#[cfg(unix)]
#[cfg(test)]
mod executable_config_tests {
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
}
