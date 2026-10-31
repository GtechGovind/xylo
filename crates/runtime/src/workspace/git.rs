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
        if let Ok(candidate) = candidate.canonicalize()
            && candidate.is_file()
            && !candidate.starts_with(&root)
        {
            return Ok(candidate);
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
    String::from_utf8(bytes).context("Git metadata is not UTF-8")
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
            } else {
                // A rename leaving it is a deletion, without exposing its sibling.
                change.path = selected_original?;
                change.original = None;
                if matches!(change.index, 'R' | 'C') {
                    change.index = 'D';
                }
                if matches!(change.worktree, 'R' | 'C') {
                    change.worktree = 'D';
                }
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
mod tests;

#[cfg(test)]
mod repository_tests;

#[cfg(unix)]
#[cfg(test)]
mod executable_config_tests;
