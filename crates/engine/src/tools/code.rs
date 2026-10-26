//! Coding tools: ranged reads, exact edits, project search, file discovery and
//! command resolution. Callers check workspace boundaries before any I/O here.

use anyhow::{Context, Result, ensure};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};
use tokio_util::sync::CancellationToken;

/// Default and maximum line window for one read.
const READ_LINES: u64 = 2000;
const READ_MAX_LINES: u64 = 10_000;
/// Keeps one read comfortably inside the model-facing result budget.
const READ_BYTES: usize = 40 * 1024;
const LINE_CHARS: usize = 2000;
const READ_FILE_BYTES: u64 = 64 * 1024 * 1024;
const EDIT_FILE_BYTES: u64 = 8 * 1024 * 1024;
const DIFF_BYTES: usize = 24 * 1024;
const SEARCH_FILE_BYTES: u64 = 4 * 1024 * 1024;
const SEARCH_LINE_CHARS: usize = 300;
const WALK_ENTRIES: usize = 200_000;

/// The operation was rejected before it changed anything, so its outcome is
/// definitive rather than uncertain, even for a writing tool.
#[derive(Debug)]
pub(crate) struct Unapplied(pub(crate) String);
impl std::fmt::Display for Unapplied {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str(&self.0)
    }
}
impl std::error::Error for Unapplied {}

pub(crate) fn unapplied(message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(Unapplied(message.into()))
}

fn text<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("Missing text parameter {key}"))
}

/// Workspace-relative display with forward slashes; outside paths stay absolute.
pub(crate) fn display(root: &Path, path: &Path) -> String {
    Shown::new(root).show(path)
}

/// Resolves the workspace root once, as given and canonical, for many paths.
pub(crate) struct Shown {
    raw: PathBuf,
    canonical: Option<PathBuf>,
}
impl Shown {
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            raw: root.to_path_buf(),
            canonical: std::fs::canonicalize(root).ok(),
        }
    }
    pub(crate) fn show(&self, path: &Path) -> String {
        let relative = path
            .strip_prefix(&self.raw)
            .ok()
            .or_else(|| path.strip_prefix(self.canonical.as_ref()?).ok());
        match relative {
            Some(relative) if relative.as_os_str().is_empty() => ".".into(),
            Some(relative) => relative.to_string_lossy().replace('\\', "/"),
            None => path.to_string_lossy().into_owned(),
        }
    }
}

/// Numbered lines (`cat -n` style) from a 1-based offset, bounded by lines and bytes.
pub(crate) fn read(path: &Path, shown: &str, args: &Value) -> Result<Value> {
    let metadata = std::fs::metadata(path)?;
    ensure!(
        metadata.is_file(),
        "{shown} is not a file; use files_list or find_files for directories"
    );
    ensure!(
        metadata.len() <= READ_FILE_BYTES,
        "{shown} exceeds 64 MiB; use search_project for a narrower excerpt"
    );
    let start = args
        .get("offset")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1);
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(READ_LINES)
        .clamp(1, READ_MAX_LINES);
    let mut reader = BufReader::new(std::fs::File::open(path)?);
    ensure!(
        !reader.fill_buf()?.contains(&0),
        "{shown} appears to be binary and cannot be shown as text"
    );
    let mut line = Vec::new();
    let mut number = 0_u64;
    let mut end = 0_u64;
    let mut content = String::new();
    let mut full = false;
    let mut shortened = 0_usize;
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        number += 1;
        if full || number < start || number >= start.saturating_add(limit) {
            continue;
        }
        let decoded = String::from_utf8_lossy(&line);
        let decoded = decoded.trim_end_matches(['\n', '\r']);
        let row = if decoded.chars().count() > LINE_CHARS {
            shortened += 1;
            let kept: String = decoded.chars().take(LINE_CHARS).collect();
            format!("{number:>6}\t{kept}… [line shortened]\n")
        } else {
            format!("{number:>6}\t{decoded}\n")
        };
        if end != 0 && content.len() + row.len() > READ_BYTES {
            full = true;
            continue;
        }
        content.push_str(&row);
        end = number;
    }
    ensure!(
        number == 0 || start <= number,
        "offset {start} is past the end of {shown} ({number} lines)"
    );
    let more = end < number;
    let mut value = json!({
        "path": shown,
        "start_line": if end == 0 { 0 } else { start },
        "end_line": end,
        "total_lines": number,
        "content": content,
        "truncated": more,
    });
    if more {
        value["next_offset"] = json!(end + 1);
    }
    if shortened > 0 {
        value["long_lines_shortened"] = json!(shortened);
    }
    if number == 0 {
        value["note"] = json!("The file is empty");
    }
    Ok(value)
}

