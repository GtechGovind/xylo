use super::*;
use xylo_core::DocumentKind;

fn key(root: &Path, name: &str) -> DocumentDraftKey {
    DocumentDraftKey {
        workspace: root.to_path_buf(),
        source: root.join(name),
    }
}
fn state(key: &DocumentDraftKey, text: &str) -> DocumentDraftState {
    DocumentDraftState {
        source: DocumentSource {
            path: key.source.clone(),
            revision: "a".repeat(64),
            bytes: 42,
            kind: DocumentKind::Text,
        },
        edits: vec![DocumentEdit::Text { text: text.into() }],
        export_name: "report-copy".into(),
        sheet: None,
        selected: None,
        pending_copy: None,
        pending_relink: None,
    }
}
fn write(key: &DocumentDraftKey, revision: i64, text: &str) -> DocumentDraftWrite {
    DocumentDraftWrite {
        key: key.clone(),
        expected_revision: revision,
        mutation_id: Uuid::new_v4(),
        state: Some(state(key, text)),
    }
}
fn saved(result: DocumentDraftSave) -> DocumentDraftSnapshot {
    match result {
        DocumentDraftSave::Saved(snapshot) => snapshot,
        DocumentDraftSave::Conflict(_) => panic!("unexpected conflict"),
    }
}

#[test]
fn restart_preserves_original_revision_edits_and_pending_copy_identity() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let db = root.join("state.db");
    let doc = key(&root, "report.txt");
    let store = Store::open(&db).unwrap();
    let mut request = write(&doc, 0, "\nNew & exact text\t");
    let intent = DocumentCopyIntent {
        operation_id: Uuid::new_v4(),
        generation: 3,
        name: "report-copy".into(),
        edits: request.state.as_ref().unwrap().edits.clone(),
    };
    request.state.as_mut().unwrap().pending_copy = Some(intent.clone());
    let original = saved(store.save_document_draft(request.clone()).unwrap());
    drop(store);
    let reopened = Store::open(&db).unwrap();
    let restored = reopened.document_draft(doc).unwrap();
    assert_eq!(
        serde_json::to_value(&restored).unwrap(),
        serde_json::to_value(&original).unwrap()
    );
    assert_eq!(
        restored.state.unwrap().pending_copy.unwrap().operation_id,
        intent.operation_id
    );
    assert_eq!(
        saved(reopened.save_document_draft(request).unwrap()).revision,
        1
    );
    assert!(
        !root.join("report.txt").exists(),
        "saving a draft must not create or overwrite its source"
    );
}

#[test]
fn concurrent_writers_and_clear_tombstones_fence_stale_edits() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let db = root.join("state.db");
    let one = Store::open(&db).unwrap();
    let two = Store::open(&db).unwrap();
    let doc = key(&root, "draft.md");
    saved(one.save_document_draft(write(&doc, 0, "one")).unwrap());
    let DocumentDraftSave::Conflict(current) =
        two.save_document_draft(write(&doc, 0, "two")).unwrap()
    else {
        panic!("stale writer overwrote current draft")
    };
    assert_eq!(current.revision, 1);
    let cleared = saved(
        one.save_document_draft(DocumentDraftWrite {
            key: doc.clone(),
            expected_revision: 1,
            mutation_id: Uuid::new_v4(),
            state: None,
        })
        .unwrap(),
    );
    assert!(cleared.state.is_none());
    assert_eq!(cleared.revision, 2);
    assert!(
        matches!(two.save_document_draft(write(&doc,1,"stale text")).unwrap(),DocumentDraftSave::Conflict(snapshot) if snapshot.revision==2 && snapshot.state.is_none())
    );
    assert!(
        one.document_drafts(root, None, 20)
            .unwrap()
            .drafts
            .is_empty()
    );
}

