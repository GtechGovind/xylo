use super::*;
use xylo_core::MemoryRecord;
fn record(scope: &str, text: &str) -> MemoryRecord {
    MemoryRecord {
        id: Uuid::new_v4(),
        scope: scope.into(),
        content: text.into(),
        source: "Explicit test entry".into(),
        created_at: 7,
        supersedes: None,
        origin: xylo_core::MemoryOrigin::UserTyped,
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
#[test]
fn saved_facts_are_redacted_and_returned_as_stored() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("mem.db")).unwrap();
    let memory = crate::SqliteMemory::new(store.clone());
    let mut entry = record(
        "project",
        "Deploy with DEPLOY_TOKEN=abcd1234efgh to staging",
    );
    entry.source = "Read from .env: api_key=zz9-plural".into();
    let stored = memory.remember_sync(entry.clone()).unwrap();
    assert_eq!(
        stored.content,
        "Deploy with DEPLOY_TOKEN=[REDACTED] to staging"
    );
    assert_eq!(stored.source, "Read from .env: api_key=[REDACTED]");
    // A retry of the unredacted record is the same save, not an id reuse.
    assert_eq!(memory.remember_sync(entry).unwrap().content, stored.content);
    let page = store.memory_page("project", None, 10).unwrap();
    assert!(!format!("{:?}", page.records).contains("abcd1234"));
}
#[test]
fn corrections_need_an_equally_trusted_origin_and_keep_the_old_version() {
    use xylo_core::MemoryOrigin;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("mem.db")).unwrap();
    let memory = crate::SqliteMemory::new(store.clone());
    let typed = record("project", "The release branch is main");
    memory.remember_sync(typed.clone()).unwrap();
    for origin in [
        MemoryOrigin::AgentInferred,
        MemoryOrigin::ToolOutput,
        MemoryOrigin::WebContent,
    ] {
        let mut attempt = record("project", "The release branch is next");
        attempt.supersedes = Some(typed.id);
        attempt.origin = origin;
        let error = memory.remember_sync(attempt).unwrap_err();
        assert!(error.to_string().contains("stated by the user"), "{error}");
    }
    let mut approved = record("project", "The release branch is release");
    approved.supersedes = Some(typed.id);
    approved.origin = MemoryOrigin::UserApproved;
    approved.created_at = 9;
    memory.remember_sync(approved.clone()).unwrap();
    let recalled = memory
        .recall_with_history("project", "release branch", 5)
        .unwrap();
    assert_eq!(recalled.len(), 1);
    assert_eq!(recalled[0].record.id, approved.id);
    let previously = recalled[0].previously.as_ref().unwrap();
    assert_eq!(previously.id, typed.id);
    assert_eq!(previously.content, "The release branch is main");
    assert_eq!(previously.origin, MemoryOrigin::UserTyped);
    assert_eq!(previously.until, 9);
    let mut inferred = record("project", "Agent fact");
    inferred.origin = MemoryOrigin::AgentInferred;
    memory.remember_sync(inferred.clone()).unwrap();
    let mut from_web = record("project", "Web correction");
    from_web.origin = MemoryOrigin::WebContent;
    from_web.supersedes = Some(inferred.id);
    assert!(memory.remember_sync(from_web).is_err());
}
#[test]
fn session_provenance_lists_actions_that_ran_with_requested_details_only() {
    use crate::ActionIntent;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("mem.db")).unwrap();
    let session = Uuid::new_v4();
    store
        .upsert_conversation(&xylo_core::Conversation {
            id: session,
            title: "Provenance".into(),
            workspace: dir.path().into(),
            updated_at: 1,
            title_source: xylo_core::TitleSource::Prompt,
            pinned: false,
            archived: false,
        })
        .unwrap();
    let turn = Uuid::new_v4();
    let begin = |key: &str, name: &str, arguments: serde_json::Value| {
        let intent = ActionIntent {
            id: Uuid::new_v4(),
            session_id: session,
            turn_id: turn,
            name: name.into(),
            arguments,
            writes: false,
            idempotency_key: key.into(),
        };
        store.begin_action(&intent).unwrap();
        intent.id
    };
    let read = begin("a", "file_read", serde_json::json!({"path":"/etc/hosts"}));
    store.mark_action_running(read).unwrap();
    store
        .finish_action(read, &serde_json::json!({"content":"large"}))
        .unwrap();
    let search = begin("b", "memory_search", serde_json::json!({"query":"x"}));
    store.mark_action_running(search).unwrap();
    store
        .finish_action(search, &serde_json::json!([{"origin":"web_content"}]))
        .unwrap();
    let denied = begin("c", "browser_open", serde_json::json!({}));
    store.deny_action(denied).unwrap();
    begin("d", "connector_call", serde_json::json!({}));
    let actions = store
        .session_action_provenance(session, &["file_read"], &["memory_search"])
        .unwrap();
    let names: Vec<_> = actions.iter().map(|action| action.name.as_str()).collect();
    assert_eq!(names, ["file_read", "memory_search"]);
    assert_eq!(actions[0].arguments.as_ref().unwrap()["path"], "/etc/hosts");
    assert!(actions[0].result.is_none());
    assert!(actions[1].arguments.is_none());
    assert_eq!(
        actions[1].result.as_ref().unwrap()[0]["origin"],
        "web_content"
    );
}
