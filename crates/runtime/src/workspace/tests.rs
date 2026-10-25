use super::*;
use rusqlite::params;
use std::time::Duration;

async fn next_matching(
    incoming: &async_channel::Receiver<WorkspaceEvent>,
    predicate: impl Fn(&FileIndex) -> bool,
) -> Arc<FileIndex> {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match incoming.recv().await.unwrap() {
                WorkspaceEvent::Indexed(index) if predicate(&index) => return index,
                WorkspaceEvent::Unavailable { message, .. } => {
                    panic!("Workspace unavailable: {message}")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("Expected workspace update")
}
fn options(directory: &Path, after: Option<FileCursor>, limit: usize) -> FileOptions {
    FileOptions {
        directory: directory.to_owned(),
        after,
        limit,
    }
}
fn catalogue(root: &Path, cache: &Path) -> PathBuf {
    let database = cache.join(paths::database_name(root));
    let mut conn = index::open_writer(&database, root).unwrap();
    let epoch = index::begin_scan(&conn).unwrap();
    let mut delta = index::Delta::default();
    let stop = CancellationToken::new();
    let context = index::ScanContext {
        root,
        cache,
        cancel: &stop,
        stop: &stop,
    };
    context
        .seed(&mut conn, root, &mut delta, |_, _| Ok(()))
        .unwrap();
    while let Some(directory) = index::next_directory(&conn, epoch, None).unwrap() {
        context
            .seed(&mut conn, &directory, &mut delta, |_, _| Ok(()))
            .unwrap();
    }
    conn.execute(
        "UPDATE meta SET indexing=0,revision=revision+1 WHERE id=1",
        [],
    )
    .unwrap();
    database
}

#[tokio::test]
async fn real_native_watch_observes_create_rename_delete_and_project_switch() {
    let directory = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(directory.path()).unwrap();
    std::fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
    std::fs::create_dir(root.join("ignored")).unwrap();
    std::fs::write(root.join("ignored/hidden.txt"), "skip").unwrap();
    let (events, incoming) = async_channel::bounded(16);
    let service = WorkspaceService::with_cache(events, cache.path().to_owned());
    service.open(&root).await.unwrap();
    let first = next_matching(&incoming, |index| !index.indexing).await;
    let page = service
        .files(
            root.clone(),
            options(&root, None, 50),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        page.entries
            .iter()
            .all(|entry| !entry.path.ends_with("ignored"))
    );
    let database = cache.path().join(paths::database_name(&root));
    let before_epoch = index::epoch(&index::open_reader(&database).unwrap()).unwrap();
    std::fs::write(root.join("new.rs"), "fn main() {}\n").unwrap();
    let created = next_matching(&incoming, |index| {
        index.changed.contains(&root.join("new.rs"))
    })
    .await;
    assert_eq!(created.file_count, first.file_count + 1);
    // A precise file event must not restart a recursive project scan.
    assert_eq!(
        index::epoch(&index::open_reader(&database).unwrap()).unwrap(),
        before_epoch
    );
    std::fs::rename(root.join("new.rs"), root.join("renamed.rs")).unwrap();
    next_matching(&incoming, |index| {
        index.removed.contains(&root.join("new.rs"))
            && index.changed.contains(&root.join("renamed.rs"))
    })
    .await;
    std::fs::remove_file(root.join("renamed.rs")).unwrap();
    next_matching(&incoming, |index| {
        index.removed.contains(&root.join("renamed.rs"))
    })
    .await;
    let other = tempfile::tempdir().unwrap();
    service.open(other.path()).await.unwrap();
    let switched = next_matching(&incoming, |index| index.root != root && !index.indexing).await;
    assert_ne!(switched.root, root);
    assert!(service.snapshot(&root).is_err());
    service.shutdown();
}

#[test]
fn full_project_search_handles_unicode_paging_ignore_rules_and_symlinks() {
    let directory = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(directory.path()).unwrap();
    std::fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(root.join("ignored.txt"), "世界 needle\n").unwrap();
    std::fs::write(
        root.join("one.rs"),
        "// 世界 needle\n// NEEDLE\n// needle\n",
    )
    .unwrap();
    std::fs::write(root.join("two.rs"), "// needle\n").unwrap();
    #[cfg(unix)]
    let outside = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        std::fs::write(outside.path().join("secret"), "needle").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), root.join("escape")).unwrap();
    }
    let database = catalogue(&root, cache.path());
    let cancel = CancellationToken::new();
    let project = CancellationToken::new();
    let options = SearchOptions {
        query: "needle".into(),
        case_sensitive: false,
        regular_expression: false,
        after: None,
        limit: 2,
    };
    let first = search::search(&database, &root, options.clone(), &cancel, &project).unwrap();
    assert_eq!(first.hits.len(), 2);
    assert_eq!(first.hits[0].byte_column, "// 世界 ".len());
    assert!(first.hits[0].preview.contains("世界"));
    assert!(
        first
            .hits
            .iter()
            .all(|hit| !hit.path.ends_with("ignored.txt") && !hit.path.ends_with("escape"))
    );
    let second = search::search(
        &database,
        &root,
        SearchOptions {
            after: first.next,
            ..options.clone()
        },
        &cancel,
        &project,
    )
    .unwrap();
    assert_eq!(second.hits.len(), 2);
    assert!(second.next.is_none());
    assert_eq!(second.hits[0].line, 3);
    assert!(
        search::search(
            &database,
            &root,
            SearchOptions {
                query: "[".into(),
                regular_expression: true,
                ..options.clone()
            },
            &cancel,
            &project
        )
        .is_err()
    );
    cancel.cancel();
    assert!(
        search::search(&database, &root, options, &cancel, &project)
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
}

