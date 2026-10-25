//! Deterministic regressions from the independent catalogue review. Native
//! event bursts are coalesced through the production queue; no watcher timing,
//! external Git command, memory-pressure assumption or source-text check is used.
use super::*;
use crate::workspace::paths;
use notify::event::{CreateKind, RemoveKind};
use rusqlite::params;

struct Fixture {
    // Connections close before their temporary directories on every platform.
    job: Job,
    database: PathBuf,
    _incoming: async_channel::Receiver<WorkspaceEvent>,
    _project: tempfile::TempDir,
    _cache: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let project = tempfile::tempdir().unwrap();
        let root = project.path().canonicalize().unwrap();
        let cache = tempfile::tempdir().unwrap();
        Self::at(project, cache, root)
    }

    fn at(project: tempfile::TempDir, cache: tempfile::TempDir, root: PathBuf) -> Self {
        let cache_path = cache.path().canonicalize().unwrap();
        let database = cache_path.join(paths::database_name(&root));
        let (events, incoming) = async_channel::bounded(2);
        let job = Job {
            conn: index::open_writer(&database, &root).unwrap(),
            root: root.clone(),
            cache: cache_path,
            progress: Progress {
                root,
                latest: Arc::new(RwLock::new(None)),
                events,
                last: Instant::now(),
            },
            stop: CancellationToken::new(),
        };
        Self {
            job,
            database,
            _incoming: incoming,
            _project: project,
            _cache: cache,
        }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.job.root.join(relative)
    }

    fn contains(&self, relative: &str) -> bool {
        self.job
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM entries WHERE path=?1)",
                [paths::encode(&self.path(relative))],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn counts(&self) -> (usize, usize) {
        let snapshot = index::metadata(&self.job.conn, &self.job.root, &Delta::default()).unwrap();
        assert!(!snapshot.indexing);
        assert_eq!(snapshot.unreadable, 0);
        (snapshot.file_count, snapshot.directory_count)
    }

    fn coalesced_replacement(&mut self, relative: &str, created: CreateKind) {
        let path = self.path(relative);
        let dirty = DirtyPaths::new();
        for kind in [
            EventKind::Remove(RemoveKind::Folder),
            EventKind::Create(created),
        ] {
            dirty.observe(
                Ok(notify::Event::new(kind).add_path(path.clone())),
                &self.job.root,
                &self.job.cache,
            );
        }
        let pending = dirty.take();
        assert!(!pending.repair);
        assert_eq!(pending.paths, BTreeMap::from([(path, true)]));
        self.job.incremental(pending.paths).unwrap();
    }
}

#[test]
fn coalesced_remove_recreate_prunes_only_missing_descendants_of_that_directory() {
    let mut fixture = Fixture::new();
    std::fs::create_dir_all(fixture.path("replaced/nested")).unwrap();
    std::fs::create_dir(fixture.path("replaced-keep")).unwrap();
    std::fs::write(fixture.path("replaced/old.txt"), "old root child").unwrap();
    std::fs::write(fixture.path("replaced/nested/old.txt"), "old deep child").unwrap();
    std::fs::write(fixture.path("replaced-keep/kept.txt"), "unrelated sibling").unwrap();
    fixture.job.full().unwrap();
    let previous_epoch = index::epoch(&fixture.job.conn).unwrap();
    assert_eq!(fixture.counts(), (3, 3));

    // The OS can report only this pair of directory events. No individual old
    // child removal is supplied, and the event queue collapses the same path.
    std::fs::remove_dir_all(fixture.path("replaced")).unwrap();
    std::fs::create_dir_all(fixture.path("replaced/current")).unwrap();
    std::fs::write(fixture.path("replaced/current/new.txt"), "new child").unwrap();
    fixture.coalesced_replacement("replaced", CreateKind::Folder);

    assert!(!fixture.contains("replaced/old.txt"));
    assert!(!fixture.contains("replaced/nested"));
    assert!(!fixture.contains("replaced/nested/old.txt"));
    assert!(fixture.contains("replaced/current/new.txt"));
    assert!(fixture.contains("replaced-keep/kept.txt"));
    assert_eq!(fixture.counts(), (2, 3));
    let untouched_seen: i64 = fixture
        .job
        .conn
        .query_row(
            "SELECT seen FROM entries WHERE path=?1",
            [paths::encode(&fixture.path("replaced-keep/kept.txt"))],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        untouched_seen, previous_epoch,
        "Scoped replacement must not rescan or prune an unrelated prefix"
    );
}

#[test]
fn coalesced_directory_to_file_replacement_removes_the_entire_old_subtree() {
    let mut fixture = Fixture::new();
    std::fs::create_dir_all(fixture.path("replaced/nested/deeper")).unwrap();
    std::fs::write(fixture.path("replaced/nested/deeper/old.txt"), "old child").unwrap();
    std::fs::write(fixture.path("unrelated.txt"), "kept").unwrap();
    fixture.job.full().unwrap();
    assert_eq!(fixture.counts(), (2, 3));

    std::fs::remove_dir_all(fixture.path("replaced")).unwrap();
    std::fs::write(fixture.path("replaced"), "now a regular file").unwrap();
    fixture.coalesced_replacement("replaced", CreateKind::File);

    assert!(fixture.contains("replaced"));
    for old in [
        "replaced/nested",
        "replaced/nested/deeper",
        "replaced/nested/deeper/old.txt",
    ] {
        assert!(!fixture.contains(old), "Stale descendant remains: {old}");
    }
    assert!(fixture.contains("unrelated.txt"));
    assert_eq!(fixture.counts(), (2, 0));
    let kind: i64 = fixture
        .job
        .conn
        .query_row(
            "SELECT kind FROM entries WHERE path=?1",
            [paths::encode(&fixture.path("replaced"))],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        kind, 0,
        "The replacement must be a browseable/searchable file"
    );
}

