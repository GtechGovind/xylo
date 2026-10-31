use super::*;
use crate::{DraftKey, DraftSave, DraftWrite};
use xylo_core::{PermissionMode, ProviderConfig};
fn fixture() -> (tempfile::TempDir, Store, DraftKey) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("queue.db")).unwrap();
    let key = DraftKey {
        session_id: Uuid::new_v4(),
        workspace: Some(dir.path().to_path_buf()),
    };
    (dir, store, key)
}
fn draft(store: &Store, key: &DraftKey, text: &str) -> DraftSubmission {
    let current = store.draft(key).unwrap();
    let DraftSave::Saved(saved) = store
        .save_draft(&DraftWrite {
            key: key.clone(),
            expected_revision: current.revision,
            mutation_id: Uuid::new_v4(),
            text: text.into(),
            attachments: vec![],
        })
        .unwrap()
    else {
        panic!("conflict")
    };
    DraftSubmission {
        key: key.clone(),
        expected_revision: saved.revision,
        text: saved.text,
        attachments: saved.attachments,
    }
}
fn turn(submission: &DraftSubmission) -> TurnRequest {
    TurnRequest {
        session_id: submission.key.session_id,
        turn_id: Uuid::new_v4(),
        prompt: submission.text.trim().into(),
        workspace: submission.key.workspace.clone().unwrap(),
        provider: ProviderConfig {
            model: "fixture-model".into(),
            ..ProviderConfig::default()
        },
        permissions: PermissionMode::ReadOnly,
        attachments: submission.attachments.clone(),
        approved_rules: Vec::new(),
    }
}
#[test]
fn exact_draft_moves_atomically_without_registering_user_message() {
    let (_dir, store, key) = fixture();
    let submission = draft(&store, &key, "  next task  ");
    let request = turn(&submission);
    let receipt = store.queue_turn(&request, &submission).unwrap();
    assert!(receipt.cleared);
    assert!(receipt.draft.is_empty());
    assert_eq!(receipt.draft.revision, submission.expected_revision + 1);
    assert_eq!(
        store.turn_queue(key.session_id).unwrap()[0].id,
        request.turn_id
    );
    assert!(store.conversations(10).unwrap().is_empty());
    let newer = draft(&store, &key, "newer text");
    let duplicate = store.queue_turn(&request, &submission).unwrap();
    assert!(!duplicate.cleared);
    assert_eq!(duplicate.draft.text, newer.text);
}
#[test]
fn lost_ack_reuses_only_its_owned_clear_revision() {
    let (_dir, store, key) = fixture();
    let sub = draft(&store, &key, "next");
    let req = turn(&sub);
    let first = store.queue_turn(&req, &sub).unwrap();
    assert!(first.cleared);
    let retry = turn(&sub);
    let receipt = store.queue_turn(&retry, &sub).unwrap();
    assert_eq!(receipt.item.id, req.turn_id);
    assert!(
        receipt.cleared,
        "canonical retry must acknowledge its owned clear"
    );
    draft(&store, &key, "newer draft");
    let empty = draft(&store, &key, "");
    let receipt = store.queue_turn(&retry, &sub).unwrap();
    assert!(
        !receipt.cleared,
        "a later empty mutation is not owned by this queue"
    );
    assert_eq!(receipt.draft.revision, empty.expected_revision);
    assert_eq!(store.turn_queue(key.session_id).unwrap().len(), 1);
}
#[test]
fn tombstones_do_not_count_against_caps_and_expire() {
    let (_dir, store, key) = fixture();
    for _ in 0..(MAX_SESSION * 2) {
        let sub = draft(&store, &key, "next");
        let req = turn(&sub);
        store.queue_turn(&req, &sub).unwrap();
        store
            .remove_queued_turn(key.session_id, req.turn_id)
            .unwrap();
    }
    let tombstones = |store: &Store| {
        store
            .call(|conn| {
                Ok(conn.query_row(
                    "SELECT count(*) FROM turn_queue WHERE state='removed'",
                    [],
                    |row| row.get::<_, i64>(0),
                )?)
            })
            .unwrap()
    };
    assert_eq!(tombstones(&store), MAX_SESSION * 2);
    let expired = xylo_core::now_ms() - TOMBSTONE_RETENTION_MS - 1;
    store
        .call(move |conn| {
            conn.execute("UPDATE turn_queue SET updated_at=?1", [expired])?;
            Ok(())
        })
        .unwrap();
    let sub = draft(&store, &key, "after purge");
    store.queue_turn(&turn(&sub), &sub).unwrap();
    assert_eq!(tombstones(&store), 0, "expired tombstones are purged");
    assert_eq!(store.turn_queue(key.session_id).unwrap().len(), 1);
}
#[test]
fn late_submission_preserves_newer_draft_and_rejects_mutated_identity() {
    let (_dir, store, key) = fixture();
    let old = draft(&store, &key, "old task");
    let newer = draft(&store, &key, "new draft");
    let request = turn(&old);
    let receipt = store.queue_turn(&request, &old).unwrap();
    assert!(!receipt.cleared);
    assert_eq!(receipt.draft.text, newer.text);
    let mut altered = request.clone();
    altered.provider.model = "another-model".into();
    assert!(store.queue_turn(&altered, &old).is_err());
    let mut same_revision = turn(&old);
    assert_eq!(
        store.queue_turn(&same_revision, &old).unwrap().item.id,
        request.turn_id
    );
    same_revision.prompt = "different".into();
    assert!(store.queue_turn(&same_revision, &old).is_err());
    assert_eq!(store.turn_queue(key.session_id).unwrap().len(), 1);
}
#[test]
fn fifo_caps_ownership_and_removed_tombstones() {
    let (_dir, store, key) = fixture();
    let mut ids = vec![];
    for index in 0..8 {
        let sub = draft(&store, &key, &format!("task {index}"));
        let req = turn(&sub);
        store.queue_turn(&req, &sub).unwrap();
        ids.push((req, sub));
    }
    assert_eq!(
        store
            .turn_queue(key.session_id)
            .unwrap()
            .iter()
            .map(|x| x.id)
            .collect::<Vec<_>>(),
        ids.iter().map(|x| x.0.turn_id).collect::<Vec<_>>()
    );
    let ninth = draft(&store, &key, "ninth");
    assert!(store.queue_turn(&turn(&ninth), &ninth).is_err());
    assert_eq!(store.draft(&key).unwrap().text, "ninth");
    assert!(
        store
            .remove_queued_turn(Uuid::new_v4(), ids[0].0.turn_id)
            .is_err()
    );
    assert!(
        store
            .queued_turn(Uuid::new_v4(), ids[0].0.turn_id)
            .unwrap()
            .is_none()
    );
    store
        .remove_queued_turn(key.session_id, ids[0].0.turn_id)
        .unwrap();
    assert!(
        store
            .queued_turn(key.session_id, ids[0].0.turn_id)
            .unwrap()
            .is_none()
    );
    assert!(store.queue_turn(&ids[0].0, &ids[0].1).is_err());
    assert_eq!(store.turn_queue(key.session_id).unwrap().len(), 7);
    for session_index in 0..7 {
        let mut other = key.clone();
        other.session_id = Uuid::new_v4();
        for index in 0..8 {
            let sub = draft(&store, &other, &format!("{session_index}/{index}"));
            store.queue_turn(&turn(&sub), &sub).unwrap();
        }
    }
    let sub = draft(&store, &key, "64th");
    store.queue_turn(&turn(&sub), &sub).unwrap();
    let mut other = key.clone();
    other.session_id = Uuid::new_v4();
    let sub = draft(&store, &other, "65th");
    assert!(store.queue_turn(&turn(&sub), &sub).is_err());
    assert_eq!(store.draft(&other).unwrap().text, "65th");
}
#[test]
fn restart_pauses_snapshots_and_resume_is_explicit() {
    let (dir, store, key) = fixture();
    let sub = draft(&store, &key, "next");
    let req = turn(&sub);
    store.queue_turn(&req, &sub).unwrap();
    drop(store);
    let store = Store::open(dir.path().join("queue.db")).unwrap();
    assert_eq!(store.recover_turn_queue().unwrap(), 1);
    let item = store
        .queued_turn(key.session_id, req.turn_id)
        .unwrap()
        .unwrap();
    assert!(item.paused);
    assert!(item.reason.unwrap().contains("restart"));
    store
        .set_queued_turn_paused(key.session_id, req.turn_id, false, None)
        .unwrap();
    assert!(!store.turn_queue(key.session_id).unwrap()[0].paused);
    store
        .pause_turn_queue(key.session_id, "Previous task failed")
        .unwrap();
    assert_eq!(
        store.turn_queue(key.session_id).unwrap()[0]
            .reason
            .as_deref(),
        Some("Previous task failed")
    );
    assert!(
        store
            .set_queued_turn_paused(Uuid::new_v4(), req.turn_id, false, None)
            .is_err()
    );
}
#[test]
fn newer_empty_draft_is_not_claimed_as_cleared_and_started_items_cannot_be_removed() {
    let (_dir, store, key) = fixture();
    let sub = draft(&store, &key, "old");
    draft(&store, &key, "");
    let req = turn(&sub);
    let receipt = store.queue_turn(&req, &sub).unwrap();
    assert!(
        !receipt.cleared,
        "a newer empty draft belongs to its own mutation"
    );
    assert!(
        store
            .complete_queued_turn(key.session_id, req.turn_id)
            .is_err()
    );
    store.register_user_turn(&req).unwrap();
    store
        .complete_queued_turn(key.session_id, req.turn_id)
        .unwrap();
    store
        .complete_queued_turn(key.session_id, req.turn_id)
        .unwrap();
    assert!(store.turn_queue(key.session_id).unwrap().is_empty());
    assert!(
        store
            .remove_queued_turn(key.session_id, req.turn_id)
            .unwrap_err()
            .to_string()
            .contains("already started")
    );
}
#[test]
fn admitted_ids_cannot_be_queued_and_conflicting_admission_cannot_consume_queue() {
    let (_dir, store, key) = fixture();
    let sub = draft(&store, &key, "task");
    let req = turn(&sub);
    store.register_user_turn(&req).unwrap();
    assert!(store.queue_turn(&req, &sub).is_err());
    assert_eq!(store.draft(&key).unwrap().text, "task");
    let (_dir, store, key) = fixture();
    let sub = draft(&store, &key, "task");
    let req = turn(&sub);
    store.queue_turn(&req, &sub).unwrap();
    let mut conflicting = req.clone();
    conflicting.prompt = "different".into();
    store.register_user_turn(&conflicting).unwrap();
    assert!(
        store
            .complete_queued_turn(key.session_id, req.turn_id)
            .is_err()
    );
    assert_eq!(store.turn_queue(key.session_id).unwrap().len(), 1);
}
#[test]
fn queued_admission_commits_message_and_started_receipt_together() {
    let (_dir, store, key) = fixture();
    let sub = draft(&store, &key, "next");
    let req = turn(&sub);
    store.queue_turn(&req, &sub).unwrap();
    let mut rewritten = req.clone();
    rewritten.prompt = "different input".into();
    assert!(
        store
            .admit_queued_turn(key.session_id, req.turn_id, &rewritten)
            .is_err()
    );
    assert!(store.conversations(10).unwrap().is_empty());
    let item = store
        .admit_queued_turn(key.session_id, req.turn_id, &req)
        .unwrap();
    assert_eq!(item.id, req.turn_id);
    assert!(store.turn_queue(key.session_id).unwrap().is_empty());
    assert_eq!(store.conversations(10).unwrap().len(), 1);
    assert!(
        store
            .remove_queued_turn(key.session_id, req.turn_id)
            .is_err()
    );
    assert!(
        store
            .admit_queued_turn(key.session_id, req.turn_id, &req)
            .is_err()
    );
}
#[test]
fn queued_admission_freezes_provider_identity_and_takes_current_permissions() {
    let (_dir, store, key) = fixture();
    let sub = draft(&store, &key, "next");
    let mut req = turn(&sub);
    req.permissions = PermissionMode::ApproveForMe;
    req.provider.managed_local = true;
    store.queue_turn(&req, &sub).unwrap();
    let mut changed = req.clone();
    changed.provider.model = "different".into();
    assert!(
        store
            .admit_queued_turn(key.session_id, req.turn_id, &changed)
            .is_err()
    );
    changed = req.clone();
    changed.provider.credential_ref = Some("another-account".into());
    assert!(
        store
            .admit_queued_turn(key.session_id, req.turn_id, &changed)
            .is_err()
    );
    changed = req.clone();
    changed.provider.endpoint = "https://remote.example/v1".into();
    assert!(
        store
            .admit_queued_turn(key.session_id, req.turn_id, &changed)
            .is_err()
    );
    // The permission mode in settings at admission is authoritative, in
    // either direction; only the provider snapshot is frozen.
    changed = req.clone();
    changed.permissions = PermissionMode::FullAccess;
    changed.provider.endpoint = "http://127.0.0.1:9099/v1".into();
    store
        .admit_queued_turn(key.session_id, req.turn_id, &changed)
        .unwrap();
    assert!(store.turn_queue(key.session_id).unwrap().is_empty());
}
#[test]
fn queued_admission_db_failure_keeps_snapshot_and_no_user_message() {
    let (_dir, store, key) = fixture();
    let sub = draft(&store, &key, "next");
    let req = turn(&sub);
    store.queue_turn(&req, &sub).unwrap();
    store.call(|conn| {conn.execute_batch("CREATE TRIGGER reject_admitted_message BEFORE INSERT ON messages BEGIN SELECT RAISE(ABORT,'injected admission failure'); END;")?;Ok(())}).unwrap();
    assert!(
        store
            .admit_queued_turn(key.session_id, req.turn_id, &req)
            .is_err()
    );
    assert_eq!(
        store.turn_queue(key.session_id).unwrap()[0].turn.prompt,
        "next"
    );
    assert!(store.conversations(10).unwrap().is_empty());
}
#[test]
fn clear_failure_rolls_back_enqueued_row_and_auth_secrets_are_rejected() {
    let (_dir, store, key) = fixture();
    let sub = draft(&store, &key, "keep");
    let mut req = turn(&sub);
    req.provider
        .headers
        .insert("Authorization".into(), "Bearer fixture-secret".into());
    assert!(store.queue_turn(&req, &sub).is_err());
    assert_eq!(store.draft(&key).unwrap().text, "keep");
    req.provider.headers.clear();
    store.turn_queue(key.session_id).unwrap();
    store.call(|conn| {conn.execute_batch("CREATE TRIGGER reject_draft_clear BEFORE UPDATE ON composer_drafts WHEN new.text='' BEGIN SELECT RAISE(ABORT,'injected clear failure'); END;")?;Ok(())}).unwrap();
    assert!(store.queue_turn(&req, &sub).is_err());
    assert_eq!(store.draft(&key).unwrap().text, "keep");
    assert!(store.turn_queue(key.session_id).unwrap().is_empty());
}
#[test]
fn attachment_snapshot_and_failed_commit_keep_user_input() {
    let (_dir, store, key) = fixture();
    let mut sub = draft(&store, &key, "");
    let image = xylo_core::ImageAttachment {
        id: Uuid::new_v4(),
        session_id: key.session_id,
        name: "same.png".into(),
        media_type: "image/png".into(),
        sha256: "a".repeat(64),
        bytes: 42,
        width: 8,
        height: 8,
        source: xylo_core::AttachmentSource::Picker,
        imported_at: 7,
    };
    let DraftSave::Saved(saved) = store
        .save_draft(&DraftWrite {
            key: key.clone(),
            expected_revision: sub.expected_revision,
            mutation_id: Uuid::new_v4(),
            text: String::new(),
            attachments: vec![image.clone()],
        })
        .unwrap()
    else {
        panic!()
    };
    sub.expected_revision = saved.revision;
    sub.attachments = vec![image.clone()];
    let req = turn(&sub);
    let receipt = store.queue_turn(&req, &sub).unwrap();
    assert!(receipt.cleared);
    assert_eq!(receipt.item.turn.attachments, vec![image]);
    let sub = draft(&store, &key, "must survive");
    let req = turn(&sub);
    store.call(|conn|{conn.execute_batch("CREATE TRIGGER reject_queue BEFORE INSERT ON turn_queue BEGIN SELECT RAISE(ABORT,'injected failure'); END;")?;Ok(())}).unwrap();
    assert!(store.queue_turn(&req, &sub).is_err());
    assert_eq!(store.draft(&key).unwrap().text, "must survive");
    assert!(
        store
            .queued_turn(key.session_id, req.turn_id)
            .unwrap()
            .is_none()
    );
}