#[test]
fn keyset_folder_pages_reach_every_row_without_project_sized_allocations() {
    let root_dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(root_dir.path()).unwrap();
    let database = cache.path().join(paths::database_name(&root));
    let mut conn = index::open_writer(&database, &root).unwrap();
    let tx = conn.transaction().unwrap();
    for number in 0..20_013 {
        tx.execute(
            "INSERT INTO entries(path,parent,kind,bytes,modified,seen) VALUES(?1,?2,0,0,NULL,1)",
            params![
                paths::encode(&root.join(format!("file-{number:06}"))),
                paths::encode(&root)
            ],
        )
        .unwrap();
    }
    tx.execute(
        "UPDATE meta SET indexing=0,epoch=1,revision=2 WHERE id=1",
        [],
    )
    .unwrap();
    tx.commit().unwrap();
    drop(conn);
    let cancel = CancellationToken::new();
    let mut after = None;
    let mut count = 0;
    let mut previous = None;
    loop {
        let page = index::files(
            &database,
            &root,
            options(&root, after, 137),
            &cancel,
            &cancel,
        )
        .unwrap();
        assert!(page.entries.len() <= 137);
        assert_eq!(page.file_count, 20_013);
        for entry in page.entries {
            let key = paths::encode(&entry.path);
            assert!(previous.as_ref().is_none_or(|previous| previous < &key));
            previous = Some(key);
            count += 1;
        }
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(count, 20_013);
    let conn = index::open_reader(&database).unwrap();
    let plans:Vec<String> = conn.prepare("EXPLAIN QUERY PLAN SELECT path FROM entries WHERE kind=0 AND path>?1 ORDER BY path LIMIT 128").unwrap().query_map([Vec::<u8>::new()],|row|row.get(3)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    assert!(
        plans
            .iter()
            .any(|plan| plan.contains("entries_kind") && plan.contains("path>")),
        "{plans:?}"
    );
}

#[test]
fn deadline_without_hits_preserves_a_search_continuation() {
    let directory = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(directory.path()).unwrap();
    std::fs::write(root.join("first"), "no match\n").unwrap();
    std::fs::write(root.join("last"), "needle\n").unwrap();
    let database = catalogue(&root, cache.path());
    let stop = CancellationToken::new();
    let options = SearchOptions {
        query: "needle".into(),
        case_sensitive: false,
        regular_expression: false,
        after: None,
        limit: 5,
    };
    let interrupted = search::search_for(
        &database,
        &root,
        options.clone(),
        &stop,
        &stop,
        Duration::ZERO,
    )
    .unwrap();
    assert!(interrupted.timed_out && interrupted.hits.is_empty());
    assert!(interrupted.next.is_some());
    let finished = search::search(
        &database,
        &root,
        SearchOptions {
            after: interrupted.next,
            ..options
        },
        &stop,
        &stop,
    )
    .unwrap();
    assert_eq!(finished.hits.len(), 1);
    assert!(finished.hits[0].path.ends_with("last"));
}

#[test]
fn folder_on_demand_seed_and_cached_restart_do_not_wait_for_recursive_scan() {
    let directory = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(directory.path()).unwrap();
    std::fs::create_dir(root.join("later")).unwrap();
    std::fs::create_dir(root.join("later/nested")).unwrap();
    std::fs::write(root.join("later/new.txt"), "saved").unwrap();
    let database = cache.path().join(paths::database_name(&root));
    let conn = index::open_writer(&database, &root).unwrap();
    index::begin_scan(&conn).unwrap();
    drop(conn); // Simulates a restart with a partially populated catalogue.
    let stop = CancellationToken::new();
    let page = index::files(
        &database,
        &root,
        options(&root.join("later"), None, 10),
        &stop,
        &stop,
    )
    .unwrap();
    assert!(page.indexing);
    assert_eq!(page.entries.len(), 2);
    assert!(
        page.entries
            .iter()
            .any(|entry| entry.path.ends_with("nested") && entry.is_dir)
    );
    assert!(
        page.entries
            .iter()
            .any(|entry| entry.path.ends_with("new.txt") && !entry.is_dir)
    );
    let conn = index::open_reader(&database).unwrap();
    assert_eq!(
        index::metadata(&conn, &root, &index::Delta::default())
            .unwrap()
            .file_count,
        1
    );
}

#[cfg(unix)]
#[test]
fn non_utf8_names_remain_distinct_native_file_identities() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let directory = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(directory.path()).unwrap();
    let first = root.join(OsString::from_vec(vec![b'f', 0x80]));
    let second = root.join(OsString::from_vec(vec![b'f', 0x81]));
    let database = match std::fs::write(&first, "needle") {
        Ok(()) => {
            std::fs::write(&second, "needle").unwrap();
            catalogue(&root, cache.path())
        }
        Err(error) => {
            // APFS rejects ill-formed UTF-8 at file creation. The database must
            // still preserve both native identities; Linux also exercises the
            // real-directory traversal branch above.
            assert!(
                cfg!(target_os = "macos") && error.raw_os_error() == Some(libc::EILSEQ),
                "{error}"
            );
            let database = cache.path().join(paths::database_name(&root));
            let conn = index::open_writer(&database, &root).unwrap();
            for path in [&first, &second] {
                conn.execute("INSERT INTO entries(path,parent,kind,bytes,modified,seen) VALUES(?1,?2,0,0,NULL,1)",params![paths::encode(path),paths::encode(&root)]).unwrap();
            }
            conn.execute(
                "UPDATE meta SET indexing=0,epoch=1,revision=2 WHERE id=1",
                [],
            )
            .unwrap();
            database
        }
    };
    let stop = CancellationToken::new();
    let page = index::files(&database, &root, options(&root, None, 10), &stop, &stop).unwrap();
    assert_eq!(page.entries.len(), 2);
    assert!(page.entries.iter().any(|entry| entry.path == first));
    assert!(page.entries.iter().any(|entry| entry.path == second));
    assert_ne!(paths::encode(&first), paths::encode(&second));
}

#[test]
fn point_ignore_rules_agree_with_walk_for_nested_ignore_and_negations() {
    let directory = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(directory.path()).unwrap();
    std::fs::create_dir(root.join("nested")).unwrap();
    std::fs::create_dir(root.join("excluded")).unwrap();
    std::fs::write(root.join(".gitignore"), "*.log\nexcluded/\n").unwrap();
    std::fs::write(root.join(".ignore"), "!keep.log\n").unwrap();
    std::fs::write(root.join("nested/.gitignore"), "!keep.log\nsecret.txt\n").unwrap();
    for name in [
        "skip.log",
        "keep.log",
        "nested/skip.log",
        "nested/keep.log",
        "nested/secret.txt",
        "nested/normal.txt",
        "excluded/normal.txt",
    ] {
        std::fs::write(root.join(name), "text").unwrap();
    }
    let scanned: std::collections::HashSet<_> = ignore::WalkBuilder::new(&root)
        .hidden(false)
        .require_git(false)
        .follow_links(false)
        .build()
        .map(|entry| entry.unwrap().into_path())
        .collect();
    let mut visibility = rules::Visibility::new(&root);
    for name in [
        "skip.log",
        "keep.log",
        "nested/skip.log",
        "nested/keep.log",
        "nested/secret.txt",
        "nested/normal.txt",
        "excluded/normal.txt",
    ] {
        assert_eq!(
            visibility.visible(&root.join(name), false),
            scanned.contains(&root.join(name)),
            "{name}"
        );
    }
}

#[tokio::test]
async fn cancelling_a_queued_query_or_switching_project_does_not_start_disk_work() {
    let directory = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (events, incoming) = async_channel::bounded(16);
    let service = WorkspaceService::with_cache(events, cache.path().to_owned());
    service.open(&root).await.unwrap();
    next_matching(&incoming, |index| !index.indexing).await;
    let (_, _, gate) = service.query_context(&root).unwrap();
    let first = gate.clone().acquire_owned().await.unwrap();
    let second = gate.acquire_owned().await.unwrap();
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let requested = root.clone();
    let worker = service.clone();
    let queued = tokio::spawn(async move {
        worker
            .files(requested.clone(), options(&requested, None, 10), token)
            .await
    });
    tokio::task::yield_now().await;
    cancel.cancel();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), queued)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
    let requested = root.clone();
    let worker = service.clone();
    let queued = tokio::spawn(async move {
        worker
            .files(
                requested.clone(),
                options(&requested, None, 10),
                CancellationToken::new(),
            )
            .await
    });
    tokio::task::yield_now().await;
    service.shutdown();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), queued)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    drop((first, second));
}

