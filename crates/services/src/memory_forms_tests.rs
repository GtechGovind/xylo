use super::*;
#[test]
fn legacy_encoded_row_bound_migrates_atomically_without_losing_revision_or_text() {
    let (_dir, store) = setup();
    store.call(|conn|{conn.execute_batch("CREATE TABLE memory_forms(scope TEXT PRIMARY KEY,json TEXT NOT NULL CHECK(length(CAST(json AS BLOB))<=614400),revision INTEGER NOT NULL CHECK(revision>0),updated_at INTEGER NOT NULL,mutation_id TEXT NOT NULL,expected INTEGER NOT NULL);")?;conn.execute("INSERT INTO memory_forms VALUES('a',?1,7,99,?2,6)",params![r#"{"text":"legacy unsaved entry","editing":null,"retry":null}"#,Uuid::new_v4().to_string()])?;Ok(())}).unwrap();
    let snapshot = store.memory_form("a").unwrap();
    assert_eq!(snapshot.form.text, "legacy unsaved entry");
    assert_eq!(snapshot.revision, 7);
    assert_eq!(snapshot.updated_at, 99);
    let text = "\n".repeat(262144);
    let record = MemoryRecord {
        id: Uuid::new_v4(),
        scope: "a".into(),
        content: text.clone(),
        source: "explicit".into(),
        created_at: 1,
        supersedes: None,
    };
    let mut request = write("a", &text, 7);
    request.form.retry = Some(record);
    assert_eq!(saved(store.save_memory_form(request).unwrap()).revision, 8);
}
#[test]
fn restart_does_not_clear_new_whitespace_after_a_committed_save() {
    let (_dir, store) = setup();
    let record = MemoryRecord {
        id: Uuid::new_v4(),
        scope: "a".into(),
        content: "saved".into(),
        source: "explicit".into(),
        created_at: 1,
        supersedes: None,
    };
    let mut request = write("a", "saved ", 0);
    request.form.retry = Some(record.clone());
    request.form.retry_input = Some("saved".into());
    saved(store.save_memory_form(request).unwrap());
    crate::SqliteMemory::new(store.clone())
        .remember_sync(record)
        .unwrap();
    let recovered = store.memory_form("a").unwrap();
    assert_eq!(recovered.form.text, "saved ");
    assert!(recovered.form.retry.is_none());
}
fn setup() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("memory.db")).unwrap();
    (dir, store)
}
fn write(scope: &str, text: &str, revision: i64) -> MemoryFormWrite {
    MemoryFormWrite {
        scope: scope.into(),
        expected_revision: revision,
        mutation_id: Uuid::new_v4(),
        form: MemoryFormState {
            text: text.into(),
            editing: None,
            retry: None,
            retry_input: None,
        },
    }
}
fn saved(result: MemoryFormSave) -> MemoryFormSnapshot {
    match result {
        MemoryFormSave::Saved(snapshot) => snapshot,
        _ => panic!("Unexpected form conflict"),
    }
}
#[test]
fn forms_restore_independently_and_empty_tombstones_fence_old_saves() {
    let (dir, store) = setup();
    let a = write("a", "unsaved αβ 🦀", 0);
    let initial = saved(store.save_memory_form(a.clone()).unwrap());
    assert_eq!(initial.revision, 1);
    assert_eq!(
        saved(store.save_memory_form(a.clone()).unwrap()).revision,
        1
    );
    saved(
        store
            .save_memory_form(write("b", "different unsaved entry", 0))
            .unwrap(),
    );
    let restarted = Store::open(dir.path().join("memory.db")).unwrap();
    assert_eq!(
        restarted.memory_form("a").unwrap().form.text,
        "unsaved αβ 🦀"
    );
    assert_eq!(
        restarted.memory_form("b").unwrap().form.text,
        "different unsaved entry"
    );
    saved(restarted.save_memory_form(write("a", "", 1)).unwrap());
    assert!(matches!(
        store.save_memory_form(a).unwrap(),
        MemoryFormSave::Conflict(_)
    ));
    assert!(store.memory_form("a").unwrap().form.text.is_empty());
    assert!(
        store
            .memory_page("a", None, 100)
            .unwrap()
            .records
            .is_empty()
    );
}
#[test]
fn concurrent_writers_conflict_without_overwriting_or_reusing_an_identity() {
    let (dir, store) = setup();
    let request = write("a", "one", 0);
    saved(store.save_memory_form(request.clone()).unwrap());
    let other = Store::open(dir.path().join("memory.db")).unwrap();
    let latest = saved(other.save_memory_form(write("a", "two", 1)).unwrap());
    assert!(
        matches!(store.save_memory_form(write("a","stale",1)).unwrap(),MemoryFormSave::Conflict(snapshot)if snapshot.revision==latest.revision&&snapshot.form.text=="two")
    );
    let mut reused = write("a", "three", latest.revision);
    let prior = reused.clone();
    saved(store.save_memory_form(prior).unwrap());
    reused.form.text = "other".into();
    assert!(store.save_memory_form(reused).is_err());
    assert_eq!(store.memory_form("a").unwrap().form.text, "three");
}
#[test]
fn restart_reconciles_fact_commit_without_losing_newer_typing() {
    let (dir, store) = setup();
    let memory = crate::SqliteMemory::new(store.clone());
    let original = MemoryRecord {
        id: Uuid::new_v4(),
        scope: "a".into(),
        content: "old".into(),
        source: "explicit".into(),
        created_at: 1,
        supersedes: None,
    };
    memory.remember_sync(original.clone()).unwrap();
    let correction = MemoryRecord {
        id: Uuid::new_v4(),
        scope: "a".into(),
        content: "corrected".into(),
        source: "explicit".into(),
        created_at: 2,
        supersedes: Some(original.id),
    };
    let mut form = write("a", "corrected", 0);
    form.form.editing = Some(original.id);
    form.form.retry = Some(correction.clone());
    form.form.retry_input = Some("corrected".into());
    saved(store.save_memory_form(form).unwrap());
    // Fact commit succeeds, but its UI acknowledgement is lost in a crash.
    memory.remember_sync(correction.clone()).unwrap();
    let restarted = Store::open(dir.path().join("memory.db")).unwrap();
    let recovered = restarted.memory_form("a").unwrap();
    assert!(recovered.form.text.is_empty());
    assert_eq!(recovered.recovered_commit, Some(correction.id));
    assert_eq!(restarted.memory_form("a").unwrap().recovered_commit, None);
    let next = MemoryRecord {
        id: Uuid::new_v4(),
        scope: "a".into(),
        content: "next correction".into(),
        source: "explicit".into(),
        created_at: 3,
        supersedes: Some(correction.id),
    };
    let mut form = write("a", "newer typing after clicking Save", recovered.revision);
    form.form.editing = Some(correction.id);
    form.form.retry = Some(next.clone());
    form.form.retry_input = Some("next correction".into());
    saved(store.save_memory_form(form).unwrap());
    memory.remember_sync(next.clone()).unwrap();
    let restored = restarted.memory_form("a").unwrap();
    assert_eq!(restored.form.text, "newer typing after clicking Save");
    assert_eq!(restored.form.editing, Some(next.id));
    assert!(restored.form.retry.is_none());
}
#[test]
fn forms_keep_unconfirmed_retry_and_support_worst_case_valid_json_escaping() {
    let (_dir, store) = setup();
    let content = "\n".repeat(262144);
    let retry = MemoryRecord {
        id: Uuid::new_v4(),
        scope: "a".into(),
        content: content.clone(),
        source: "explicit".into(),
        created_at: 1,
        supersedes: None,
    };
    let mut form = write("a", &content, 0);
    form.form.retry = Some(retry.clone());
    saved(store.save_memory_form(form).unwrap());
    let restored = store.memory_form("a").unwrap();
    assert_eq!(restored.form.text, content);
    assert_eq!(restored.form.retry.unwrap().id, retry.id);
    assert!(
        store
            .save_memory_form(write("b", &"x".repeat(262145), 0))
            .is_err()
    );
    let mut wrong = write("b", "text", 0);
    wrong.form.retry = Some(retry);
    assert!(store.save_memory_form(wrong).is_err());
}
#[test]
fn fact_purge_never_erases_unsaved_forms_or_composer_tables() {
    let (_dir, store) = setup();
    let memory = crate::SqliteMemory::new(store.clone());
    let record = MemoryRecord {
        id: Uuid::new_v4(),
        scope: "a".into(),
        content: "saved fact".into(),
        source: "explicit".into(),
        created_at: 1,
        supersedes: None,
    };
    memory.remember_sync(record).unwrap();
    saved(
        store
            .save_memory_form(write("a", "keep unsaved entry", 0))
            .unwrap(),
    );
    let preview = store
        .fact_delete_preview(
            "a",
            crate::FactDeleteKind::Project,
            &tokio_util::sync::CancellationToken::new(),
        )
        .unwrap();
    memory
        .delete_facts(
            crate::FactDeleteRequest {
                operation_id: Uuid::new_v4(),
                preview,
            },
            &tokio_util::sync::CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(
        store.memory_form("a").unwrap().form.text,
        "keep unsaved entry"
    );
}
