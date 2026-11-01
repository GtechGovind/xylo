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
const INSTRUCTION_FILES: [&str; 4] = [
    "AGENTS.md",
    "CLAUDE.md",
    "AGENTS.local.md",
    "CLAUDE.local.md",
];
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
    project_instruction_sources(workspace, budget).0
}

/// Expanded startup text and the canonical files that contributed to it. Keep
/// these sources with the text: an import may leave the active project.
pub fn project_instruction_sources(workspace: &Path, budget: usize) -> (String, Vec<PathBuf>) {
    project_instruction_sources_with_home(
        workspace,
        std::env::var_os("HOME").map(PathBuf::from).as_deref(),
        budget,
    )
}
#[cfg(test)]
fn project_instructions_with_home(workspace: &Path, home: Option<&Path>, budget: usize) -> String {
    project_instruction_sources_with_home(workspace, home, budget).0
}
fn project_instruction_sources_with_home(
    workspace: &Path,
    home: Option<&Path>,
    budget: usize,
) -> (String, Vec<PathBuf>) {
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
    let mut paths = Vec::new();
    let mut discovered = Vec::<String>::new();
    for directory in directories {
        let mut group = Vec::new();
        for name in INSTRUCTION_FILES {
            let path = directory.join(name);
            if let Some((text, sources)) = expanded(&path, workspace, home, budget)
                && !text.is_empty()
                && !discovered.contains(&text)
            {
                discovered.push(text.clone());
                group.push((path, text, sources));
            }
        }
        paths.extend(group.into_iter().rev());
    }
    if let Some(home) = home {
        for name in [".claude/CLAUDE.md", ".xylo/AGENTS.md"] {
            let path = home.join(name);
            if let Some((text, sources)) = expanded(&path, workspace, Some(home), budget) {
                paths.push((path, text, sources));
            }
        }
    }
    let mut sections: Vec<String> = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    let mut used = 0;
    let mut omitted = false;
    let mut sources = Vec::new();
    for (path, text, contributing) in paths {
        if text.is_empty() || texts.contains(&text) {
            continue;
        }
        let remaining = budget.saturating_sub(used);
        if remaining < MIN_FILE_BYTES {
            omitted = true;
            break;
        }
        let section = section(&display_path(&path, base), &text, remaining);
        used += section.len();
        texts.push(text);
        sections.push(section);
        sources.extend(contributing);
    }
    sections.reverse();
    let mut output = String::new();
    if omitted {
        output.push_str(OMITTED);
    }
    output.push_str(sections.join("\n").trim_end());
    (output, sources)
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

/// Retains the canonical source set alongside the expanded text.
fn expanded(
    path: &Path,
    workspace: &Path,
    home: Option<&Path>,
    budget: usize,
) -> Option<(String, Vec<PathBuf>)> {
    let mut sources = Vec::new();
    let text = expand(
        path,
        workspace,
        home,
        budget,
        0,
        &mut Default::default(),
        &mut sources,
    )?;
    Some((text, sources))
}

/// Expands whole-line imports, bounded before every read and canonicalized before access.
fn expand(
    path: &Path,
    workspace: &Path,
    home: Option<&Path>,
    budget: usize,
    depth: usize,
    seen: &mut std::collections::HashSet<PathBuf>,
    sources: &mut Vec<PathBuf>,
) -> Option<String> {
    let canonical = path.canonicalize().ok()?;
    if depth > 5 || !seen.insert(canonical.clone()) {
        return None;
    }
    if depth > 0
        && !workspace
            .canonicalize()
            .is_ok_and(|root| canonical.starts_with(root))
        && !home
            .and_then(|home| home.canonicalize().ok())
            .is_some_and(|home| canonical.starts_with(home))
    {
        return None;
    }
    let text = read_bounded(&canonical, budget)?;
    sources.push(canonical.clone());
    let mut output = String::new();
    let mut fenced = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
        }
        let imports = if fenced {
            Vec::new()
        } else {
            line.split_whitespace()
                .filter_map(|word| word.strip_prefix('@'))
                .map(|path| path.trim_end_matches([',', ';', ')', '.']))
                .filter(|path| !path.is_empty())
                .collect::<Vec<_>>()
        };
        if imports.is_empty() || line.split_whitespace().count() != 1 {
            output.push_str(line);
            output.push('\n');
        }
        for import in imports {
            let target = if let Some(tail) = import.strip_prefix("~/") {
                home.map(|home| home.join(tail))
            } else {
                Some(canonical.parent()?.join(import))
            };
            if let Some(target) = target.and_then(|target| {
                expand(
                    &target,
                    workspace,
                    home,
                    budget.saturating_sub(output.len()),
                    depth + 1,
                    seen,
                    sources,
                )
            }) {
                output.push_str(&target);
                output.push('\n');
            }
        }
        if output.len() >= budget {
            break;
        }
    }
    Some(output.trim().to_owned())
}

