use super::*;
use xylo_core::{PermissionMode, ProviderConfig};
fn fixture() -> (tempfile::TempDir, Store, DraftKey) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("draft.db")).unwrap();
    let key = DraftKey {
        session_id: Uuid::new_v4(),
        workspace: Some(dir.path().to_path_buf()),
    };
    (dir, store, key)
}
fn request(key: &DraftKey, revision: i64, text: &str) -> DraftWrite {
    DraftWrite {
        key: key.clone(),
        expected_revision: revision,
        mutation_id: Uuid::new_v4(),
        text: text.into(),
        attachments: Vec::new(),
    }
}
fn saved(store: &Store, write: &DraftWrite) -> DraftSnapshot {
    match store.save_draft(write).unwrap() {
        DraftSave::Saved(snapshot) => snapshot,
        _ => panic!("Unexpected draft conflict"),
    }
}
fn make_turn(key: &DraftKey, text: &str) -> TurnRequest {
    TurnRequest {
        session_id: key.session_id,
        turn_id: Uuid::new_v4(),
        prompt: text.trim().into(),
        workspace: key.workspace.clone().unwrap(),
        provider: ProviderConfig::default(),
        permissions: PermissionMode::ReadOnly,
        attachments: Vec::new(),
    }
}
#[test]
fn drafts_survive_restart_without_conversation_rows_and_remain_workspace_scoped() {
    let (dir, store, key) = fixture();
    assert!(store.conversations(10).unwrap().is_empty());
    let first = saved(&store, &request(&key, 0, "αβ 🦀 unsent draft"));
    let mut other = key.clone();
    other.workspace = Some(dir.path().join("another"));
    saved(&store, &request(&other, 0, "Another project"));
    assert_eq!(store.draft(&key).unwrap().text, first.text);
    drop(store);
    let reopened = Store::open(dir.path().join("draft.db")).unwrap();
    assert_eq!(reopened.draft(&key).unwrap(), first);
    let page = reopened
        .drafts_page(key.workspace.clone(), None, 100)
        .unwrap();
    assert_eq!(page.drafts.len(), 1);
    assert_eq!(page.drafts[0].key, key);
    assert_eq!(page.drafts[0].preview, first.text);
    assert!(reopened.conversations(10).unwrap().is_empty());
}
#[test]
fn late_save_clear_and_duplicate_mutation_cannot_overwrite_newer_revision() {
    let (_dir, store, key) = fixture();
    let first = request(&key, 0, "First");
    let one = saved(&store, &first);
    assert_eq!(saved(&store, &first), one);
    let two = saved(&store, &request(&key, one.revision, "Newer input"));
    assert!(
        matches!(store.save_draft(&request(&key,one.revision,"")).unwrap(),DraftSave::Conflict(current) if current==two)
    );
    assert!(
        matches!(store.save_draft(&first).unwrap(),DraftSave::Conflict(current) if current==two)
    );
    let mut forged = request(&key, two.revision, "Forged mutation");
    forged.mutation_id = first.mutation_id;
    // Older identities cannot claim success; only the current mutation is retryable.
    let three = saved(&store, &forged);
    assert_eq!(three.revision, 3);
    forged.text = "Different payload with same identity".into();
    assert!(store.save_draft(&forged).is_err());
    let clear = saved(&store, &request(&key, three.revision, ""));
    assert!(clear.is_empty());
    assert!(
        matches!(store.save_draft(&request(&key,0,"Very late first write")).unwrap(),DraftSave::Conflict(current) if current==clear)
    );
}
#[test]
fn durable_admission_is_atomic_idempotent_and_preserves_newer_typing() {
    let (_dir, store, key) = fixture();
    let one = saved(&store, &request(&key, 0, " Send this αβ "));
    let turn = make_turn(&key, &one.text);
    let submission = DraftSubmission {
        key: key.clone(),
        expected_revision: one.revision,
        text: one.text.clone(),
        attachments: Vec::new(),
    };
    let admitted = store.admit_draft_turn(&turn, &submission).unwrap();
    assert!(admitted.cleared);
    assert!(admitted.draft.is_empty());
    assert_eq!(
        store.messages(key.session_id, 10).unwrap()[0].content,
        turn.prompt
    );
    let later = saved(
        &store,
        &request(
            &key,
            admitted.draft.revision,
            "New draft while previous send completes",
        ),
    );
    let repeat = store.admit_draft_turn(&turn, &submission).unwrap();
    assert!(!repeat.cleared);
    assert_eq!(repeat.draft, later);
    assert_eq!(store.messages(key.session_id, 10).unwrap().len(), 1);
    let another = make_turn(&key, &later.text);
    let pending = DraftSubmission {
        key: key.clone(),
        expected_revision: later.revision,
        text: later.text.clone(),
        attachments: Vec::new(),
    };
    let newest = saved(&store, &request(&key, later.revision, "Typed after send"));
    let outcome = store.admit_draft_turn(&another, &pending).unwrap();
    assert!(!outcome.cleared);
    assert_eq!(outcome.draft, newest);
    assert_eq!(store.messages(key.session_id, 10).unwrap().len(), 2);
}
#[test]
fn failed_or_mismatched_admission_does_not_clear_draft_or_create_user_history() {
    let (_dir, store, key) = fixture();
    let draft = saved(&store, &request(&key, 0, "Keep this"));
    let turn = make_turn(&key, "Wrong text");
    let submission = DraftSubmission {
        key: key.clone(),
        expected_revision: draft.revision,
        text: draft.text.clone(),
        attachments: Vec::new(),
    };
    assert!(store.admit_draft_turn(&turn, &submission).is_err());
    assert_eq!(store.draft(&key).unwrap(), draft);
    assert!(store.messages(key.session_id, 10).unwrap().is_empty());
    let valid = make_turn(&key, &draft.text);
    let mut other = submission.clone();
    other.key.workspace = None;
    assert!(store.admit_draft_turn(&valid, &other).is_err());
    assert_eq!(store.draft(&key).unwrap(), draft);
    // Trigger a failure after registration would have happened; both effects roll back.
    store.call(|conn|{conn.execute_batch("CREATE TRIGGER reject_clear BEFORE UPDATE ON composer_drafts WHEN new.text='' BEGIN SELECT RAISE(ABORT,'fixture commit failure'); END;")?;Ok(())}).unwrap();
    assert!(store.admit_draft_turn(&valid, &submission).is_err());
    assert_eq!(store.draft(&key).unwrap(), draft);
    assert!(store.messages(key.session_id, 10).unwrap().is_empty());
}
#[test]
fn explicit_caps_reject_oversized_payload_without_discarding_existing_content() {
    let (_dir, store, key) = fixture();
    let draft = saved(&store, &request(&key, 0, "Existing text"));
    assert!(
        store
            .save_draft(&request(
                &key,
                draft.revision,
                &"a".repeat(MAX_DRAFT_BYTES + 1)
            ))
            .is_err()
    );
    assert_eq!(store.draft(&key).unwrap(), draft);
    store
        .call(|conn| {
            conn.execute(
                "UPDATE composer_draft_usage SET total_bytes=?1",
                [MAX_DRAFT_STORAGE_BYTES as i64],
            )?;
            Ok(())
        })
        .unwrap();
    assert!(
        store
            .save_draft(&request(&key, draft.revision, "Larger than existing text"))
            .is_err()
    );
    assert_eq!(store.draft(&key).unwrap(), draft);
    store
        .call(|conn| {
            conn.execute(
                "UPDATE composer_draft_usage SET row_count=?1",
                [MAX_DRAFT_ROWS as i64],
            )?;
            Ok(())
        })
        .unwrap();
    let other = DraftKey {
        session_id: Uuid::new_v4(),
        workspace: key.workspace.clone(),
    };
    assert!(store.save_draft(&request(&other, 0, "New draft")).is_err());
    assert_eq!(store.draft(&other).unwrap().revision, 0);
}
#[test]
fn draft_listing_keysets_tied_timestamps_and_omits_empty_tombstones() {
    let (_dir, store, key) = fixture();
    for index in 0..7 {
        let k = DraftKey {
            session_id: Uuid::new_v4(),
            workspace: key.workspace.clone(),
        };
        saved(&store, &request(&k, 0, &format!("Draft {index}")));
    }
    saved(&store, &request(&key, 0, ""));
    store
        .call(|conn| {
            conn.execute("UPDATE composer_drafts SET updated_at=7", [])?;
            Ok(())
        })
        .unwrap();
    let mut before = None;
    let mut ids = std::collections::BTreeSet::new();
    loop {
        let page = store.drafts_page(key.workspace.clone(), before, 2).unwrap();
        assert!(page.drafts.len() <= 2);
        for draft in page.drafts {
            assert!(ids.insert(draft.key.session_id));
        }
        if !page.has_more {
            break;
        }
        before = page.before;
    }
    assert_eq!(ids.len(), 7);
}

