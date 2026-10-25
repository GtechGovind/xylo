use super::*;
use workbench_core::MemoryRecord;
fn record(scope: &str, text: &str) -> MemoryRecord {
    MemoryRecord {
        id: Uuid::new_v4(),
        scope: scope.into(),
        content: text.into(),
        source: "Explicit test entry".into(),
        created_at: 7,
        supersedes: None,
    }
}
#[test]
fn correction_is_idempotent_and_rejects_stale_branches_and_cross_scope_deletion() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("mem.db")).unwrap();
    let memory = crate::SqliteMemory::new(store.clone());
    let original = record("project-a", "original choice");
    memory.remember_sync(original.clone()).unwrap();
    let mut correction = record("project-a", "corrected choice");
    correction.supersedes = Some(original.id);
    memory.remember_sync(correction.clone()).unwrap();
    memory.remember_sync(correction.clone()).unwrap();
    memory.remember_sync(original.clone()).unwrap(); // retry must not reactivate it
    let mut branch = record("project-a", "stale correction");
    branch.supersedes = Some(original.id);
    assert!(memory.remember_sync(branch).is_err());
    let mut changed = correction.clone();
    changed.content = "ID reused".into();
    assert!(memory.remember_sync(changed).is_err());
    assert!(memory.forget_scoped("project-b", correction.id).is_err());
    let active = store.memory_page("project-a", None, 50).unwrap();
    assert_eq!(active.records.len(), 1);
    assert_eq!(active.records[0].id, correction.id);
    memory.forget_scoped("project-a", correction.id).unwrap();
    memory.forget_scoped("project-a", correction.id).unwrap();
    assert!(
        store
            .memory_page("project-a", None, 50)
            .unwrap()
            .records
            .is_empty()
    );
}
#[test]
fn tied_timestamp_pages_stay_scoped_and_export_retains_version_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("mem.db");
    let store = Store::open(&database).unwrap();
    let memory = crate::SqliteMemory::new(store.clone());
    let mut ids = std::collections::BTreeSet::new();
    for n in 0..113 {
        let entry = record("chosen", &format!("memory {n} αβ 🦀"));
        ids.insert(entry.id);
        memory.remember_sync(entry).unwrap();
    }
    memory
        .remember_sync(record("another", "private outside scope"))
        .unwrap();
    let mut seen = std::collections::BTreeSet::new();
    let mut before = None;
    loop {
        let page = store.memory_page("chosen", before, 23).unwrap();
        assert!(page.records.len() <= 23);
        for record in page.records {
            assert_eq!(record.scope, "chosen");
            assert!(seen.insert(record.id));
        }
        before = page.before;
        if !page.has_more {
            break;
        }
    }
    assert_eq!(seen, ids);
    let old = record("chosen", "old export version");
    memory.remember_sync(old.clone()).unwrap();
    let mut new = record("chosen", "new export version");
    new.supersedes = Some(old.id);
    memory.remember_sync(new).unwrap();
    let destination = dir.path().join("export.json");
    let receipt =
        export_memories(&database, "chosen", &destination, &CancellationToken::new()).unwrap();
    assert_eq!(receipt.records, 115);
    let json: serde_json::Value =
        serde_json::from_reader(File::open(destination).unwrap()).unwrap();
    let rows = json["records"].as_array().unwrap();
    assert_eq!(rows.len(), 115);
    assert!(rows.iter().all(|row| row["record"]["scope"] == "chosen"));
    assert!(
        rows.iter()
            .any(|row| row["active"] == false && row["record"]["id"] == old.id.to_string())
    );
}
#[test]
fn cancelled_export_leaves_existing_file_and_database_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("mem.db");
    let store = Store::open(&database).unwrap();
    crate::SqliteMemory::new(store.clone())
        .remember_sync(record("scope", "retained memory"))
        .unwrap();
    let destination = dir.path().join("keep.json");
    std::fs::write(&destination, "keep these bytes").unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(export_memories(&database, "scope", &destination, &cancel).is_err());
    assert_eq!(
        std::fs::read_to_string(destination).unwrap(),
        "keep these bytes"
    );
    assert_eq!(
        store.memory_page("scope", None, 1).unwrap().records.len(),
        1
    );
}

#[test]
fn export_accepts_valid_content_that_expands_when_json_escaped() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("mem.db");
    let store = Store::open(&database).unwrap();
    let content = "a\n".repeat(131_000);
    let record = record("scope", &content);
    crate::SqliteMemory::new(store)
        .remember_sync(record)
        .unwrap();
    let destination = dir.path().join("escaped.json");
    let exported =
        export_memories(&database, "scope", &destination, &CancellationToken::new()).unwrap();
    assert_eq!(exported.records, 1);
    let json: serde_json::Value =
        serde_json::from_reader(File::open(destination).unwrap()).unwrap();
    assert_eq!(
        json["records"][0]["record"]["content"].as_str().unwrap(),
        content
    );
}