#[test]
fn mutation_identity_cannot_be_reused_for_different_content() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let store = Store::open(root.join("state.db")).unwrap();
    let mut request = write(&key(&root, "draft.txt"), 0, "first");
    saved(store.save_document_draft(request.clone()).unwrap());
    request.state.as_mut().unwrap().export_name = "second".into();
    assert!(
        store
            .save_document_draft(request)
            .unwrap_err()
            .to_string()
            .contains("identity")
    );
}

#[test]
fn saved_document_list_pages_with_exact_project_scope() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let store = Store::open(root.join("state.db")).unwrap();
    let project = root.join("a");
    let other = root.join("b");
    for name in ["one.txt", "two.txt", "three.txt"] {
        saved(
            store
                .save_document_draft(write(&key(&project, name), 0, name))
                .unwrap(),
        );
    }
    saved(
        store
            .save_document_draft(write(&key(&other, "foreign.txt"), 0, "private"))
            .unwrap(),
    );
    let first = store.document_drafts(project.clone(), None, 2).unwrap();
    assert_eq!(first.workspace, project);
    assert_eq!(first.drafts.len(), 2);
    assert!(first.has_more);
    let second = store.document_drafts(project, first.before, 2).unwrap();
    assert_eq!(second.drafts.len(), 1);
    assert!(!second.has_more);
    assert!(
        first
            .drafts
            .iter()
            .all(|draft| draft.key != second.drafts[0].key)
    );
    assert!(
        first
            .drafts
            .iter()
            .chain(second.drafts.iter())
            .all(|draft| !draft.key.source.ends_with("foreign.txt"))
    );
}

#[test]
fn oversized_or_inconsistent_edits_never_replace_saved_draft() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let store = Store::open(root.join("state.db")).unwrap();
    let doc = key(&root, "draft.txt");
    let original = saved(
        store
            .save_document_draft(write(&doc, 0, "recover me"))
            .unwrap(),
    );
    let mut invalid = write(&doc, 1, "other");
    invalid.state.as_mut().unwrap().source.path = root.join("different.txt");
    assert!(store.save_document_draft(invalid).is_err());
    let oversized = write(&doc, 1, &"\0".repeat(MAX_DOCUMENT_DRAFT_BYTES / 6 + 1));
    assert!(store.save_document_draft(oversized).is_err());
    assert_eq!(
        serde_json::to_value(store.document_draft(doc).unwrap()).unwrap(),
        serde_json::to_value(original).unwrap()
    );
}

#[test]
fn storage_accounting_tracks_replacement_and_clear_without_deleting_other_drafts() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let store = Store::open(root.join("state.db")).unwrap();
    let doc = key(&root, "draft.txt");
    saved(
        store
            .save_document_draft(write(&doc, 0, &"x".repeat(10000)))
            .unwrap(),
    );
    saved(
        store
            .save_document_draft(write(&key(&root, "other.txt"), 0, "keep"))
            .unwrap(),
    );
    saved(
        store
            .save_document_draft(write(&doc, 1, "smaller"))
            .unwrap(),
    );
    saved(
        store
            .save_document_draft(DocumentDraftWrite {
                key: doc,
                expected_revision: 2,
                mutation_id: Uuid::new_v4(),
                state: None,
            })
            .unwrap(),
    );
    let (usage, actual, rows): (i64, i64, i64) = store
        .call(|conn| {
            Ok((
                conn.query_row(
                    "SELECT bytes FROM document_draft_usage WHERE singleton=1",
                    [],
                    |row| row.get(0),
                )?,
                conn.query_row(
                    "SELECT sum(length(CAST(json AS BLOB))) FROM document_drafts",
                    [],
                    |row| row.get(0),
                )?,
                conn.query_row(
                    "SELECT rows FROM document_draft_usage WHERE singleton=1",
                    [],
                    |row| row.get(0),
                )?,
            ))
        })
        .unwrap();
    assert_eq!(usage, actual);
    assert_eq!(rows, 2);
    assert_eq!(
        store.document_drafts(root, None, 10).unwrap().drafts.len(),
        1
    );
}
