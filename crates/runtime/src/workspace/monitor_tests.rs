use super::*;
fn job(root: &Path, cache: &Path) -> Job {
    let (events, _) = async_channel::bounded(16);
    Job {
        conn: index::open_writer(&cache.join(super::super::paths::database_name(root)), root)
            .unwrap(),
        root: root.to_owned(),
        cache: cache.to_owned(),
        progress: Progress {
            root: root.to_owned(),
            latest: Arc::new(RwLock::new(None)),
            events,
            last: Instant::now(),
        },
        stop: CancellationToken::new(),
    }
}
#[test]
fn event_overflow_and_native_watch_error_request_one_bounded_repair() {
    let dirty = DirtyPaths::new();
    for number in 0..DIRTY_LIMIT + 200 {
        dirty.path(PathBuf::from(format!("/project/{number}")), false);
    }
    let pending = dirty.take();
    assert!(pending.repair && pending.paths.is_empty());
    assert!(dirty.take().paths.is_empty());
    dirty.observe(
        Err(notify::Error::generic("watch capacity exhausted")),
        Path::new("/project"),
        Path::new("/private-cache"),
    );
    assert!(dirty.watch_failed.load(Ordering::Acquire));
    assert!(dirty.take().repair);
    dirty.observe(
        Ok(
            notify::Event::new(EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )))
            .add_path(PathBuf::from("/project/cache/index.sqlite-wal")),
        ),
        Path::new("/project"),
        Path::new("/project/cache"),
    );
    assert!(dirty.take().paths.is_empty());
}
#[test]
fn cancellation_preserves_partial_catalogue_and_restart_repairs_it() {
    let directory = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    for number in 0..1_025 {
        std::fs::write(root.join(format!("file-{number}")), "text").unwrap();
    }
    let mut first = job(&root, cache.path());
    first.full().unwrap();
    assert_eq!(
        index::metadata(&first.conn, &root, &Delta::default())
            .unwrap()
            .file_count,
        1_025
    );
    for number in 0..20 {
        std::fs::remove_file(root.join(format!("file-{number}"))).unwrap();
    }
    index::begin_scan(&first.conn).unwrap();
    let context = index::ScanContext {
        root: &root,
        cache: cache.path(),
        cancel: &first.stop,
        stop: &first.stop,
    };
    assert!(
        context
            .seed(&mut first.conn, &root, &mut Delta::default(), |_, _| {
                first.stop.cancel();
                Ok(())
            })
            .is_err()
    );
    let interrupted = index::metadata(&first.conn, &root, &Delta::default()).unwrap();
    assert!(interrupted.indexing);
    assert_eq!(
        interrupted.file_count, 1_025,
        "Cancelled traversal must not claim unvisited files were removed"
    );
    drop(first);
    let mut restarted = job(&root, cache.path());
    restarted.full().unwrap();
    let completed = index::metadata(&restarted.conn, &root, &Delta::default()).unwrap();
    assert!(!completed.indexing);
    assert_eq!(completed.file_count, 1_005);
}
#[test]
fn large_subtree_delete_is_chunked_and_never_aliases_a_sibling_prefix() {
    let directory = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("many")).unwrap();
    std::fs::create_dir(root.join("many-keep")).unwrap();
    for number in 0..1_025 {
        std::fs::write(root.join("many").join(format!("file-{number}")), "text").unwrap();
    }
    std::fs::write(root.join("many-keep/kept"), "text").unwrap();
    let mut job = job(&root, cache.path());
    job.full().unwrap();
    let first = job.progress.latest.read().clone().unwrap();
    assert_eq!(first.file_count, 1_026);
    assert!(first.changed.len() <= 256 && !first.changes_complete);
    let epoch = index::epoch(&job.conn).unwrap();
    std::fs::remove_dir_all(root.join("many")).unwrap();
    job.incremental(BTreeMap::from([(root.join("many"), true)]))
        .unwrap();
    let changed = job.progress.latest.read().clone().unwrap();
    assert_eq!(changed.file_count, 1);
    assert_eq!(changed.directory_count, 1);
    assert!(changed.removed.len() <= 256 && !changed.changes_complete);
    assert_eq!(index::epoch(&job.conn).unwrap(), epoch);
    assert_eq!(
        job.conn
            .query_row(
                "SELECT count(*) FROM entries WHERE path=?1",
                [super::super::paths::encode(&root.join("many-keep/kept"))],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
}
#[test]
fn incremental_edits_atomically_invalidate_cursors_without_a_new_scan_epoch() {
    let directory = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    for name in ["a", "b"] {
        std::fs::write(root.join(name), "text").unwrap();
    }
    let mut job = job(&root, cache.path());
    job.full().unwrap();
    let database = cache.path().join(super::super::paths::database_name(&root));
    let stop = CancellationToken::new();
    let page = index::files(
        &database,
        &root,
        index::FileOptions {
            directory: root.clone(),
            after: None,
            limit: 1,
        },
        &stop,
        &stop,
    )
    .unwrap();
    assert!(page.next.is_some());
    let epoch = index::epoch(&job.conn).unwrap();
    std::fs::write(root.join("b"), "new content").unwrap();
    job.incremental(BTreeMap::from([(root.join("b"), false)]))
        .unwrap();
    assert_eq!(index::epoch(&job.conn).unwrap(), epoch);
    assert!(
        index::files(
            &database,
            &root,
            index::FileOptions {
                directory: root.clone(),
                after: page.next,
                limit: 1
            },
            &stop,
            &stop
        )
        .unwrap_err()
        .to_string()
        .contains("changed")
    );
}

#[test]
fn named_file_change_invalidates_even_if_size_and_timestamp_are_preserved() {
    let directory = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let path = root.join("file");
    std::fs::write(&path, "old").unwrap();
    let original = std::fs::metadata(&path).unwrap().modified().unwrap();
    let mut job = job(&root, cache.path());
    job.full().unwrap();
    std::fs::write(&path, "new").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(original)
        .unwrap();
    job.incremental(BTreeMap::from([(path.clone(), false)]))
        .unwrap();
    assert!(
        job.progress
            .latest
            .read()
            .as_ref()
            .unwrap()
            .changed
            .contains(&path)
    );
}