#[test]
fn attachment_only_draft_survives_restart_and_cross_chat_refs_are_rejected() {
    let (dir, store, key) = fixture();
    let image = ImageAttachment {
        id: Uuid::new_v4(),
        session_id: key.session_id,
        name: "Fixture.png".into(),
        media_type: "image/png".into(),
        sha256: "a".repeat(64),
        bytes: 42,
        width: 8,
        height: 8,
        source: xylo_core::AttachmentSource::Picker,
        imported_at: 7,
    };
    let mut write = request(&key, 0, "");
    write.attachments.push(image.clone());
    let draft = saved(&store, &write);
    assert!(!draft.is_empty());
    let page = store.drafts_page(key.workspace.clone(), None, 100).unwrap();
    assert_eq!(page.drafts[0].attachment_count, 1);
    drop(store);
    let reopened = Store::open(dir.path().join("draft.db")).unwrap();
    assert_eq!(reopened.draft(&key).unwrap().attachments, vec![image]);
    let mut other = write;
    other.key.session_id = Uuid::new_v4();
    assert!(reopened.save_draft(&other).is_err());
    assert_eq!(reopened.draft(&key).unwrap(), draft);
}

#[test]
fn simultaneous_store_handles_cas_one_winner_without_overwriting_other_input() {
    let (dir, store, key) = fixture();
    let other = Store::open(dir.path().join("draft.db")).unwrap();
    let first = request(&key, 0, "One writer");
    let second = request(&key, 0, "Another writer");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let a = barrier.clone();
    let one = std::thread::spawn(move || {
        a.wait();
        store.save_draft(&first).unwrap()
    });
    let b = barrier.clone();
    let two = std::thread::spawn(move || {
        b.wait();
        other.save_draft(&second).unwrap()
    });
    barrier.wait();
    let outcomes = [one.join().unwrap(), two.join().unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, DraftSave::Saved(_)))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, DraftSave::Conflict(_)))
            .count(),
        1
    );
    let reopened = Store::open(dir.path().join("draft.db")).unwrap();
    assert_eq!(reopened.draft(&key).unwrap().revision, 1);
}
