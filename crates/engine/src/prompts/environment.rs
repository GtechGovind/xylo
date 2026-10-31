//! Environment facts for the system prompt: repository, branch and date.

use std::path::{Path, PathBuf};

/// Version-control state of the workspace as shown to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitInfo {
    /// Checked-out branch; `None` for a detached HEAD or an unreadable HEAD.
    pub branch: Option<String>,
}

impl GitInfo {
    /// Detects the repository that contains `workspace`, if any.
    pub fn detect(workspace: &Path) -> Option<GitInfo> {
        let repository = repository_root(workspace)?;
        Some(GitInfo {
            branch: current_branch(&repository),
        })
    }
}

/// The nearest ancestor of `workspace` (inclusive) that holds `.git`.
pub(super) fn repository_root(workspace: &Path) -> Option<PathBuf> {
    workspace
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

fn current_branch(repository: &Path) -> Option<String> {
    let dot_git = repository.join(".git");
    let git_dir = if dot_git.is_file() {
        // Worktrees and submodules point at their real Git directory.
        let pointer = std::fs::read_to_string(&dot_git).ok()?;
        let target = PathBuf::from(pointer.strip_prefix("gitdir:")?.trim());
        if target.is_absolute() {
            target
        } else {
            repository.join(target)
        }
    } else {
        dot_git
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    head.trim()
        .strip_prefix("ref: refs/heads/")
        .map(str::to_owned)
}

/// Civil UTC date (`YYYY-MM-DD`) from Unix milliseconds, proleptic Gregorian.
///
/// The system prompt carries only the day so it stays byte-identical, and
/// therefore cacheable by providers, for a whole day.
pub fn utc_date(millis: i64) -> String {
    let days = millis.div_euclid(86_400_000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}
