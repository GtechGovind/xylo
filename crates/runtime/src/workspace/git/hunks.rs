//! Exact-preview hunk mutations through the native Git boundary.
use super::{ChangeGroup, Path, PathBuf, Result};
use anyhow::{Context, ensure};
use sha2::{Digest, Sha256};
use std::{io::Write, time::Duration};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Stage,
    Unstage,
    Revert,
}
#[derive(Clone, Debug)]
pub struct Hunk {
    pub index: usize,
    pub header_line: usize,
    pub old_start: usize,
    pub new_start: usize,
    pub patch: String,
}
#[derive(Clone, Debug)]
pub struct Request {
    pub path: PathBuf,
    pub original: Option<PathBuf>,
    pub group: ChangeGroup,
    pub hunk: usize,
    pub digest: String,
    pub operation: Operation,
}
/// Identifies the complete preview, including headers and all hunks.
pub fn digest(diff: &str) -> String {
    hex::encode(Sha256::digest(diff.as_bytes()))
}
/// Extracts separately applicable text hunks from a bounded Git diff.
pub fn parse(diff: &str) -> Result<Vec<Hunk>> {
    ensure!(
        diff.len() <= 512 * 1024,
        "Refresh a smaller diff before changing a hunk"
    );
    let lines: Vec<_> = diff.split_inclusive('\n').collect();
    let mut hunks = Vec::new();
    let mut header = String::new();
    let mut file_start = 0;
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if line.starts_with("diff --git ") {
            file_start = index;
            header.clear();
        }
        if line.starts_with("@@ ") {
            if header.is_empty() {
                header = lines[file_start..index].concat();
            }
            ensure!(
                header.starts_with("diff --git ")
                    && header.contains("\n--- ")
                    && header.contains("\n+++ "),
                "This change is not a supported text diff"
            );
            let fields: Vec<_> = line.split_whitespace().collect();
            ensure!(
                fields.len() >= 4 && fields[3] == "@@",
                "Invalid hunk header"
            );
            let coordinate = |input: &str, sign: char| -> Result<usize> {
                Ok(input
                    .strip_prefix(sign)
                    .context("Invalid hunk coordinate")?
                    .split(',')
                    .next()
                    .context("Missing hunk coordinate")?
                    .parse()?)
            };
            let old_start = coordinate(fields[1], '-')?;
            let new_start = coordinate(fields[2], '+')?;
            let end = (index + 1..lines.len())
                .find(|&next| {
                    lines[next].starts_with("@@ ") || lines[next].starts_with("diff --git ")
                })
                .unwrap_or(lines.len());
            hunks.push(Hunk {
                index: hunks.len(),
                header_line: index,
                old_start,
                new_start,
                patch: format!("{header}{}", lines[index..end].concat()),
            });
            index = end;
        } else {
            index += 1;
        }
    }
    Ok(hunks)
}

fn confined_path(root: &Path, path: &Path) -> Result<()> {
    super::validate_path(path)?;
    let joined = root.join(path);
    let existing = joined
        .ancestors()
        .find(|path| path.exists())
        .context("File parent is unavailable")?;
    ensure!(
        existing.canonicalize()?.starts_with(root),
        "The changed file leaves the selected project"
    );
    Ok(())
}
fn metadata_scope(native: &super::NativeGit) -> Result<()> {
    let pointer = native.worktree.join(".git");
    ensure!(
        !std::fs::symlink_metadata(&pointer)?
            .file_type()
            .is_symlink(),
        "Git metadata cannot be a symbolic link"
    );
    if pointer.is_file() {
        let back = super::bounded_text(&native.git_directory.join("gitdir"), 4096)?;
        ensure!(
            Path::new(back.trim()).canonicalize()? == pointer.canonicalize()?,
            "Git checkout registration does not match this project"
        );
    } else {
        ensure!(
            pointer.canonicalize()? == native.git_directory,
            "Git metadata changed"
        );
    }
    for name in ["index", "xylo-review.lock"] {
        if let Ok(metadata) = std::fs::symlink_metadata(native.git_directory.join(name)) {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "Git index and lock must be regular files"
            );
        }
    }
    Ok(())
}

/// Applies one hunk only if the current diff still matches its shown preview.
pub async fn apply(root: &Path, request: Request) -> Result<()> {
    let mut native = super::prepare_native(root)?;
    metadata_scope(&native)?;
    confined_path(&native.root, &request.path)?;
    if let Some(original) = &request.original {
        confined_path(&native.root, original)?;
    }
    ensure!(
        match request.operation {
            Operation::Stage => request.group != ChangeGroup::Staged,
            Operation::Unstage => request.group == ChangeGroup::Staged,
            Operation::Revert => request.group == ChangeGroup::Unstaged,
        },
        "Choose an action for this change group"
    );
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let lock = options.open(native.git_directory.join("xylo-review.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock).context("Another Git change is in progress")?;
    let diff = super::diff(
        &native.root,
        &request.path,
        request.original.as_deref(),
        request.group,
    )
    .await?;
    ensure!(
        digest(&diff) == request.digest,
        "This diff changed. Refresh it before applying the hunk."
    );
    let hunks = parse(&diff)?;
    let hunk = hunks
        .get(request.hunk)
        .context("This hunk is no longer available")?;
    let mut patch = tempfile::NamedTempFile::new()?;
    patch.write_all(hunk.patch.as_bytes())?;
    patch.flush()?;
    native
        .command
        .env("GIT_INDEX_FILE", native.git_directory.join("index"));
    native.command.args([
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "submodule.recurse=false",
        "apply",
        "--whitespace=nowarn",
    ]);
    match request.operation {
        Operation::Stage => {
            native.command.arg("--cached");
        }
        Operation::Unstage => {
            native.command.args(["--cached", "--reverse"]);
        }
        Operation::Revert => {
            native.command.arg("--reverse");
        }
    }
    let prefix = native.root.strip_prefix(&native.worktree)?;
    if !prefix.as_os_str().is_empty() {
        native.command.arg(format!(
            "--directory={}",
            prefix.to_str().context("Project prefix is not UTF-8")?
        ));
    }
    native.command.arg("--").arg(patch.path());
    crate::process::require_success(
        crate::process::run_prepared(
            native.command,
            &native.worktree,
            Duration::from_secs(30),
            Default::default(),
        )
        .await?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests;