fn read_text(path: &Path, shown: &str) -> Result<String> {
    let metadata = std::fs::metadata(path).map_err(|_| {
        unapplied(format!(
            "{shown} does not exist; use file_write to create it"
        ))
    })?;
    if !metadata.is_file() {
        return Err(unapplied(format!("{shown} is not a file")));
    }
    if metadata.len() > EDIT_FILE_BYTES {
        return Err(unapplied(format!(
            "{shown} exceeds 8 MiB and cannot be edited"
        )));
    }
    String::from_utf8(std::fs::read(path)?)
        .map_err(|_| unapplied(format!("{shown} is not UTF-8 text and cannot be edited")))
}

pub(crate) struct EditPlan {
    pub(crate) original: String,
    pub(crate) updated: String,
    pub(crate) replacements: usize,
}

/// Exact-string replacement against the file's current content. Every rejection
/// happens before a write, so it is reported as definitive.
pub(crate) fn plan_edit(path: &Path, shown: &str, args: &Value) -> Result<EditPlan> {
    let old = text(args, "old_string")?;
    let new = text(args, "new_string")?;
    let all = args
        .get("replace_all")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if old.is_empty() {
        return Err(unapplied(
            "old_string is empty; use file_write to create or replace a whole file",
        ));
    }
    if old == new {
        return Err(unapplied("old_string and new_string are identical"));
    }
    let original = read_text(path, shown)?;
    // Models usually send LF; keep CRLF files consistent instead of failing.
    let (old, new) = if !original.contains(old)
        && original.contains("\r\n")
        && old.contains('\n')
        && !old.contains("\r\n")
    {
        (
            old.replace('\n', "\r\n"),
            new.replace("\r\n", "\n").replace('\n', "\r\n"),
        )
    } else {
        (old.to_owned(), new.to_owned())
    };
    let count = original.matches(old.as_str()).count();
    if count == 0 {
        return Err(unapplied(format!(
            "old_string was not found in {shown}. Read the file again and copy the exact text, including indentation, without line-number prefixes"
        )));
    }
    if count > 1 && !all {
        return Err(unapplied(format!(
            "old_string matches {count} places in {shown}. Include more surrounding lines to make it unique, or set replace_all to true"
        )));
    }
    let updated = if all {
        original.replace(&old, &new)
    } else {
        original.replacen(&old, &new, 1)
    };
    Ok(EditPlan {
        original,
        updated,
        replacements: if all { count } else { 1 },
    })
}

/// The current text a whole-file write replaces, if any.
pub(crate) fn existing_text(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > EDIT_FILE_BYTES {
        return None;
    }
    String::from_utf8(std::fs::read(path).ok()?).ok()
}

/// Unified diff for approval and results, bounded for events and context.
pub(crate) fn diff(shown: &str, before: &str, after: &str) -> Value {
    use similar::{Algorithm, ChangeTag, TextDiff};
    let changes = TextDiff::configure()
        .algorithm(Algorithm::Patience)
        .timeout(Duration::from_millis(500))
        .diff_lines(before, after);
    let (mut added, mut removed) = (0_usize, 0_usize);
    for change in changes.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => removed += 1,
            ChangeTag::Equal => {}
        }
    }
    let mut unified = changes
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{shown}"), &format!("b/{shown}"))
        .to_string();
    let truncated = unified.len() > DIFF_BYTES;
    if truncated {
        let mut cut = DIFF_BYTES;
        while !unified.is_char_boundary(cut) {
            cut -= 1;
        }
        unified.truncate(cut);
        unified.push_str("\n… diff shortened …\n");
    }
    json!({"diff": unified, "added": added, "removed": removed, "diff_truncated": truncated})
}

/// Atomic replacement that keeps an existing file's permissions.
pub(crate) fn write_atomic(path: &Path, content: &str) -> Result<()> {
    use std::io::Write;
    let temp = path.with_file_name(format!(".workbench-{}.tmp", uuid::Uuid::new_v4()));
    let permissions = std::fs::metadata(path).ok().map(|m| m.permissions());
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);
        if let Some(permissions) = permissions {
            std::fs::set_permissions(&temp, permissions)?;
        }
        std::fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn walker(base: &Path) -> ignore::Walk {
    ignore::WalkBuilder::new(base)
        .hidden(false)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build()
}

