//! Project instruction discovery (AGENTS.md and CLAUDE.md).
//!
//! Every directory from the repository root down to the workspace is checked
//! for `AGENTS.md` then `CLAUDE.md` (outside a repository only the workspace
//! itself). Files are emitted root first, so the nearest file comes last and
//! wins on conflict. The byte budget (scaled to the model's window by the
//! caller, at most [`INSTRUCTION_BYTES`]) is spent nearest first: when it runs
//! out, the outermost files are the ones dropped or truncated. A file whose
//! text duplicates one already included (for example `CLAUDE.md` symlinked
//! to `AGENTS.md`) is skipped.

use std::{
    io::Read,
    path::{Path, PathBuf},
};

use super::environment::repository_root;

/// Instruction file names, in the order they are read within one directory.
const INSTRUCTION_FILES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];
/// Largest instruction budget any window gets.
pub(super) const INSTRUCTION_BYTES: usize = 24 * 1024;
/// Below this remaining budget no further file is started.
const MIN_FILE_BYTES: usize = 256;
const TRUNCATED: &str = "\n[… truncated]";
const OMITTED: &str = "[Outer instruction files omitted: size limit reached]\n";

/// Instruction files that apply to `workspace`, formatted as `## path`
/// sections in precedence order (root first, nearest last), bounded by
/// `budget` bytes (never more than [`INSTRUCTION_BYTES`]). Empty when there
/// are none.
pub fn project_instructions(workspace: &Path, budget: usize) -> String {
    let budget = budget.min(INSTRUCTION_BYTES);
    let repository = repository_root(workspace);
    let base = repository.as_deref().unwrap_or(workspace);
    let directories: Vec<&Path> = match &repository {
        Some(root) => workspace
            .ancestors()
            .take_while(|dir| dir.starts_with(root))
            .collect(),
        None => vec![workspace],
    };
    let mut sections: Vec<String> = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    let mut used = 0;
    let mut omitted = false;
    'directories: for directory in directories {
        // Within one directory AGENTS.md precedes CLAUDE.md in the output, so
        // collect the pair and add it in reverse to the nearest-first list.
        let mut found = Vec::new();
        for name in INSTRUCTION_FILES {
            let path = directory.join(name);
            let Some(text) = read_bounded(&path, budget) else {
                continue;
            };
            if text.is_empty() || texts.contains(&text) {
                continue;
            }
            let remaining = budget.saturating_sub(used);
            if remaining < MIN_FILE_BYTES {
                omitted = true;
                sections.extend(found.into_iter().rev());
                break 'directories;
            }
            let section = section(&display_path(&path, base), &text, remaining);
            used += section.len();
            texts.push(text);
            found.push(section);
        }
        sections.extend(found.into_iter().rev());
    }
    sections.reverse();
    let mut output = String::new();
    if omitted {
        output.push_str(OMITTED);
    }
    output.push_str(sections.join("\n").trim_end());
    output
}

/// Reads at most one budget's worth of a file, trimmed; `None` when absent
/// or unreadable.
fn read_bounded(path: &Path, budget: usize) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(budget as u64 + 1).read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).trim().to_owned())
}

fn section(shown: &str, text: &str, remaining: usize) -> String {
    let heading = format!("## {shown}\n");
    let room = remaining.saturating_sub(heading.len() + TRUNCATED.len() + 1);
    let mut section = heading;
    if text.len() <= room {
        section.push_str(text);
    } else {
        let mut cut = room;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        section.push_str(&text[..cut]);
        section.push_str(TRUNCATED);
    }
    section.push('\n');
    section
}

fn display_path(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| PathBuf::from(path))
        .to_string_lossy()
        .replace('\\', "/")
}