#[test]
fn search_resumes_at_a_byte_offset_with_original_unicode_line_numbers() {
    let directory = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let prefix = "世界 needle already returned\n";
    std::fs::write(
        root.join("file"),
        format!("{prefix}nonmatching\n世界 needle next\n"),
    )
    .unwrap();
    let database = catalogue(&root, cache.path());
    let stop = CancellationToken::new();
    let revision = index::metadata(
        &index::open_reader(&database).unwrap(),
        &root,
        &index::Delta::default(),
    )
    .unwrap()
    .revision;
    let page = search::search(
        &database,
        &root,
        SearchOptions {
            query: "needle".into(),
            case_sensitive: false,
            regular_expression: false,
            after: Some(SearchCursor {
                revision,
                path: root.join("file"),
                line: 1,
                byte_offset: prefix.len() as u64,
            }),
            limit: 10,
        },
        &stop,
        &stop,
    )
    .unwrap();
    assert_eq!(page.hits.len(), 1);
    assert_eq!(page.hits[0].line, 3);
    assert_eq!(page.hits[0].byte_column, "世界 ".len());
    assert!(page.next.is_none());
}

#[tokio::test]
async fn eight_column_legacy_cache_is_preserved_while_versioned_catalogue_builds() {
    use sha2::{Digest, Sha256};
    let directory = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    std::fs::write(root.join("old.txt"), "old file").unwrap();
    std::fs::write(root.join("fresh.txt"), "new file").unwrap();
    let legacy = cache
        .path()
        .join(format!("{:x}.sqlite", Sha256::digest(paths::encode(&root))));
    {
        let conn = rusqlite::Connection::open(&legacy).unwrap();
        // The exact legacy metadata shape observed in the real default profile.
        conn.execute_batch("CREATE TABLE meta(id INTEGER PRIMARY KEY,root BLOB NOT NULL,revision INTEGER,epoch INTEGER,files INTEGER,dirs INTEGER,unreadable INTEGER,indexing INTEGER);CREATE TABLE entries(path BLOB PRIMARY KEY,parent BLOB,kind INTEGER,bytes INTEGER,modified BLOB,seen INTEGER,seeded INTEGER) WITHOUT ROWID;").unwrap();
        conn.execute(
            "INSERT INTO meta VALUES(1,?1,9,4,1,0,0,0)",
            [paths::encode(&root)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO entries VALUES(?1,?2,0,8,NULL,4,0)",
            params![paths::encode(&root.join("old.txt")), paths::encode(&root)],
        )
        .unwrap();
    }
    let before = std::fs::read(&legacy).unwrap();
    let (events, incoming) = async_channel::bounded(16);
    let service = WorkspaceService::with_cache(events, cache.path().to_owned());
    service.open(&root).await.unwrap();
    let complete = next_matching(&incoming, |index| !index.indexing).await;
    assert_eq!(complete.file_count, 2);
    let page = service
        .files(
            root.clone(),
            options(&root, None, 10),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(page.entries.len(), 2);
    assert!(
        page.entries
            .iter()
            .any(|entry| entry.path.ends_with("fresh.txt"))
    );
    let versioned = cache.path().join(paths::database_name(&root));
    assert_ne!(versioned, legacy);
    assert!(
        versioned
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(".v1.sqlite")
    );
    let conn = index::open_reader(&versioned).unwrap();
    assert_eq!(
        conn.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        paths::FORMAT_VERSION
    );
    assert_eq!(
        std::fs::read(&legacy).unwrap(),
        before,
        "Opening current code must not mutate or delete the old cache"
    );
    service.shutdown();
}

#[test]
fn unknown_cache_version_or_layout_fails_closed_without_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    for (name, version) in [("future", 99), ("untagged", 0), ("wrong-layout", 1)] {
        let database = cache.path().join(format!("{name}.sqlite"));
        {
            let conn = rusqlite::Connection::open(&database).unwrap();
            conn.execute_batch("CREATE TABLE meta(id INTEGER PRIMARY KEY,root BLOB NOT NULL,revision INTEGER,epoch INTEGER,files INTEGER,dirs INTEGER,unreadable INTEGER,indexing INTEGER);CREATE TABLE entries(path BLOB PRIMARY KEY,parent BLOB,kind INTEGER,bytes INTEGER,modified BLOB,seen INTEGER,seeded INTEGER) WITHOUT ROWID;").unwrap();
            conn.pragma_update(None, "user_version", version).unwrap();
        }
        let before = std::fs::read(&database).unwrap();
        assert!(
            index::open_writer(&database, &root)
                .unwrap_err()
                .to_string()
                .contains("preserved")
        );
        assert!(index::open_reader(&database).is_err());
        assert_eq!(
            std::fs::read(&database).unwrap(),
            before,
            "Unsupported format must not be guessed or rewritten"
        );
    }
    let empty = cache.path().join("interrupted-empty.v1.sqlite");
    std::fs::File::create(&empty).unwrap();
    let initialized = index::open_writer(&empty, &root).unwrap();
    assert_eq!(
        initialized
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        paths::FORMAT_VERSION
    );
}
