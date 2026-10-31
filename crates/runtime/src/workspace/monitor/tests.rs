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
/// A busy file event service can hold watcher registration for seconds. That
/// must not hold the project task past cancellation or delay runtime shutdown,
/// and a registration that finishes late is still released.
#[test]
fn slow_native_watch_registration_never_delays_runtime_shutdown() {
    struct Watcher(std::sync::mpsc::Sender<()>);
    impl Drop for Watcher {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (entered, registering) = std::sync::mpsc::channel();
    let (finish, finished) = std::sync::mpsc::channel::<()>();
    let (released, release) = std::sync::mpsc::channel();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let stop = CancellationToken::new();
    let (events, _incoming) = async_channel::bounded(16);
    let task = runtime.spawn(observe(
        root,
        cache.path().join("cache"),
        cache.path().join("index.sqlite"),
        Arc::new(RwLock::new(None)),
        Arc::new(DirtyPaths::new()),
        events,
        stop.clone(),
        move |_: &Path, _, _| {
            entered.send(()).unwrap();
            let _ = finished.recv();
            Some(Watcher(released))
        },
    ));
    registering
        .recv_timeout(Duration::from_secs(5))
        .expect("registration started");
    let (stopped, shutdown) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        stop.cancel();
        let result = runtime.block_on(task);
        drop(runtime);
        let _ = stopped.send(result.map(|observed| observed.is_ok()));
    });
    let observed = shutdown
        .recv_timeout(Duration::from_secs(3))
        .expect("runtime shut down while registration was still blocked");
    assert!(observed.unwrap());
    finish.send(()).unwrap();
    release
        .recv_timeout(Duration::from_secs(5))
        .expect("late watcher released");
}
/// A watcher that registers after the first scan started repairs the
/// catalogue once, so a change made before it was live is not lost.
#[test]
fn late_native_watch_registration_repairs_changes_made_before_it_was_live() {
    let directory = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (finish, finished) = std::sync::mpsc::channel::<()>();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let stop = CancellationToken::new();
    let (events, incoming) = async_channel::bounded(64);
    runtime.spawn(observe(
        root.clone(),
        cache.path().join("cache"),
        cache.path().join("index.sqlite"),
        Arc::new(RwLock::new(None)),
        Arc::new(DirtyPaths::new()),
        events,
        stop.clone(),
        move |_: &Path, _, _| {
            let _ = finished.recv();
            Some(())
        },
    ));
    let next = |accept: &dyn Fn(&FileIndex) -> bool| {
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if let WorkspaceEvent::Indexed(index) = incoming.recv().await.unwrap()
                        && accept(&index)
                    {
                        return index;
                    }
                }
            })
            .await
            .expect("catalogue event")
        })
    };
    let first = next(&|index| !index.indexing);
    assert_eq!((first.file_count, first.watching), (0, false));
    std::fs::write(root.join("missed.txt"), "text").unwrap();
    finish.send(()).unwrap();
    next(&|index| index.watching && !index.indexing && index.file_count == 1);
    stop.cancel();
}
