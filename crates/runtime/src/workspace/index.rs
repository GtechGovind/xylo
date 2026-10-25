//! A private, rebuildable SQLite catalogue. Short writer transactions and small
//! independent reader caches keep traversal away from the durable app database.
use super::{FileIndex, paths, rules::Visibility};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;

pub(super) const BATCH: usize = 512;
const DELTA: usize = 256;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileCursor {
    pub revision: u64,
    pub path: PathBuf,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileOptions {
    pub directory: PathBuf,
    pub after: Option<FileCursor>,
    pub limit: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: PathBuf,
    pub is_dir: bool,
    pub bytes: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FilePage {
    pub root: PathBuf,
    pub directory: PathBuf,
    pub revision: u64,
    pub entries: Vec<FileEntry>,
    pub next: Option<FileCursor>,
    pub indexing: bool,
    pub file_count: usize,
    pub directory_count: usize,
    pub unreadable: usize,
}
#[derive(Default)]
pub(super) struct Delta {
    pub changed: Vec<PathBuf>,
    pub removed: Vec<PathBuf>,
    pub incomplete: bool,
    pub count: u64,
}
impl Delta {
    pub fn changed(&mut self, path: PathBuf) {
        self.count += 1;
        if self.changed.len() + self.removed.len() < DELTA {
            self.changed.push(path);
        } else {
            self.incomplete = true;
        }
    }
    fn removed(&mut self, path: PathBuf) {
        self.count += 1;
        if self.changed.len() + self.removed.len() < DELTA {
            self.removed.push(path);
        } else {
            self.incomplete = true;
        }
    }
    pub fn any(&self) -> bool {
        self.incomplete || !self.changed.is_empty() || !self.removed.is_empty()
    }
}
pub(super) struct Entry {
    pub path: PathBuf,
    kind: i64,
    bytes: i64,
    modified: Option<Vec<u8>>,
}
impl Entry {
    pub fn read(path: &Path, root: &Path) -> Result<Option<Self>> {
        let symlink = std::fs::symlink_metadata(path)?;
        if symlink.file_type().is_symlink() {
            let target = std::fs::canonicalize(path)?;
            if !target.starts_with(root) || !target.is_file() {
                return Ok(None);
            }
        } else if !symlink.is_dir() && !symlink.is_file() {
            return Ok(None);
        }
        let metadata = std::fs::metadata(path)?;
        Ok(Some(Self {
            path: path.to_owned(),
            kind: i64::from(metadata.is_dir()),
            bytes: i64::try_from(metadata.len()).unwrap_or(i64::MAX),
            modified: metadata.modified().ok().map(timestamp),
        }))
    }
}
fn timestamp(time: SystemTime) -> Vec<u8> {
    let value = match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos() as i128,
        Err(error) => -(error.duration().as_nanos() as i128),
    };
    value.to_be_bytes().to_vec()
}
pub(super) fn open_writer(database: &Path, root: &Path) -> Result<Connection> {
    let directory = database
        .parent()
        .context("Project cache location is missing")?;
    std::fs::create_dir_all(directory)?;
    ensure!(
        !std::fs::symlink_metadata(directory)?
            .file_type()
            .is_symlink(),
        "Project cache must be a private folder"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    }
    if let Ok(metadata) = std::fs::symlink_metadata(database) {
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "Project cache is not a regular file"
        );
    }
    let mut conn = Connection::open(database)?;
    conn.busy_timeout(Duration::from_secs(2))?;
    let fresh = check_format(&conn)?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA cache_size=-4096; PRAGMA mmap_size=0; PRAGMA temp_store=FILE; PRAGMA wal_autocheckpoint=1000; PRAGMA journal_size_limit=8388608;")?;
    let tx = conn.transaction()?;
    tx.execute_batch("
        CREATE TABLE IF NOT EXISTS meta(id INTEGER PRIMARY KEY CHECK(id=1),root BLOB NOT NULL,revision INTEGER NOT NULL DEFAULT 0,epoch INTEGER NOT NULL DEFAULT 0,files INTEGER NOT NULL DEFAULT 0,dirs INTEGER NOT NULL DEFAULT 0,unreadable INTEGER NOT NULL DEFAULT 0,indexing INTEGER NOT NULL DEFAULT 1,watching INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE IF NOT EXISTS entries(path BLOB PRIMARY KEY,parent BLOB NOT NULL,kind INTEGER NOT NULL,bytes INTEGER NOT NULL,modified BLOB,seen INTEGER NOT NULL,seeded INTEGER NOT NULL DEFAULT 0) WITHOUT ROWID;
        CREATE INDEX IF NOT EXISTS entries_parent ON entries(parent,path);
        CREATE INDEX IF NOT EXISTS entries_parent_kind ON entries(parent,kind,path);
        CREATE INDEX IF NOT EXISTS entries_kind ON entries(kind,path);
        CREATE INDEX IF NOT EXISTS entries_seen ON entries(seen,path);
        CREATE INDEX IF NOT EXISTS entries_frontier ON entries(seen,kind,seeded,path);
        CREATE TRIGGER IF NOT EXISTS entry_insert AFTER INSERT ON entries BEGIN UPDATE meta SET files=files+(NEW.kind=0),dirs=dirs+(NEW.kind=1) WHERE id=1; END;
        CREATE TRIGGER IF NOT EXISTS entry_delete AFTER DELETE ON entries BEGIN UPDATE meta SET files=files-(OLD.kind=0),dirs=dirs-(OLD.kind=1) WHERE id=1; END;
        CREATE TRIGGER IF NOT EXISTS entry_kind AFTER UPDATE OF kind ON entries WHEN OLD.kind!=NEW.kind BEGIN UPDATE meta SET files=files+(NEW.kind=0)-(OLD.kind=0),dirs=dirs+(NEW.kind=1)-(OLD.kind=1) WHERE id=1; END;")?;
    tx.execute(
        "INSERT OR IGNORE INTO meta(id,root) VALUES(1,?1)",
        [paths::encode(root)],
    )?;
    let stored: Vec<u8> = tx.query_row("SELECT root FROM meta WHERE id=1", [], |row| row.get(0))?;
    ensure!(
        stored == paths::encode(root),
        "Project cache belongs to another folder"
    );
    if fresh {
        tx.pragma_update(None, "user_version", paths::FORMAT_VERSION)?;
    }
    tx.commit()?;
    Ok(conn)
}
/// Inspect before any schema/journal mutation. Empty version-zero files are the
/// only resumable initialization state; an unknown populated format fails
/// closed instead of guessing a migration or deleting the user's old cache.
fn check_format(conn: &Connection) -> Result<bool> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let tables: Vec<String> = {
        let mut query=conn.prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")?;
        query
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    if version == 0 && tables.is_empty() {
        return Ok(true);
    }
    ensure!(
        version == paths::FORMAT_VERSION,
        "This project catalogue format is unsupported; its saved cache has been preserved"
    );
    ensure!(
        tables == ["entries", "meta"],
        "This project catalogue layout is unsupported; its saved cache has been preserved"
    );
    for (sql, expected) in [
        (
            "PRAGMA table_info(meta)",
            &[
                "id",
                "root",
                "revision",
                "epoch",
                "files",
                "dirs",
                "unreadable",
                "indexing",
                "watching",
            ][..],
        ),
        (
            "PRAGMA table_info(entries)",
            &[
                "path", "parent", "kind", "bytes", "modified", "seen", "seeded",
            ][..],
        ),
    ] {
        let mut query = conn.prepare(sql)?;
        let columns = query
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(
            columns == expected,
            "This project catalogue layout is unsupported; its saved cache has been preserved"
        );
    }
    Ok(false)
}
pub(super) fn open_reader(database: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .context("Project files are still loading")?;
    conn.busy_timeout(Duration::from_secs(2))?;
    ensure!(!check_format(&conn)?, "Project files are still loading");
    conn.execute_batch("PRAGMA cache_size=-2048; PRAGMA mmap_size=0; PRAGMA temp_store=FILE; PRAGMA query_only=ON;")?;
    Ok(conn)
}
fn open_existing_writer(database: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .context("Project files are still loading")?;
    conn.busy_timeout(Duration::from_secs(2))?;
    ensure!(!check_format(&conn)?, "Project files are still loading");
    conn.execute_batch("PRAGMA cache_size=-2048; PRAGMA mmap_size=0; PRAGMA temp_store=FILE;")?;
    Ok(conn)
}
pub(super) fn metadata(conn: &Connection, root: &Path, delta: &Delta) -> Result<FileIndex> {
    conn.query_row(
        "SELECT revision,files,dirs,unreadable,indexing,watching FROM meta WHERE id=1",
        [],
        |row| {
            Ok(FileIndex {
                root: root.to_owned(),
                revision: row.get(0)?,
                file_count: row.get(1)?,
                directory_count: row.get(2)?,
                unreadable: row.get(3)?,
                indexing: row.get(4)?,
                changed: delta.changed.clone(),
                removed: delta.removed.clone(),
                changes_complete: !delta.incomplete,
                watching: row.get(5)?,
            })
        },
    )
    .map_err(Into::into)
}
pub(super) fn begin_scan(conn: &Connection) -> Result<i64> {
    conn.execute(
        "UPDATE meta SET epoch=epoch+1,revision=revision+1,indexing=1,unreadable=0 WHERE id=1",
        [],
    )?;
    Ok(conn.query_row("SELECT epoch FROM meta WHERE id=1", [], |row| row.get(0))?)
}
pub(super) fn epoch(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT epoch FROM meta WHERE id=1", [], |row| row.get(0))?)
}
pub(super) fn upsert(
    conn: &mut Connection,
    entries: &[Entry],
    epoch: i64,
    delta: &mut Delta,
    stop: &CancellationToken,
) -> Result<()> {
    let before = delta.count;
    let tx = conn.transaction()?;
    ensure!(
        self::epoch(&tx)? == epoch,
        "The project changed; open this folder again"
    );
    {
        let mut prior =
            tx.prepare_cached("SELECT kind,bytes,modified FROM entries WHERE path=?1")?;
        let mut write = tx.prepare_cached("INSERT INTO entries(path,parent,kind,bytes,modified,seen) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(path) DO UPDATE SET kind=excluded.kind,bytes=excluded.bytes,modified=excluded.modified,seen=excluded.seen WHERE kind!=excluded.kind OR bytes!=excluded.bytes OR modified IS NOT excluded.modified OR seen!=excluded.seen")?;
        for entry in entries {
            ensure!(!stop.is_cancelled(), "Project refresh cancelled");
            let key = paths::encode(&entry.path);
            let old: Option<(i64, i64, Option<Vec<u8>>)> = prior
                .query_row([&key], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .optional()?;
            if old.as_ref() != Some(&(entry.kind, entry.bytes, entry.modified.clone())) {
                delta.changed(entry.path.clone());
            }
            write.execute(params![
                key,
                paths::encode(entry.path.parent().context("Project file has no parent")?),
                entry.kind,
                entry.bytes,
                entry.modified,
                epoch
            ])?;
        }
    }
    if delta.count != before {
        tx.execute(
            "UPDATE meta SET revision=revision+1 WHERE id=1 AND indexing=0",
            [],
        )?;
    }
    tx.commit()?;
    Ok(())
}
/// Chunk deletion keeps one large rename/reconciliation from monopolising the
/// writer or allocating the removed subtree in memory.
pub(super) fn remove(
    conn: &mut Connection,
    path: Option<&Path>,
    stale: Option<i64>,
    delta: &mut Delta,
    stop: &CancellationToken,
) -> Result<()> {
    let (exact, low, high) = path
        .map(|path| {
            let (low, high) = paths::descendants(path);
            (paths::encode(path), low, high)
        })
        .unwrap_or_default();
    if path.is_some() {
        ensure!(stale.is_none(), "Invalid project catalogue deletion");
        let tx = conn.transaction()?;
        if tx.execute("DELETE FROM entries WHERE path=?1", [&exact])? != 0 {
            delta.removed(paths::decode(exact)?);
            tx.execute(
                "UPDATE meta SET revision=revision+1 WHERE id=1 AND indexing=0",
                [],
            )?;
        }
        tx.commit()?;
    }
    loop {
        ensure!(!stop.is_cancelled(), "Project refresh cancelled");
        let tx = conn.transaction()?;
        let keys: Vec<Vec<u8>> = {
            if let Some(epoch) = stale {
                let mut query = tx.prepare(
                    "SELECT path FROM entries WHERE seen<?1 ORDER BY seen,path LIMIT 512",
                )?;
                query
                    .query_map([epoch], |row| row.get(0))?
                    .collect::<rusqlite::Result<_>>()?
            } else {
                let mut query = tx.prepare(
                    "SELECT path FROM entries WHERE path>=?1 AND path<?2 ORDER BY path LIMIT 512",
                )?;
                query
                    .query_map(params![low, high], |row| row.get(0))?
                    .collect::<rusqlite::Result<_>>()?
            }
        };
        if keys.is_empty() {
            break;
        }
        for key in &keys {
            delta.removed(paths::decode(key.clone())?);
            tx.execute("DELETE FROM entries WHERE path=?1", [key])?;
        }
        tx.execute(
            "UPDATE meta SET revision=revision+1 WHERE id=1 AND indexing=0",
            [],
        )?;
        tx.commit()?;
    }
    Ok(())
}
pub(super) fn check(cancel: &CancellationToken, project: &CancellationToken) -> Result<()> {
    ensure!(!cancel.is_cancelled(), "Search cancelled");
    ensure!(!project.is_cancelled(), "The project changed; try again");
    Ok(())
}
/// Directory frontier is persisted in the same bounded-cache catalogue. The
/// walker opens only one directory at a time; wide ancestors are never evacuated
/// into WalkDir's project-sized in-memory vectors.
pub(super) fn next_directory(
    conn: &Connection,
    current: i64,
    scope: Option<&Path>,
) -> Result<Option<PathBuf>> {
    let key: Option<Vec<u8>> = if let Some(scope) = scope {
        let (low, high) = paths::descendants(scope);
        conn.query_row("SELECT path FROM entries WHERE seen=?1 AND kind=1 AND seeded<?1 AND path>=?2 AND path<?3 ORDER BY seeded,path LIMIT 1", params![current,low,high], |row| row.get(0)).optional()?
    } else {
        conn.query_row("SELECT path FROM entries WHERE seen=?1 AND kind=1 AND seeded<?1 ORDER BY seeded,path LIMIT 1", [current], |row| row.get(0)).optional()?
    };
    key.map(paths::decode).transpose()
}
/// Keyset through a replaced subtree once. Repeating a filter from its first
/// key for every delete batch would make a large unchanged prefix quadratic.
pub(super) fn prune_scope(
    conn: &mut Connection,
    scope: &Path,
    current: i64,
    delta: &mut Delta,
    stop: &CancellationToken,
) -> Result<()> {
    let (low, high) = paths::descendants(scope);
    let mut after = low;
    loop {
        ensure!(!stop.is_cancelled(), "Project refresh cancelled");
        let tx = conn.transaction()?;
        let rows: Vec<(Vec<u8>, i64)> = {
            let mut query = tx.prepare(
                "SELECT path,seen FROM entries WHERE path>=?1 AND path<?2 ORDER BY path LIMIT 512",
            )?;
            query
                .query_map(params![after, high], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        if rows.is_empty() {
            break;
        }
        for (key, seen) in &rows {
            if *seen < current {
                tx.execute("DELETE FROM entries WHERE path=?1", [key])?;
                delta.removed(paths::decode(key.clone())?);
            }
        }
        // No valid path can contain NUL; appending one advances strictly beyond
        // this complete native key without skipping any valid subsequent key.
        after = rows.last().expect("Nonempty scan batch").0.clone();
        after.push(0);
        tx.commit()?;
    }
    Ok(())
}
/// One folder may be requested before the background walker reaches it. Both
/// paths participate in the same epoch and persisted directory frontier.
pub(super) struct ScanContext<'a> {
    pub root: &'a Path,
    pub cache: &'a Path,
    pub cancel: &'a CancellationToken,
    pub stop: &'a CancellationToken,
}
impl ScanContext<'_> {
    pub fn seed(
        &self,
        conn: &mut Connection,
        directory: &Path,
        delta: &mut Delta,
        mut progress: impl FnMut(&Connection, &Delta) -> Result<()>,
    ) -> Result<usize> {
        let current = epoch(conn)?;
        let mut unreadable = 0;
        let mut batch = Vec::with_capacity(BATCH);
        if directory != self.root {
            match Entry::read(directory, self.root) {
                Ok(Some(entry)) => batch.push(entry),
                _ => unreadable += 1,
            }
        }
        for entry in super::monitor::walker(directory, self.cache).build() {
            check(self.cancel, self.stop)?;
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    unreadable += 1;
                    continue;
                }
            };
            if entry.path() == directory {
                continue;
            }
            match Entry::read(entry.path(), self.root) {
                Ok(Some(entry)) => batch.push(entry),
                Ok(None) => {}
                Err(_) => unreadable += 1,
            }
            if batch.len() == BATCH {
                upsert(conn, &batch, current, delta, self.stop)?;
                batch.clear();
                progress(conn, delta)?;
            }
        }
        upsert(conn, &batch, current, delta, self.stop)?;
        let tx = conn.transaction()?;
        ensure!(
            epoch(&tx)? == current,
            "The project changed; open this folder again"
        );
        tx.execute(
            "UPDATE entries SET seeded=?2 WHERE path=?1",
            params![paths::encode(directory), current],
        )?;
        tx.commit()?;
        Ok(unreadable)
    }
}
pub(super) fn files(
    database: &Path,
    root: &Path,
    options: FileOptions,
    cancel: &CancellationToken,
    project: &CancellationToken,
) -> Result<FilePage> {
    check(cancel, project)?;
    let directory =
        std::fs::canonicalize(&options.directory).context("Folder is no longer available")?;
    ensure!(
        directory.starts_with(root) && directory.is_dir(),
        "Choose a folder in this project"
    );
    let cache = database.parent().context("Project cache is missing")?;
    ensure!(
        !directory.starts_with(cache),
        "This folder contains the project cache"
    );
    let mut conn = open_existing_writer(database)?;
    let meta = metadata(&conn, root, &Delta::default())?;
    if meta.indexing {
        let current = epoch(&conn)?;
        let seeded: Option<i64> = conn
            .query_row(
                "SELECT seeded FROM entries WHERE path=?1 AND kind=1",
                [paths::encode(&directory)],
                |row| row.get(0),
            )
            .optional()?;
        if directory != root && seeded != Some(current) {
            ensure!(
                Visibility::new(root).visible(&directory, true),
                "This folder is excluded by the project ignore rules"
            );
            ScanContext {
                root,
                cache,
                cancel,
                stop: project,
            }
            .seed(&mut conn, &directory, &mut Delta::default(), |_, _| Ok(()))?;
        }
    }
    let tx = conn.transaction()?;
    let meta = metadata(&tx, root, &Delta::default())?;
    if let Some(cursor) = &options.after {
        ensure!(
            cursor.revision == meta.revision && cursor.path.parent() == Some(directory.as_path()),
            "The project changed; open this folder again"
        );
    }
    let after = options
        .after
        .as_ref()
        .map(|cursor| paths::encode(&cursor.path))
        .unwrap_or_default();
    // The revision and parent were checked inside this transaction. Resolve
    // the cursor's kind through its primary key, preserving the public opaque
    // cursor while giving directory and file pages a stable folders-first order.
    let after_kind: i64 = if options.after.is_some() {
        tx.query_row(
            "SELECT kind FROM entries WHERE path=?1 AND parent=?2",
            params![after, paths::encode(&directory)],
            |row| row.get(0),
        )
        .optional()?
        .context("The project changed; open this folder again")?
    } else {
        1
    };
    ensure!(
        matches!(after_kind, 0 | 1),
        "Project catalogue entry kind is invalid"
    );
    let limit = options.limit.clamp(1, 500);
    let rows: Vec<(Vec<u8>, bool, u64)> = {
        // At most two indexed seeks, retaining only limit+1 entries. Avoid an
        // OFFSET or mixed-direction OR predicate which revisits earlier pages.
        let mut rows = Vec::with_capacity(limit + 1);
        let mut query = tx.prepare("SELECT path,kind,bytes FROM entries WHERE parent=?1 AND kind=?2 AND path>?3 ORDER BY path LIMIT ?4")?;
        for kind in (0..=after_kind).rev() {
            check(cancel, project)?;
            let path = if kind == after_kind {
                after.as_slice()
            } else {
                &[]
            };
            let page = query.query_map(
                params![
                    paths::encode(&directory),
                    kind,
                    path,
                    limit + 1 - rows.len()
                ],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            for entry in page {
                rows.push(entry?);
            }
            if rows.len() > limit {
                break;
            }
        }
        rows
    };
    let more = rows.len() > limit;
    let mut entries = Vec::with_capacity(limit.min(rows.len()));
    for (path, is_dir, bytes) in rows.into_iter().take(limit) {
        check(cancel, project)?;
        entries.push(FileEntry {
            path: paths::decode(path)?,
            is_dir,
            bytes,
        });
    }
    let next = more.then(|| FileCursor {
        revision: meta.revision,
        path: entries
            .last()
            .expect("Nonempty page with continuation")
            .path
            .clone(),
    });
    Ok(FilePage {
        root: root.to_owned(),
        directory,
        revision: meta.revision,
        entries,
        next,
        indexing: meta.indexing,
        file_count: meta.file_count,
        directory_count: meta.directory_count,
        unreadable: meta.unreadable,
    })
}