/// Comma-separated globs matched against base-relative paths; a pattern
/// without a slash matches at any depth. Ignore files still apply.
fn glob_filter(globs: Option<&str>) -> Result<Option<globset::GlobSet>> {
    let Some(globs) = globs.filter(|globs| !globs.trim().is_empty()) else {
        return Ok(None);
    };
    let mut set = globset::GlobSetBuilder::new();
    for pattern in globs.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let pattern = pattern.replace('\\', "/");
        let pattern = if pattern.contains('/') {
            pattern
        } else {
            format!("**/{pattern}")
        };
        set.add(
            globset::GlobBuilder::new(&pattern)
                .literal_separator(true)
                .build()
                .context("Invalid glob filter")?,
        );
    }
    Ok(Some(set.build().context("Invalid glob filter")?))
}

fn relative(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

struct Collect<'a> {
    shown: &'a str,
    output: &'a mut String,
    matches: &'a mut usize,
    limit: usize,
    files_only: bool,
    matched_file: bool,
}
impl Collect<'_> {
    fn line(&mut self, separator: char, number: Option<u64>, bytes: &[u8]) {
        let decoded = String::from_utf8_lossy(bytes);
        let mut decoded = decoded.trim_end_matches(['\n', '\r']).to_owned();
        if decoded.chars().count() > SEARCH_LINE_CHARS {
            decoded = decoded.chars().take(SEARCH_LINE_CHARS).collect::<String>() + "…";
        }
        let number = number.map_or_else(String::new, |n| n.to_string());
        self.output.push_str(&format!(
            "{}{separator}{number}{separator}{decoded}\n",
            self.shown
        ));
    }
}
impl Sink for Collect<'_> {
    type Error = std::io::Error;
    fn matched(&mut self, _: &Searcher, found: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        self.matched_file = true;
        if self.files_only {
            return Ok(false);
        }
        *self.matches += 1;
        self.line(':', found.line_number(), found.bytes());
        Ok(*self.matches < self.limit)
    }
    fn context(&mut self, _: &Searcher, context: &SinkContext<'_>) -> Result<bool, Self::Error> {
        self.line('-', context.line_number(), context.bytes());
        Ok(true)
    }
    fn context_break(&mut self, _: &Searcher) -> Result<bool, Self::Error> {
        self.output.push_str("--\n");
        Ok(true)
    }
}

/// In-process ripgrep-engine search that respects ignore files.
pub(crate) fn search(
    root: &Path,
    base: &Path,
    args: &Value,
    cancel: &CancellationToken,
) -> Result<Value> {
    let query = text(args, "query")?;
    ensure!(!query.is_empty(), "query is empty");
    let flag = |key| args.get(key).and_then(Value::as_bool).unwrap_or(false);
    let files_only = flag("files_only");
    let context = args
        .get("context")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(10) as usize;
    let limit = args
        .get("max_results")
        .and_then(Value::as_u64)
        .unwrap_or(200)
        .clamp(1, 2000) as usize;
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(flag("case_insensitive"))
        .fixed_strings(flag("literal"))
        .build(query)
        .context("Invalid regular expression; set literal to true to search exact text")?;
    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(0))
        .line_number(true)
        .before_context(if files_only { 0 } else { context })
        .after_context(if files_only { 0 } else { context })
        .build();
    let filter = glob_filter(args.get("glob").and_then(Value::as_str))?;
    let shown_root = Shown::new(root);
    let mut output = String::new();
    let (mut matches, mut files, mut visited) = (0_usize, 0_usize, 0_usize);
    let mut truncated = false;
    for entry in walker(base) {
        ensure!(!cancel.is_cancelled(), "Search cancelled");
        let Ok(entry) = entry else { continue };
        visited += 1;
        if visited > WALK_ENTRIES {
            truncated = true;
            break;
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file())
            || entry
                .metadata()
                .map_or(true, |m| m.len() > SEARCH_FILE_BYTES)
            || filter
                .as_ref()
                .is_some_and(|set| !set.is_match(relative(base, entry.path())))
        {
            continue;
        }
        let shown = shown_root.show(entry.path());
        let mut sink = Collect {
            shown: &shown,
            output: &mut output,
            matches: &mut matches,
            limit,
            files_only,
            matched_file: false,
        };
        if searcher
            .search_path(&matcher, entry.path(), &mut sink)
            .is_err()
        {
            continue;
        }
        if sink.matched_file {
            files += 1;
            if files_only {
                output.push_str(&shown);
                output.push('\n');
            }
        }
        if (files_only && files >= limit) || (!files_only && matches >= limit) {
            truncated = true;
            break;
        }
    }
    Ok(json!({
        "matches": if files_only { files } else { matches },
        "files": files,
        "truncated": truncated,
        "results": if output.is_empty() { "No matches".to_owned() } else { output },
    }))
}

