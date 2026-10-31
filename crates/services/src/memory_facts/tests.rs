use super::*;
#[test]
fn in_flight_sql_cancellation_is_local_to_this_operation() {
    let (_dir, store, _memory) = setup();
    let cancel = CancellationToken::new();
    let stopped = cancel.clone();
    let trigger = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(10));
        stopped.cancel();
    });
    store.call(move|conn|{
        let result=cancellable(conn,&cancel,|conn|{
            conn.query_row("WITH RECURSIVE n(x)AS(VALUES(0) UNION ALL SELECT x+1 FROM n WHERE x<100000000) SELECT sum(x) FROM n",[],|row|row.get::<_,i64>(0)).map_err(anyhow::Error::from)
        });
        assert!(result.is_err());let value:i64=conn.query_row("SELECT 7",[],|row|row.get(0))?;assert_eq!(value,7);Ok(())
    }).unwrap();
    trigger.join().unwrap();
}
#[test]
fn failed_canonical_delete_rolls_back_fts_vectors_and_receipt() {
    let (_dir, store, memory) = setup();
    let record = fact("a", "preserve on failure", None);
    let id = record.id;
    memory.remember_sync(record).unwrap();
    store.call(move|conn|{conn.execute_batch("CREATE VIRTUAL TABLE memory_vec_deadbeef USING vec0(id TEXT PRIMARY KEY,scope TEXT PARTITION KEY,embedding float[2]);CREATE TABLE IF NOT EXISTS memory_vector_tables(name TEXT PRIMARY KEY,model TEXT NOT NULL,dimensions INTEGER NOT NULL);INSERT INTO memory_vector_tables VALUES('memory_vec_deadbeef','fixture',2);CREATE TRIGGER reject_fact_delete BEFORE DELETE ON memories BEGIN SELECT RAISE(ABORT,'fixture reject');END;")?;conn.execute("INSERT INTO memory_vec_deadbeef VALUES(?1,'a','[1,0]')",[id.to_string()])?;Ok(())}).unwrap();
    let preview = store
        .fact_delete_preview("a", FactDeleteKind::Project, &CancellationToken::new())
        .unwrap();
    let request = FactDeleteRequest {
        operation_id: Uuid::new_v4(),
        preview: preview.clone(),
    };
    assert!(
        memory
            .delete_facts(request, &CancellationToken::new())
            .is_err()
    );
    assert_eq!(
        store
            .fact_delete_preview("a", FactDeleteKind::Project, &CancellationToken::new())
            .unwrap()
            .revision,
        preview.revision
    );
    store
        .call(|conn| {
            for table in ["memories", "memories_fts", "memory_vec_deadbeef"] {
                let count: i64 =
                    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                        row.get(0)
                    })?;
                assert_eq!(count, 1);
            }
            Ok(())
        })
        .unwrap();
}
fn fact(scope: &str, text: &str, parent: Option<Uuid>) -> MemoryRecord {
    MemoryRecord {
        id: Uuid::new_v4(),
        scope: scope.into(),
        content: text.into(),
        source: "Explicit fixture provenance".into(),
        created_at: 7,
        supersedes: parent,
        origin: xylo_core::MemoryOrigin::UserTyped,
    }
}
fn setup() -> (tempfile::TempDir, Store, SqliteMemory) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("memory.db")).unwrap();
    let memory = SqliteMemory::new(store.clone());
    (dir, store, memory)
}
fn delete(memory: &SqliteMemory, store: &Store, scope: &str, kind: FactDeleteKind) -> FactDeletion {
    let preview = store
        .fact_delete_preview(scope, kind, &CancellationToken::new())
        .unwrap();
    memory
        .delete_facts(
            FactDeleteRequest {
                operation_id: Uuid::new_v4(),
                preview,
            },
            &CancellationToken::new(),
        )
        .unwrap()
}
#[test]
fn chain_survives_middle_deletion_and_forget_removes_all_derived_records_only_in_scope() {
    let (_dir, store, memory) = setup();
    let a = fact("a", "first", None);
    memory.remember_sync(a.clone()).unwrap();
    let b = fact("a", "second", Some(a.id));
    memory.remember_sync(b.clone()).unwrap();
    let c = fact("a", "third", Some(b.id));
    memory.remember_sync(c.clone()).unwrap();
    let unrelated = fact("a", "different fact", None);
    memory.remember_sync(unrelated.clone()).unwrap();
    let outside = fact("b", "other project", None);
    memory.remember_sync(outside.clone()).unwrap();
    let initial = store
        .fact_versions("a", Some(c.id), None, 50, &CancellationToken::new())
        .unwrap();
    assert_eq!(initial.versions.len(), 3);
    assert_eq!(initial.versions.iter().filter(|v| v.active).count(), 1);
    delete(&memory, &store, "a", FactDeleteKind::Version { id: b.id });
    let remaining = store
        .fact_versions("a", Some(c.id), None, 50, &CancellationToken::new())
        .unwrap();
    assert_eq!(remaining.versions.len(), 2);
    assert!(remaining.versions.iter().any(|v| v.record.id == a.id));
    assert!(
        memory.remember_sync(b.clone()).is_err(),
        "Deleted versions must never be resurrected by a stale retry"
    );
    let ids = [a.id, c.id, unrelated.id, outside.id];
    store.call(move|conn|{
        conn.execute_batch("CREATE VIRTUAL TABLE memory_vec_abcdef USING vec0(id TEXT PRIMARY KEY,scope TEXT PARTITION KEY,embedding float[2]);CREATE TABLE IF NOT EXISTS memory_vector_tables(name TEXT PRIMARY KEY,model TEXT NOT NULL,dimensions INTEGER NOT NULL);INSERT INTO memory_vector_tables VALUES('memory_vec_abcdef','fixture',2);")?;
        for(id,scope)in ids.into_iter().zip(["a","a","a","b"]){conn.execute("INSERT INTO memory_vec_abcdef(id,scope,embedding) VALUES(?1,?2,'[1,0]')",params![id.to_string(),scope])?;}
        conn.execute("INSERT INTO skills(name,version,json)VALUES('procedure',1,'{}')",[])?;
        conn.execute("INSERT INTO conversations(id,json,updated_at)VALUES('keep','{}',0)",[])?;
        conn.execute("INSERT INTO messages(id,session_id,json,created_at)VALUES('keep','keep','{}',0)",[])?;
        conn.execute("INSERT INTO checkpoints(session_id,turn_id,length,revision,updated_at)VALUES('keep','turn',0,1,0)",[])?;Ok(())
    }).unwrap();
    let receipt = delete(&memory, &store, "a", FactDeleteKind::Chain { id: c.id });
    assert_eq!(receipt.removed, 2);
    store
        .call(|conn| {
            for table in ["memories", "memories_fts", "memory_vec_abcdef"] {
                let count: i64 =
                    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                        row.get(0)
                    })?;
                assert_eq!(count, 2, "{table}");
            }
            for table in ["skills", "messages", "checkpoints"] {
                let count: i64 =
                    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                        row.get(0)
                    })?;
                assert_eq!(count, 1, "{table}");
            }
            Ok(())
        })
        .unwrap();
    let receipt = delete(&memory, &store, "a", FactDeleteKind::Project);
    assert_eq!(receipt.removed, 1);
    assert_eq!(
        store.memory_page("b", None, 100).unwrap().records[0].id,
        outside.id
    );
    assert!(memory.remember_sync(a).is_err());
    assert!(memory.remember_sync(c).is_err());
}
#[test]
fn revisions_fence_preview_races_and_operation_receipts_are_idempotent() {
    let (_dir, store, memory) = setup();
    let first = fact("a", "one", None);
    memory.remember_sync(first.clone()).unwrap();
    let preview = store
        .fact_delete_preview("a", FactDeleteKind::Project, &CancellationToken::new())
        .unwrap();
    memory
        .remember_sync(fact("a", "added after confirmation opened", None))
        .unwrap();
    assert!(
        memory
            .delete_facts(
                FactDeleteRequest {
                    operation_id: Uuid::new_v4(),
                    preview
                },
                &CancellationToken::new()
            )
            .is_err()
    );
    let preview = store
        .fact_delete_preview(
            "a",
            FactDeleteKind::Version { id: first.id },
            &CancellationToken::new(),
        )
        .unwrap();
    let request = FactDeleteRequest {
        operation_id: Uuid::new_v4(),
        preview,
    };
    let receipt = memory
        .delete_facts(request.clone(), &CancellationToken::new())
        .unwrap();
    assert_eq!(
        memory
            .delete_facts(request.clone(), &CancellationToken::new())
            .unwrap()
            .removed,
        receipt.removed
    );
    let mut reused = request;
    reused.preview.kind = FactDeleteKind::Project;
    assert!(
        memory
            .delete_facts(reused, &CancellationToken::new())
            .is_err()
    );
    assert_eq!(store.memory_page("a", None, 100).unwrap().records.len(), 1);
}
#[test]
fn archive_pages_are_tie_safe_revision_consistent_and_cancellable() {
    let (_dir, store, memory) = setup();
    let mut expected = std::collections::BTreeSet::new();
    for i in 0..113 {
        let record = fact("a", &format!("fact {i}"), None);
        expected.insert(record.id);
        memory.remember_sync(record).unwrap();
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut before = None;
    loop {
        let page = store
            .fact_versions("a", None, before, 17, &CancellationToken::new())
            .unwrap();
        for version in page.versions {
            assert!(seen.insert(version.record.id));
        }
        before = page.before;
        if !page.has_more {
            break;
        }
    }
    assert_eq!(seen, expected);
    let page = store
        .fact_versions("a", None, None, 1, &CancellationToken::new())
        .unwrap();
    memory.remember_sync(fact("a", "concurrent", None)).unwrap();
    assert!(
        store
            .fact_versions("a", None, page.before, 1, &CancellationToken::new())
            .is_err()
    );
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(
        store
            .fact_versions("a", None, None, 50, &cancelled)
            .is_err()
    );
    assert!(
        store
            .fact_delete_preview(
                "b",
                FactDeleteKind::Version {
                    id: *expected.first().unwrap()
                },
                &CancellationToken::new()
            )
            .is_err()
    );
    let preview = store
        .fact_delete_preview("a", FactDeleteKind::Project, &CancellationToken::new())
        .unwrap();
    assert!(
        memory
            .delete_facts(
                FactDeleteRequest {
                    operation_id: Uuid::new_v4(),
                    preview
                },
                &cancelled
            )
            .is_err()
    );
    assert_eq!(
        store
            .fact_versions("a", None, None, 100, &CancellationToken::new())
            .unwrap()
            .versions
            .len(),
        100
    );
}
#[test]
fn selected_current_deletion_never_reactivates_predecessor_and_export_is_exact() {
    let (dir, store, memory) = setup();
    let a = fact("a", "old", None);
    memory.remember_sync(a.clone()).unwrap();
    let b = fact("a", "new", Some(a.id));
    memory.remember_sync(b.clone()).unwrap();
    memory.remember_sync(fact("a", "unrelated", None)).unwrap();
    memory.remember_sync(fact("b", "private", None)).unwrap();
    delete(&memory, &store, "a", FactDeleteKind::Version { id: b.id });
    let page = store
        .fact_versions("a", Some(a.id), None, 50, &CancellationToken::new())
        .unwrap();
    assert_eq!(page.versions.len(), 1);
    assert!(!page.versions[0].active);
    assert_eq!(store.memory_page("a", None, 50).unwrap().records.len(), 1);
    let destination = dir.path().join("chain.json");
    let result = crate::export_fact_versions(
        &dir.path().join("memory.db"),
        "a",
        &ExportSelection::Chain { id: a.id },
        &destination,
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(result.records, 1);
    let json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&destination).unwrap()).unwrap();
    assert_eq!(json["records"][0]["record"]["content"], "old");
    assert_eq!(json["schema_version"], 2);
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(
        crate::export_fact_versions(
            &dir.path().join("memory.db"),
            "a",
            &ExportSelection::Project,
            &destination,
            &cancel
        )
        .is_err()
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&std::fs::read(&destination).unwrap()).unwrap(),
        json
    );
}
#[test]
fn deleted_versions_leave_no_links_and_later_versions_are_reparented() {
    let (_dir, store, memory) = setup();
    let a = fact("a", "first", None);
    memory.remember_sync(a.clone()).unwrap();
    let b = fact("a", "second", Some(a.id));
    memory.remember_sync(b.clone()).unwrap();
    let c = fact("a", "third", Some(b.id));
    memory.remember_sync(c.clone()).unwrap();
    delete(&memory, &store, "a", FactDeleteKind::Version { id: b.id });
    memory.forget_scoped("a", a.id).unwrap();
    let links = store
        .call(|conn| {
            let mut statement = conn.prepare("SELECT id,parent FROM memory_fact_links")?;
            let rows = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap();
    assert_eq!(links, vec![(c.id.to_string(), None)]);
}
