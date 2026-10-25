use super::{index, paths};
use anyhow::{Result, ensure};
use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

/// Resume by native path and input position, rather than a project-sized vector
/// offset. `line == u64::MAX` means the named file has been fully examined.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchCursor {
    pub revision: u64,
    pub path: PathBuf,
    pub line: u64,
    #[serde(default)]
    pub byte_offset: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchOptions {
    pub query: String,
    pub case_sensitive: bool,
    pub regular_expression: bool,
    pub after: Option<SearchCursor>,
    pub limit: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchHit {
    pub path: PathBuf,
    pub line: u64,
    pub byte_column: usize,
    pub preview: String,
    pub match_bytes: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchPage {
    pub root: PathBuf,
    pub query: String,
    pub hits: Vec<SearchHit>,
    pub next: Option<SearchCursor>,
    pub skipped_files: usize,
    pub timed_out: bool,
    /// Initial/reconciliation indexing can still discover more paths.
    pub indexing: bool,
}
pub(super) fn search(
    database: &Path,
    root: &Path,
    options: SearchOptions,
    cancel: &CancellationToken,
    project: &CancellationToken,
) -> Result<SearchPage> {
    search_for(
        database,
        root,
        options,
        cancel,
        project,
        Duration::from_secs(30),
    )
}
pub(super) fn search_for(
    database: &Path,
    root: &Path,
    options: SearchOptions,
    cancel: &CancellationToken,
    project: &CancellationToken,
    budget: Duration,
) -> Result<SearchPage> {
    index::check(cancel, project)?;
    ensure!(
        !options.query.trim().is_empty() && options.query.len() <= 4096,
        "Enter a search up to 4,096 bytes"
    );
    let conn = index::open_reader(database)?;
    let meta = index::metadata(&conn, root, &index::Delta::default())?;
    if let Some(cursor) = &options.after {
        ensure!(
            cursor.revision == meta.revision && cursor.path.starts_with(root),
            "The project changed; search again"
        );
    }
    let matcher = RegexMatcherBuilder::new()
        .fixed_strings(!options.regular_expression)
        .case_insensitive(!options.case_sensitive)
        .size_limit(4 * 1024 * 1024)
        .dfa_size_limit(4 * 1024 * 1024)
        .build(&options.query)?;
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .passthru(true)
        .heap_limit(Some(2 * 1024 * 1024))
        .binary_detection(BinaryDetection::quit(0))
        .build();
    let limit = options.limit.clamp(1, 500);
    let started = Instant::now();
    let mut hits = Vec::with_capacity(limit);
    let mut skipped_files = 0;
    let mut next = None;
    let mut timed_out = false;
    let mut position = options.after.clone();
    let reader = super::reader::RootReader::open(root, cancel, project)?;
    'pages: loop {
        index::check(cancel, project)?;
        let after = position
            .as_ref()
            .map(|cursor| paths::encode(&cursor.path))
            .unwrap_or_default();
        let inclusive = position
            .as_ref()
            .is_some_and(|cursor| cursor.line != u64::MAX);
        // Each query releases its read snapshot before file I/O, so an active
        // search cannot retain an entire scan's WAL history.
        let keys: Vec<Vec<u8>> = {
            let sql = if inclusive {
                "SELECT path FROM entries WHERE kind=0 AND path>=?1 ORDER BY path LIMIT 128"
            } else {
                "SELECT path FROM entries WHERE kind=0 AND path>?1 ORDER BY path LIMIT 128"
            };
            let mut query = conn.prepare(sql)?;
            query
                .query_map(params![after], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        if keys.is_empty() {
            break;
        }
        for key in keys {
            index::check(cancel, project)?;
            let path = paths::decode(key)?;
            let resume = position
                .as_ref()
                .filter(|cursor| cursor.path == path && cursor.line != u64::MAX);
            let base_line = resume.map_or(0, |cursor| cursor.line);
            let base_offset = resume.map_or(0, |cursor| cursor.byte_offset);
            let before_file = SearchCursor {
                revision: meta.revision,
                path: path.clone(),
                line: base_line,
                byte_offset: base_offset,
            };
            if started.elapsed() >= budget {
                timed_out = true;
                next = Some(before_file);
                break 'pages;
            }
            if let Ok(mut file) = reader.file(&path, cancel, project) {
                if file.seek(SeekFrom::Start(base_offset)).is_err() {
                    skipped_files += 1;
                } else {
                    let mut sink = PageSink {
                        matcher: &matcher,
                        path: &path,
                        revision: meta.revision,
                        limit,
                        hits: &mut hits,
                        position: before_file,
                        base_line,
                        base_offset,
                        started,
                        budget,
                        cancel,
                        project,
                        more: false,
                        timed_out: false,
                    };
                    let reader = CancellableReader {
                        file,
                        cancel,
                        project,
                        started,
                        budget,
                    };
                    let result = searcher.search_reader(&matcher, reader, &mut sink);
                    if sink.more || sink.timed_out || started.elapsed() >= budget {
                        timed_out = sink.timed_out || started.elapsed() >= budget;
                        next = Some(sink.position);
                        break 'pages;
                    }
                    if result.is_err() {
                        skipped_files += 1;
                    }
                }
            } else {
                skipped_files += 1;
            }
            position = Some(SearchCursor {
                revision: meta.revision,
                path,
                line: u64::MAX,
                byte_offset: 0,
            });
        }
        ensure!(
            index::metadata(&conn, root, &index::Delta::default())?.revision == meta.revision,
            "The project changed; search again"
        );
    }
    index::check(cancel, project)?;
    ensure!(
        index::metadata(&conn, root, &index::Delta::default())?.revision == meta.revision,
        "The project changed; search again"
    );
    Ok(SearchPage {
        root: root.to_owned(),
        query: options.query,
        hits,
        next,
        skipped_files,
        timed_out,
        indexing: meta.indexing,
    })
}

struct PageSink<'a> {
    matcher: &'a RegexMatcher,
    path: &'a Path,
    revision: u64,
    limit: usize,
    hits: &'a mut Vec<SearchHit>,
    position: SearchCursor,
    base_line: u64,
    base_offset: u64,
    started: Instant,
    budget: Duration,
    cancel: &'a CancellationToken,
    project: &'a CancellationToken,
    more: bool,
    timed_out: bool,
}
impl PageSink<'_> {
    fn proceed(&mut self) -> bool {
        self.timed_out |= self.started.elapsed() >= self.budget;
        !self.timed_out && !self.cancel.is_cancelled() && !self.project.is_cancelled()
    }
    fn advance(&mut self, line: u64, offset: u64, bytes: &[u8]) {
        self.position = SearchCursor {
            revision: self.revision,
            path: self.path.to_owned(),
            line: self.base_line + line,
            byte_offset: self.base_offset + offset + bytes.len() as u64,
        };
    }
}
impl Sink for PageSink<'_> {
    type Error = std::io::Error;
    fn matched(&mut self, _: &Searcher, matched: &SinkMatch<'_>) -> std::io::Result<bool> {
        if !self.proceed() {
            return Ok(false);
        }
        if self.hits.len() == self.limit {
            self.more = true;
            return Ok(false);
        }
        let line = std::str::from_utf8(matched.bytes()).map_err(std::io::Error::other)?;
        let Some(found) = self
            .matcher
            .find(matched.bytes())
            .map_err(std::io::Error::other)?
        else {
            return Ok(true);
        };
        let mut from = found.start().saturating_sub(180);
        while !line.is_char_boundary(from) {
            from += 1;
        }
        let mut to = (from + 700).min(line.len());
        while !line.is_char_boundary(to) {
            to -= 1;
        }
        let number = matched.line_number().expect("Line numbering enabled");
        self.hits.push(SearchHit {
            path: self.path.to_owned(),
            line: self.base_line + number,
            byte_column: found.start(),
            preview: line[from..to].trim_end_matches(['\r', '\n']).to_owned(),
            match_bytes: found.len(),
        });
        self.advance(number, matched.absolute_byte_offset(), matched.bytes());
        Ok(true)
    }
    fn context(&mut self, _: &Searcher, context: &SinkContext<'_>) -> std::io::Result<bool> {
        if !self.proceed() {
            return Ok(false);
        }
        self.advance(
            context.line_number().expect("Line numbering enabled"),
            context.absolute_byte_offset(),
            context.bytes(),
        );
        Ok(true)
    }
}
struct CancellableReader<'a> {
    file: std::fs::File,
    cancel: &'a CancellationToken,
    project: &'a CancellationToken,
    started: Instant,
    budget: Duration,
}
impl Read for CancellableReader<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        if self.cancel.is_cancelled() || self.project.is_cancelled() {
            return Err(std::io::Error::other("Search cancelled"));
        }
        if self.started.elapsed() >= self.budget {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Search timed out",
            ));
        }
        self.file.read(bytes)
    }
}