/// Glob discovery (newest first) that respects ignore files.
pub(crate) fn find(
    root: &Path,
    base: &Path,
    args: &Value,
    cancel: &CancellationToken,
) -> Result<Value> {
    let pattern = text(args, "pattern")?.trim().replace('\\', "/");
    ensure!(!pattern.is_empty(), "pattern is empty");
    let pattern = if pattern.contains('/') {
        pattern
    } else {
        format!("**/{pattern}")
    };
    let matcher = globset::GlobBuilder::new(&pattern)
        .literal_separator(true)
        .build()
        .context("Invalid glob pattern")?
        .compile_matcher();
    let limit = args
        .get("max_results")
        .and_then(Value::as_u64)
        .unwrap_or(300)
        .clamp(1, 5000) as usize;
    let shown_root = Shown::new(root);
    let mut found = Vec::new();
    let mut visited = 0_usize;
    for entry in walker(base) {
        ensure!(!cancel.is_cancelled(), "File search cancelled");
        let Ok(entry) = entry else { continue };
        visited += 1;
        if visited > WALK_ENTRIES {
            break;
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if matcher.is_match(relative(base, entry.path())) {
            let modified = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            found.push((modified, shown_root.show(entry.path())));
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let total = found.len();
    let files: Vec<String> = found
        .into_iter()
        .take(limit)
        .map(|(_, path)| path)
        .collect();
    Ok(json!({
        "files": files,
        "count": total,
        "truncated": total > limit || visited > WALK_ENTRIES,
    }))
}

/// Directory listing with a trailing slash on directories.
pub(crate) fn list(path: &Path) -> Result<Value> {
    let mut entries = Vec::new();
    let mut truncated = false;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        if entries.len() >= 2000 {
            truncated = true;
            break;
        }
        let mut name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            name.push('/');
        }
        entries.push(name);
    }
    entries.sort_unstable();
    Ok(json!({"entries": entries, "truncated": truncated}))
}

/// Resolve a program like the platform shell would, including Windows
/// `PATHEXT` launchers such as `npm.cmd`. A missing program is definitive.
pub(crate) fn resolve_program(program: &str, cwd: &Path) -> Result<PathBuf> {
    let requested = Path::new(program);
    ensure!(!program.trim().is_empty(), "program is empty");
    let candidates: Vec<PathBuf> = if requested.is_absolute() {
        vec![requested.to_path_buf()]
    } else if requested.components().count() > 1 {
        vec![cwd.join(requested)]
    } else {
        std::env::var_os("PATH")
            .map(|paths| {
                std::env::split_paths(&paths)
                    .map(|dir| dir.join(requested))
                    .collect()
            })
            .unwrap_or_default()
    };
    candidates
        .iter()
        .find_map(|candidate| executable(candidate))
        .ok_or_else(|| {
            unapplied(format!(
                "Program `{program}` was not found on PATH; nothing was run"
            ))
        })
}

#[cfg(windows)]
fn executable(path: &Path) -> Option<PathBuf> {
    if path.extension().is_some() && path.is_file() {
        return Some(path.to_path_buf());
    }
    let extensions = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned());
    extensions
        .split(';')
        .filter(|extension| !extension.is_empty())
        .map(|extension| {
            let mut candidate = path.as_os_str().to_owned();
            candidate.push(extension);
            PathBuf::from(candidate)
        })
        .find(|candidate| candidate.is_file())
}

#[cfg(not(windows))]
fn executable(path: &Path) -> Option<PathBuf> {
    path.is_file().then(|| path.to_path_buf())
}

/// The platform shell invocation for a model-written command line.
pub(crate) fn shell_invocation(command: &str, cwd: &Path) -> Result<(PathBuf, Vec<String>)> {
    #[cfg(windows)]
    {
        let script = format!(
            "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; $ErrorActionPreference='Continue'; & {{ {command} }}; if ($LASTEXITCODE) {{ exit $LASTEXITCODE }} elseif (-not $?) {{ exit 1 }}"
        );
        Ok((
            resolve_program("powershell", cwd)?,
            vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                script,
            ],
        ))
    }
    #[cfg(not(windows))]
    {
        let shell = resolve_program("bash", cwd).or_else(|_| resolve_program("sh", cwd))?;
        Ok((shell, vec!["-c".into(), command.to_owned()]))
    }
}

