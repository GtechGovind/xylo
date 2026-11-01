//! Owned Git worktrees with neutral configuration and conservative cleanup.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

/// A checkout created by Xylo, including its original local base commit.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    pub repository: PathBuf,
    pub base: String,
}
/// Changes and divergence from the recorded base; unpushed counts new commits
/// absent from every locally known remote tracking branch. No network is used.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorktreeStatus {
    pub changed_files: Vec<String>,
    pub ahead: u64,
    pub behind: u64,
    pub unpushed: u64,
}
impl WorktreeStatus {
    /// Whether automatic cleanup can remove this checkout without losing work.
    pub fn removable(&self) -> bool {
        self.changed_files.is_empty() && self.unpushed == 0
    }
}
/// A merge leaves conflicts in place for the user to resolve.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SyncResult {
    pub merged: bool,
    pub conflicts: Vec<String>,
}
fn valid_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 80
            && name.as_bytes()[0].is_ascii_alphanumeric()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "Use a worktree name of letters, numbers, dashes and underscores"
    );
    Ok(())
}
fn plain_directory(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "Worktree directories cannot be symbolic links: {}",
        path.display()
    );
    Ok(())
}
fn directory(path: &Path) -> Result<()> {
    if !path.exists() {
        std::fs::create_dir(path)?;
    }
    plain_directory(path)
}
fn repository(repo: &Path) -> Result<PathBuf> {
    let root = repo.canonicalize()?;
    plain_directory(&root.join(".git"))?;
    ensure!(
        root.join(".git/HEAD").is_file(),
        "Choose the main Git checkout"
    );
    Ok(root)
}
fn records(repo: &Path) -> PathBuf {
    repo.join(".git/xylo-worktrees")
}
fn record_path(repo: &Path, name: &str) -> PathBuf {
    records(repo).join(format!("{name}.json"))
}
/// Validates the checkout and its ownership record without running Git.
pub fn inspect(path: &Path) -> Result<Worktree> {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .context("Invalid worktree path")?;
    valid_name(name)?;
    let repo = path
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .context("Invalid worktree root")?;
    let repo = repository(repo)?;
    ensure!(
        path == repo.join(".xylo/worktrees").join(name),
        "Only managed worktrees can be changed"
    );
    plain_directory(&repo.join(".xylo"))?;
    plain_directory(&repo.join(".xylo/worktrees"))?;
    plain_directory(path)?;
    plain_directory(&records(&repo))?;
    let record = record_path(&repo, name);
    ensure!(
        !std::fs::symlink_metadata(&record)?.file_type().is_symlink(),
        "Invalid worktree ownership record"
    );
    let tree: Worktree = serde_json::from_slice(&std::fs::read(record)?)?;
    ensure!(
        tree.path == path && tree.repository == repo && tree.branch == format!("xylo/{name}"),
        "Worktree ownership changed"
    );
    ensure!(
        !std::fs::symlink_metadata(path.join(".git"))?
            .file_type()
            .is_symlink(),
        "Worktree Git pointers cannot be symbolic links"
    );
    let pointer = std::fs::read_to_string(path.join(".git"))?;
    let admin = PathBuf::from(
        pointer
            .strip_prefix("gitdir: ")
            .context("Invalid worktree Git pointer")?
            .trim(),
    );
    ensure!(
        admin.parent() == Some(repo.join(".git/worktrees").as_path()),
        "Worktree metadata is outside its repository"
    );
    plain_directory(&admin)?;
    ensure!(
        std::fs::read_to_string(admin.join("gitdir"))?.trim()
            == path.join(".git").to_string_lossy(),
        "Worktree registration changed"
    );
    ensure!(
        std::fs::read_to_string(admin.join("HEAD"))?.trim()
            == format!("ref: refs/heads/{}", tree.branch),
        "Worktree branch changed; retain this checkout for manual recovery"
    );
    Ok(tree)
}
fn lock(repo: &Path) -> Result<std::fs::File> {
    let path = repo.join(".git/xylo-worktrees.lock");
    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "Invalid worktree lock"
        );
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    fs2::FileExt::try_lock_exclusive(&file).context("Another worktree operation is running")?;
    Ok(file)
}
/// Every native operation uses only Git data through a neutral common directory.
/// Repository/user config, hooks, attributes and credential helpers are never
/// imported. Protocols and lazy fetching are disabled; no shell is involved.
async fn run(
    repo: &Path,
    tree: Option<&Path>,
    args: &[&str],
) -> Result<crate::process::ProcessResult> {
    let common = repo.join(".git");
    for name in ["objects", "refs", "worktrees"] {
        plain_directory(&common.join(name))?;
    }
    ensure!(
        !common.join("objects/info/alternates").exists(),
        "Native worktrees do not read alternate object stores outside this repository"
    );
    let overlay = tempfile::tempdir()?;
    for name in ["objects", "refs", "packed-refs", "shallow", "worktrees"] {
        super::git::metadata_link(&common.join(name), &overlay.path().join(name))?;
    }
    let original = super::git::bounded_text(&common.join("config"), 1024 * 1024)?;
    let sha256 = original
        .lines()
        .any(|line| line.trim().eq_ignore_ascii_case("objectformat = sha256"));
    let config = if sha256 {
        "[core]\nrepositoryformatversion = 1\nbare = false\n[extensions]\nobjectformat = sha256\n"
    } else {
        "[core]\nrepositoryformatversion = 0\nbare = false\n"
    };
    std::fs::write(overlay.path().join("config"), config)?;
    let git_dir = if let Some(tree) = tree {
        let pointer = std::fs::read_to_string(tree.join(".git"))?;
        PathBuf::from(
            pointer
                .strip_prefix("gitdir: ")
                .context("Invalid Git pointer")?
                .trim(),
        )
    } else {
        std::fs::copy(common.join("HEAD"), overlay.path().join("HEAD"))?;
        overlay.path().to_owned()
    };
    let mut command = tokio::process::Command::new(super::git::trusted_git(repo)?);
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("HOME", overlay.path())
        .env("GIT_DIR", git_dir)
        .env("GIT_COMMON_DIR", overlay.path())
        .env("GIT_WORK_TREE", tree.unwrap_or(repo))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_AUTHOR_NAME", "Xylo")
        .env("GIT_AUTHOR_EMAIL", "xylo@localhost")
        .env("GIT_COMMITTER_NAME", "Xylo")
        .env("GIT_COMMITTER_EMAIL", "xylo@localhost");
    #[cfg(windows)]
    for name in ["SYSTEMROOT", "WINDIR"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.args([
        "--no-pager",
        "--literal-pathspecs",
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.fsmonitor=false",
        "-c",
        "protocol.allow=never",
        "-c",
        "commit.gpgSign=false",
        "-c",
        "gc.auto=0",
        "-c",
        "maintenance.auto=false",
    ]);
    command.args(args);
    crate::process::run_prepared(
        command,
        tree.unwrap_or(repo),
        Duration::from_secs(60),
        Default::default(),
    )
    .await
}
async fn checked(repo: &Path, tree: Option<&Path>, args: &[&str]) -> Result<String> {
    let result = run(repo, tree, args).await?;
    ensure!(
        result.code == Some(0),
        "Git worktree operation failed: {}",
        result.stderr
    );
    ensure!(!result.truncated, "Git worktree output exceeded its limit");
    Ok(result.stdout)
}
async fn resolve(repo: &Path, reference: &str) -> Result<String> {
    ensure!(
        !reference.starts_with('-')
            && !reference.is_empty()
            && !reference.contains(['\n', '\r', '\0']),
        "Invalid base reference"
    );
    let oid = checked(
        repo,
        None,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{reference}^{{commit}}"),
        ],
    )
    .await?;
    let oid = oid.trim();
    ensure!(
        matches!(oid.len(), 40 | 64) && oid.bytes().all(|c| c.is_ascii_hexdigit()),
        "Invalid base commit"
    );
    Ok(oid.into())
}
/// Creates `.xylo/worktrees/name` on `xylo/name` without checking out the main tree.
pub async fn create(repo: &Path, name: &str, base_ref: &str) -> Result<Worktree> {
    valid_name(name)?;
    let repo = repository(repo)?;
    let _lock = lock(&repo)?;
    directory(&repo.join(".xylo"))?;
    directory(&repo.join(".xylo/worktrees"))?;
    directory(&repo.join(".git/worktrees"))?;
    directory(&records(&repo))?;
    plain_directory(&repo.join(".git/refs"))?;
    directory(&repo.join(".git/refs/heads"))?;
    directory(&repo.join(".git/refs/heads/xylo"))?;
    let path = repo.join(".xylo/worktrees").join(name);
    ensure!(
        std::fs::symlink_metadata(&path).is_err() && !record_path(&repo, name).exists(),
        "A worktree with this name already exists"
    );
    ensure!(
        std::fs::symlink_metadata(repo.join(".git/worktrees").join(name)).is_err(),
        "Git already has a registration with this name"
    );
    let base = resolve(&repo, base_ref).await?;
    let tree = Worktree {
        path,
        branch: format!("xylo/{name}"),
        repository: repo.clone(),
        base,
    };
    directory(&repo.join(".git/info"))?;
    let exclude = repo.join(".git/info/exclude");
    if let Ok(metadata) = std::fs::symlink_metadata(&exclude) {
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "Git exclude must be a regular file"
        );
    }
    use std::io::Write;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(exclude)?
        .write_all(b"\n/.xylo/worktrees/\n")?;
    let mut record = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(record_path(&repo, name))?;
    record.write_all(&serde_json::to_vec(&tree)?)?;
    record.sync_all()?;
    if let Err(error) = checked(
        &repo,
        None,
        &[
            "worktree",
            "add",
            "--no-track",
            "-b",
            &tree.branch,
            tree.path.to_str().context("Worktree path is not UTF-8")?,
            &tree.base,
        ],
    )
    .await
    {
        if !tree.path.exists() {
            std::fs::remove_file(record_path(&repo, name))?;
        }
        return Err(error);
    }
    // Git wrote the temporary common-directory spelling; persist the actual
    // repository's admin path before the neutral view disappears.
    let admin = repo.join(".git/worktrees").join(name);
    ensure!(admin.is_dir(), "Worktree registration missing");
    std::fs::write(
        tree.path.join(".git"),
        format!("gitdir: {}\n", admin.display()),
    )?;
    Ok(tree)
}
/// Lists only owned checkouts. Damaged ownership records are surfaced as errors.
pub async fn list(repo: &Path) -> Result<Vec<Worktree>> {
    let repo = repository(repo)?;
    if !records(&repo).exists() {
        return Ok(Vec::new());
    }
    plain_directory(&records(&repo))?;
    let mut result = Vec::new();
    for entry in std::fs::read_dir(records(&repo))? {
        let entry = entry?;
        let name = entry
            .path()
            .file_stem()
            .and_then(|s| s.to_str())
            .context("Invalid worktree record")?
            .to_owned();
        valid_name(&name)?;
        result.push(inspect(&repo.join(".xylo/worktrees").join(name))?);
    }
    result.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(result)
}
/// Reports disk changes, divergence, and locally known unpushed commits.
pub async fn status(path: &Path) -> Result<WorktreeStatus> {
    let tree = inspect(path)?;
    status_inner(&tree).await
}
async fn status_inner(tree: &Worktree) -> Result<WorktreeStatus> {
    let raw = checked(
        &tree.repository,
        Some(&tree.path),
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignored=matching",
            "--ignore-submodules=none",
        ],
    )
    .await?;
    let counts = checked(
        &tree.repository,
        Some(&tree.path),
        &[
            "rev-list",
            "--left-right",
            "--count",
            &format!("{}...HEAD", tree.base),
        ],
    )
    .await?;
    let mut counts = counts.split_whitespace();
    let behind = counts.next().context("Missing behind count")?.parse()?;
    let ahead = counts.next().context("Missing ahead count")?.parse()?;
    let unpushed = checked(
        &tree.repository,
        Some(&tree.path),
        &[
            "rev-list",
            "--count",
            "HEAD",
            &format!("^{}", tree.base),
            "--not",
            "--remotes",
        ],
    )
    .await?
    .trim()
    .parse()?;
    Ok(WorktreeStatus {
        changed_files: raw.split_terminator('\0').map(str::to_owned).collect(),
        ahead,
        behind,
        unpushed,
    })
}
/// Removes only a registered Xylo worktree. Force is an explicit destructive
/// override; branches are retained so committed history remains recoverable.
pub async fn remove(path: &Path, force: bool) -> Result<()> {
    let tree = inspect(path)?;
    let _lock = lock(&tree.repository)?;
    ensure!(
        force || status_inner(&tree).await?.removable(),
        "Worktree contains changed files or unpushed commits; keep it or explicitly force removal"
    );
    let path_text = path.to_str().context("Worktree path is not UTF-8")?;
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.extend(["--", path_text]);
    checked(&tree.repository, None, &args).await?;
    std::fs::remove_file(record_path(
        &tree.repository,
        path.file_name().unwrap().to_str().unwrap(),
    ))?;
    Ok(())
}
/// Merges an existing local base commit; it never fetches or runs merge drivers.
pub async fn sync_with_base(path: &Path, base: &str) -> Result<SyncResult> {
    let tree = inspect(path)?;
    let _lock = lock(&tree.repository)?;
    ensure!(
        status_inner(&tree).await?.changed_files.is_empty(),
        "Commit or restore worktree changes before syncing"
    );
    let base = resolve(&tree.repository, base).await?;
    let result = run(
        &tree.repository,
        Some(path),
        &["merge", "--no-edit", "--no-verify", "--no-gpg-sign", &base],
    )
    .await?;
    let conflicts = checked(
        &tree.repository,
        Some(path),
        &["diff", "--name-only", "--diff-filter=U", "-z"],
    )
    .await?
    .split_terminator('\0')
    .map(str::to_owned)
    .collect::<Vec<_>>();
    ensure!(
        result.code == Some(0) || !conflicts.is_empty(),
        "Merge failed: {}",
        result.stderr
    );
    Ok(SyncResult {
        merged: result.code == Some(0),
        conflicts,
    })
}
#[cfg(test)]
mod tests;