#[test]
fn worktree_common_excludes_agree_between_initial_scan_and_point_events() {
    for absolute_common in [false, true] {
        // Construct the real Git worktree metadata layout consumed by ignore:
        // linked/.git -> repo/.git/worktrees/linked -> commondir -> repo/.git.
        // No external Git executable, hooks or global commit config is invoked.
        let project = tempfile::tempdir().unwrap();
        let base = project.path().canonicalize().unwrap();
        let common = base.join("repo/.git");
        let gitdir = common.join("worktrees/linked");
        let root = base.join("linked");
        std::fs::create_dir_all(common.join("info")).unwrap();
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
        std::fs::write(
            gitdir.join("commondir"),
            if absolute_common {
                format!("{}\n", common.display())
            } else {
                "../..\n".into()
            },
        )
        .unwrap();
        std::fs::write(common.join("info/exclude"), "*.private\nhidden/\n").unwrap();
        std::fs::create_dir(root.join("hidden")).unwrap();
        std::fs::write(root.join("hidden/inside.txt"), "excluded folder").unwrap();
        std::fs::write(root.join("existing.private"), "excluded file").unwrap();
        std::fs::write(root.join("visible.txt"), "visible").unwrap();
        let cache = tempfile::tempdir().unwrap();
        let mut fixture = Fixture::at(project, cache, root);
        fixture.job.full().unwrap();
        assert_eq!(fixture.counts(), (1, 0));
        assert!(!fixture.contains("existing.private"));
        assert!(!fixture.contains("hidden"));

        std::fs::write(fixture.path("existing.private"), "modified excluded file").unwrap();
        std::fs::write(fixture.path("new.private"), "new excluded file").unwrap();
        std::fs::write(fixture.path("new-visible.txt"), "new visible file").unwrap();
        fixture
            .job
            .incremental(BTreeMap::from([
                (fixture.path("existing.private"), false),
                (fixture.path("new.private"), false),
                (fixture.path("hidden/inside.txt"), false),
                (fixture.path("new-visible.txt"), false),
            ]))
            .unwrap();
        assert!(!fixture.contains("existing.private"));
        assert!(!fixture.contains("new.private"));
        assert!(!fixture.contains("hidden/inside.txt"));
        assert!(fixture.contains("visible.txt"));
        assert!(fixture.contains("new-visible.txt"));
        assert_eq!(fixture.counts(), (2, 0));
    }
}

#[test]
fn wide_deep_tree_uses_shallow_walks_and_a_restartable_sqlite_frontier() {
    const WIDTH: usize = 4_097;
    const DEPTH: usize = 16; // Exceeds WalkDir's old ten-descriptor threshold.
    let mut fixture = Fixture::new();
    let mut deepest = fixture.job.root.clone();
    for _ in 0..DEPTH {
        deepest.push("deep");
        std::fs::create_dir(&deepest).unwrap();
    }
    std::fs::write(deepest.join("needle.txt"), "the deep leaf").unwrap();
    for number in 0..WIDTH {
        std::fs::write(fixture.path(&format!("wide-{number:05}.txt")), "wide leaf").unwrap();
    }

    let mut yielded = 0;
    for entry in walker(&fixture.job.root, &fixture.job.cache).build() {
        let entry = entry.unwrap();
        assert!(
            entry.depth() <= 1,
            "A directory walker descended into a child"
        );
        yielded += 1;
    }
    assert_eq!(yielded, WIDTH + 2); // Root, immediate deep directory, wide files.

    let current = index::begin_scan(&fixture.job.conn).unwrap();
    let mut delta = Delta::default();
    fixture
        .job
        .seed(&fixture.job.root.clone(), &mut delta)
        .unwrap();
    assert!(
        !fixture.contains("deep/deep"),
        "Shallow seed must not enumerate grandchildren"
    );
    assert_eq!(
        index::next_directory(&fixture.job.conn, current, None).unwrap(),
        Some(fixture.path("deep"))
    );
    // Replace the actual connection. The traversal's pending directory survives
    // in SQLite; no in-memory list of remaining ancestor entries is required.
    fixture.job.conn = index::open_writer(&fixture.database, &fixture.job.root).unwrap();
    assert_eq!(
        index::next_directory(&fixture.job.conn, current, None).unwrap(),
        Some(fixture.path("deep"))
    );
    fixture
        .job
        .tree(&fixture.job.root.clone(), true, &mut delta)
        .unwrap();
    assert!(
        index::next_directory(&fixture.job.conn, current, None)
            .unwrap()
            .is_none()
    );
    let row: (i64, i64) = fixture.job.conn.query_row(
        "SELECT (SELECT count(*) FROM entries WHERE kind=0), (SELECT count(*) FROM entries WHERE kind=1)",
        params![], |row| Ok((row.get(0)?, row.get(1)?)),
    ).unwrap();
    assert_eq!(row, ((WIDTH + 1) as i64, DEPTH as i64));
    assert!(
        fixture
            .job
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM entries WHERE path=?1)",
                [paths::encode(&deepest.join("needle.txt"))],
                |row| row.get::<_, bool>(0),
            )
            .unwrap()
    );
    assert!(delta.changed.len() + delta.removed.len() <= 256);
    assert!(
        delta.incomplete,
        "A large streamed scan must not claim a complete tiny delta"
    );
}