/// Human name of the shell `shell_invocation` uses.
pub(crate) fn shell_name() -> &'static str {
    if cfg!(windows) {
        "PowerShell (Windows PowerShell 5.1 syntax)"
    } else {
        "bash (falls back to sh)"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranged_reads_number_lines_and_report_continuation() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        let body: String = (1..=50).map(|n| format!("line {n}\n")).collect();
        std::fs::write(&file, body).unwrap();
        let first = read(&file, "a.txt", &json!({"limit": 10})).unwrap();
        assert_eq!(first["end_line"], 10);
        assert_eq!(first["total_lines"], 50);
        assert_eq!(first["next_offset"], 11);
        assert!(
            first["content"]
                .as_str()
                .unwrap()
                .starts_with("     1\tline 1\n")
        );
        let tail = read(&file, "a.txt", &json!({"offset": 45})).unwrap();
        assert_eq!(tail["start_line"], 45);
        assert_eq!(tail["truncated"], false);
        assert!(read(&file, "a.txt", &json!({"offset": 51})).is_err());
    }

    #[test]
    fn large_reads_stop_on_the_byte_budget() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("big.txt");
        std::fs::write(&file, format!("{}\n", "x".repeat(100)).repeat(5000)).unwrap();
        let value = read(&file, "big.txt", &json!({})).unwrap();
        assert!(value["content"].as_str().unwrap().len() <= READ_BYTES);
        assert_eq!(value["truncated"], true);
    }

    #[test]
    fn edits_require_a_unique_exact_match() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "fn a() {}\nfn b() {}\nfn a() {}\n").unwrap();
        let error = plan_edit(
            &file,
            "a.rs",
            &json!({"old_string":"fn a() {}","new_string":"x"}),
        )
        .err()
        .unwrap();
        assert!(error.downcast_ref::<Unapplied>().is_some());
        assert!(error.to_string().contains("2 places"));
        let all = plan_edit(
            &file,
            "a.rs",
            &json!({"old_string":"fn a() {}","new_string":"fn c() {}","replace_all":true}),
        )
        .unwrap();
        assert_eq!(all.replacements, 2);
        assert_eq!(all.updated, "fn c() {}\nfn b() {}\nfn c() {}\n");
        let missing = plan_edit(
            &file,
            "a.rs",
            &json!({"old_string":"nope","new_string":"x"}),
        );
        assert!(missing.err().unwrap().downcast_ref::<Unapplied>().is_some());
    }

    #[test]
    fn edits_keep_crlf_line_endings() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("w.txt");
        std::fs::write(&file, "one\r\ntwo\r\nthree\r\n").unwrap();
        let plan = plan_edit(
            &file,
            "w.txt",
            &json!({"old_string":"one\ntwo","new_string":"uno\ndos"}),
        )
        .unwrap();
        assert_eq!(plan.updated, "uno\r\ndos\r\nthree\r\n");
    }

    #[test]
    fn search_and_find_respect_ignore_files_and_filters() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn needle() {}\n").unwrap();
        std::fs::write(root.join("src/notes.md"), "needle\n").unwrap();
        std::fs::write(root.join("target/out.rs"), "fn needle() {}\n").unwrap();
        let cancel = CancellationToken::new();
        let hits = search(
            root,
            root,
            &json!({"query":"needle","glob":"*.rs"}),
            &cancel,
        )
        .unwrap();
        assert_eq!(hits["matches"], 1);
        assert!(hits["results"].as_str().unwrap().contains("src/lib.rs:1:"));
        let files = find(root, root, &json!({"pattern":"*.rs"}), &cancel).unwrap();
        assert_eq!(files["files"], json!(["src/lib.rs"]));
        let none = search(root, root, &json!({"query":"absent"}), &cancel).unwrap();
        assert_eq!(none["results"], "No matches");
    }

    #[test]
    fn missing_programs_are_definitive() {
        let dir = tempfile::tempdir().unwrap();
        let error = resolve_program("definitely-not-a-real-program-xyz", dir.path())
            .err()
            .unwrap();
        assert!(error.downcast_ref::<Unapplied>().is_some());
    }

    #[test]
    fn diffs_count_changes() {
        let value = diff("a.txt", "a\nb\nc\n", "a\nB\nc\nd\n");
        assert_eq!(value["added"], 2);
        assert_eq!(value["removed"], 1);
        assert!(value["diff"].as_str().unwrap().contains("+B"));
    }
}