/// Lazily loaded instruction sources for one model turn, scoped to an explicit root.
/// The caller passes the active root on each touch so checkout changes cannot reuse state.
#[derive(Default)]
pub struct ScopedInstructions {
    root: PathBuf,
    seen: std::collections::HashSet<PathBuf>,
    used: usize,
    rules: Option<std::collections::BTreeMap<PathBuf, (String, Vec<PathBuf>)>>,
}
impl ScopedInstructions {
    /// Returns newly applicable project text, once per source and within the turn's budget.
    #[cfg(test)]
    pub fn touch(&mut self, workspace: &Path, path: &Path, budget: usize) -> String {
        self.touch_with_sources(workspace, path, budget).0
    }
    /// Sources are emitted only for matching instruction sections, including imports.
    pub fn touch_with_sources(
        &mut self,
        workspace: &Path,
        path: &Path,
        budget: usize,
    ) -> (String, Vec<PathBuf>) {
        let Ok(root) = workspace.canonicalize() else {
            return Default::default();
        };
        if self.root != root {
            self.root = root.clone();
            self.seen.clear();
            self.rules = None;
            self.used = project_instructions(&root, budget).len().min(budget);
        }
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            root.join(path)
        };
        let path = path
            .canonicalize()
            .or_else(|_| {
                path.parent()
                    .and_then(|p| p.canonicalize().ok())
                    .map(|p| p.join(path.file_name().unwrap_or_default()))
                    .ok_or(())
            })
            .ok();
        let Some(path) = path.filter(|path| path.starts_with(&root)) else {
            return Default::default();
        };
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let budget = budget.min(INSTRUCTION_BYTES);
        let mut candidates = Vec::new();
        let directory = if path.is_dir() {
            &path
        } else {
            path.parent().unwrap_or(&root)
        };
        let mut parents = directory
            .ancestors()
            .take_while(|p| *p != root && p.starts_with(&root))
            .collect::<Vec<_>>();
        parents.reverse();
        for parent in parents {
            for name in INSTRUCTION_FILES {
                candidates.push((parent.join(name), false));
            }
        }
        // Search can inspect thousands of files. Snapshot bounded rule text once per
        // turn/root so each inspected path only performs glob matching.
        let rules = self.rules.get_or_insert_with(|| {
            let mut rules = std::collections::BTreeMap::new();
            let mut remaining = 2 * 1024 * 1024usize;
            for folder in [".claude/rules", ".xylo/rules"] {
                if let Ok(entries) = std::fs::read_dir(root.join(folder)) {
                    let mut paths = entries
                        .filter_map(Result::ok)
                        .map(|entry| entry.path())
                        .filter(|path| path.extension().is_some_and(|extension| extension == "md"))
                        .take(256)
                        .collect::<Vec<_>>();
                    paths.sort();
                    for source in paths {
                        let Ok(source) = source.canonicalize() else {
                            continue;
                        };
                        if !source.starts_with(&root) || remaining < MIN_FILE_BYTES {
                            continue;
                        }
                        if let Some((text, sources)) =
                            expanded(&source, &root, home.as_deref(), budget.min(remaining))
                        {
                            remaining = remaining.saturating_sub(text.len());
                            rules.insert(source, (text, sources));
                        }
                    }
                }
            }
            rules
        });
        candidates.extend(rules.keys().cloned().map(|path| (path, true)));
        let mut output = String::new();
        let mut sources = Vec::new();
        for (source, rule) in candidates {
            let source = if rule {
                source
            } else {
                let Ok(source) = source.canonicalize() else {
                    continue;
                };
                source
            };
            if !source.starts_with(&root) || self.seen.contains(&source) {
                continue;
            }
            let remaining = budget.saturating_sub(self.used);
            if remaining < MIN_FILE_BYTES {
                break;
            }
            let expansion;
            let (text, contributing) = if rule {
                let Some(text) = self.rules.as_ref().and_then(|rules| rules.get(&source)) else {
                    continue;
                };
                text
            } else {
                expansion = expanded(&source, &root, home.as_deref(), remaining);
                let Some(text) = expansion.as_ref() else {
                    continue;
                };
                text
            };
            let body = if rule {
                let Some(body) = matching_rule(text, &display_path(&path, &root)) else {
                    continue;
                };
                body
            } else {
                text
            };
            let note = section(&display_path(&source, &root), body, remaining);
            self.used += note.len();
            self.seen.insert(source);
            output.push_str(&note);
            sources.extend(contributing.iter().cloned());
        }
        (output, sources)
    }
}

/// Rules use a small frontmatter subset: paths as a quoted list, comma list, or YAML list.
fn matching_rule<'a>(text: &'a str, path: &str) -> Option<&'a str> {
    let Some(front) = text.strip_prefix("---\n") else {
        return Some(text);
    };
    let (metadata, body) = front.split_once("\n---")?;
    let mut patterns = Vec::new();
    let mut collecting = false;
    for line in metadata.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("paths:") {
            collecting = true;
            patterns.extend(rule_patterns(value));
        } else if collecting && line.starts_with('-') {
            patterns.push(
                line.trim_start_matches('-')
                    .trim()
                    .trim_matches(['\"', '\''])
                    .to_owned(),
            );
        } else {
            collecting = false;
        }
    }
    if patterns.is_empty()
        || patterns.iter().any(|pattern| {
            globset::GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .is_ok_and(|glob| glob.compile_matcher().is_match(path))
        })
    {
        Some(body.trim())
    } else {
        None
    }
}

/// Splits path lists without splitting quoted globs or brace alternatives.
fn rule_patterns(value: &str) -> Vec<String> {
    let value = value.trim().trim_start_matches('[').trim_end_matches(']');
    let mut output = Vec::new();
    let mut token = String::new();
    let mut quote = None;
    let mut braces = 0usize;
    for ch in value.chars() {
        if let Some(end) = quote {
            if ch == end {
                quote = None
            } else {
                token.push(ch)
            }
        } else if matches!(ch, '\'' | '"') {
            quote = Some(ch);
        } else if ch == '{' {
            braces += 1;
            token.push(ch);
        } else if ch == '}' {
            braces = braces.saturating_sub(1);
            token.push(ch);
        } else if ch == ',' && braces == 0 {
            if !token.trim().is_empty() {
                output.push(token.trim().to_owned());
            }
            token.clear();
        } else {
            token.push(ch);
        }
    }
    if !token.trim().is_empty() {
        output.push(token.trim().to_owned());
    }
    output
}
#[cfg(test)]
mod tests;

#[cfg(test)]
mod provenance_tests;
