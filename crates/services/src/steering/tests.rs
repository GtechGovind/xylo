use super::*;
use crate::{DraftKey, DraftSave, DraftWrite};
use xylo_core::Conversation;
fn fixture() -> (tempfile::TempDir, Store, SteeringRequest, DraftSubmission) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("store.db")).unwrap();
    let session = Uuid::new_v4();
    let turn = Uuid::new_v4();
    store
        .upsert_conversation(&Conversation {
            id: session,
            title: "Active task".into(),
            workspace: directory.path().into(),
            updated_at: 7,
        })
        .unwrap();
    store
        .save_checkpoint(
            session,
            turn,
            &json!([{"role":"user","content":"Original task"}]),
        )
        .unwrap();
    let key = DraftKey {
        session_id: session,
        workspace: Some(directory.path().into()),
    };
    let write = DraftWrite {
        key: key.clone(),
        expected_revision: 0,
        mutation_id: Uuid::new_v4(),
        text: " Use the verified option αβ 🦀 ".into(),
        attachments: Vec::new(),
    };
    let DraftSave::Saved(saved) = store.save_draft(&write).unwrap() else {
        panic!("Unexpected conflict")
    };
    let guidance = SteeringRequest {
        id: Uuid::new_v4(),
        session_id: session,
        turn_id: turn,
        text: saved.text.trim().into(),
    };
    let submission = DraftSubmission {
        key,
        expected_revision: saved.revision,
        text: saved.text,
        attachments: Vec::new(),
    };
    (directory, store, guidance, submission)
}
#[test]
fn queue_is_durable_and_aliases_of_same_revision_never_repeat_applied_guidance() {
    let (directory, store, guidance, submission) = fixture();
    let queued = store.queue_steering(&guidance, &submission).unwrap();
    assert_eq!(queued.receipt.state, SteeringState::Queued);
    assert_eq!(store.draft(&submission.key).unwrap().text, submission.text);
    drop(store);
    let store = Store::open(directory.path().join("store.db")).unwrap();
    assert_eq!(
        store.steering(guidance.id).unwrap().unwrap().receipt.state,
        SteeringState::Queued
    );
    let applied = store
        .apply_steering(
            guidance.session_id,
            guidance.turn_id,
            &json!([{"role":"user","content":"Original task"}]),
        )
        .unwrap();
    assert_eq!(applied.applied.len(), 1);
    assert_eq!(applied.applied[0].receipt.state, SteeringState::Applied);
    assert!(applied.applied[0].cleared_revision.is_some());
    assert!(store.draft(&submission.key).unwrap().is_empty());
    assert_eq!(applied.messages.as_array().unwrap().len(), 2);
    assert_eq!(store.messages(guidance.session_id, 10).unwrap().len(), 1);
    let mut alias = guidance.clone();
    alias.id = Uuid::new_v4();
    let reused = store.queue_steering(&alias, &submission).unwrap();
    assert_eq!(reused.request.id, guidance.id);
    assert_eq!(reused.receipt.state, SteeringState::Applied);
    let repeated = store
        .apply_steering(guidance.session_id, guidance.turn_id, &applied.messages)
        .unwrap();
    assert!(repeated.applied.is_empty());
    assert_eq!(repeated.messages, applied.messages);
    assert_eq!(store.messages(guidance.session_id, 10).unwrap().len(), 1);
    drop(store);
    let reopened = Store::open(directory.path().join("store.db")).unwrap();
    let replay = reopened
        .steering_for_turn(guidance.session_id, Some(guidance.turn_id), 64)
        .unwrap();
    assert_eq!(replay[0].receipt.state, SteeringState::Applied);
    assert_eq!(replay[0].cleared_revision, reused.cleared_revision);
    assert_eq!(
        reopened
            .checkpoint(guidance.session_id)
            .unwrap()
            .unwrap()
            .messages,
        applied.messages
    );
}
#[test]
fn newer_typing_wins_guidance_clear_without_preventing_checkpoint_application() {
    let (_directory, store, guidance, submission) = fixture();
    store.queue_steering(&guidance, &submission).unwrap();
    let DraftSave::Saved(newer) = store
        .save_draft(&DraftWrite {
            key: submission.key.clone(),
            expected_revision: submission.expected_revision,
            mutation_id: Uuid::new_v4(),
            text: "A newer unsent message".into(),
            attachments: Vec::new(),
        })
        .unwrap()
    else {
        panic!("Unexpected conflict")
    };
    let outcome = store
        .apply_steering(guidance.session_id, guidance.turn_id, &json!([]))
        .unwrap();
    assert_eq!(outcome.applied.len(), 1);
    assert_eq!(outcome.applied[0].cleared_revision, None);
    assert_eq!(store.draft(&submission.key).unwrap(), newer);
}
#[test]
fn queued_cancel_failure_and_forged_identity_leave_original_draft_and_no_false_application() {
    let (_directory, store, guidance, submission) = fixture();
    store.queue_steering(&guidance, &submission).unwrap();
    let mut different = guidance.clone();
    different.text = "Different instruction".into();
    assert!(store.queue_steering(&different, &submission).is_err());
    let ended = store
        .finish_pending_steering(
            guidance.session_id,
            Some(guidance.turn_id),
            "Worker stopped",
        )
        .unwrap();
    assert_eq!(ended[0].receipt.state, SteeringState::Cancelled);
    assert_eq!(store.draft(&submission.key).unwrap().text, submission.text);
    assert!(
        store
            .apply_steering(guidance.session_id, guidance.turn_id, &json!([]))
            .unwrap()
            .applied
            .is_empty()
    );
    assert!(store.messages(guidance.session_id, 10).unwrap().is_empty());
}
#[test]
fn checkpoint_and_transcript_and_clear_rollback_together_on_storage_failure() {
    let (_directory, store, guidance, submission) = fixture();
    store.queue_steering(&guidance, &submission).unwrap();
    let original = store
        .checkpoint(guidance.session_id)
        .unwrap()
        .unwrap()
        .messages;
    store.call(|conn|{conn.execute_batch("CREATE TRIGGER reject_guidance_clear BEFORE UPDATE ON composer_drafts WHEN new.text='' BEGIN SELECT RAISE(ABORT,'fixture storage failure'); END;")?;Ok(())}).unwrap();
    assert!(
        store
            .apply_steering(guidance.session_id, guidance.turn_id, &original)
            .is_err()
    );
    assert_eq!(
        store.steering(guidance.id).unwrap().unwrap().receipt.state,
        SteeringState::Queued
    );
    assert_eq!(
        store
            .checkpoint(guidance.session_id)
            .unwrap()
            .unwrap()
            .messages,
        original
    );
    assert!(store.messages(guidance.session_id, 10).unwrap().is_empty());
    assert_eq!(store.draft(&submission.key).unwrap().text, submission.text);
}
#[test]
fn wrong_target_checkpoint_cannot_apply_or_clear_guidance() {
    let (_directory, store, guidance, submission) = fixture();
    store.queue_steering(&guidance, &submission).unwrap();
    store
        .save_checkpoint(guidance.session_id, Uuid::new_v4(), &json!([]))
        .unwrap();
    assert!(
        store
            .apply_steering(guidance.session_id, guidance.turn_id, &json!([]))
            .is_err()
    );
    assert_eq!(
        store.steering(guidance.id).unwrap().unwrap().receipt.state,
        SteeringState::Queued
    );
    assert_eq!(store.draft(&submission.key).unwrap().text, submission.text);
}

#[test]
fn startup_recovery_cancels_only_unapplied_guidance_and_keeps_its_draft() {
    let (directory, store, applied, submission) = fixture();
    store.queue_steering(&applied, &submission).unwrap();
    store
        .apply_steering(applied.session_id, applied.turn_id, &json!([]))
        .unwrap();
    let empty = store.draft(&submission.key).unwrap();
    let DraftSave::Saved(saved) = store
        .save_draft(&DraftWrite {
            key: submission.key.clone(),
            expected_revision: empty.revision,
            mutation_id: Uuid::new_v4(),
            text: "Pending when application stopped".into(),
            attachments: Vec::new(),
        })
        .unwrap()
    else {
        panic!("Unexpected conflict")
    };
    let pending = SteeringRequest {
        id: Uuid::new_v4(),
        text: saved.text.clone(),
        ..applied.clone()
    };
    store
        .queue_steering(
            &pending,
            &DraftSubmission {
                key: submission.key.clone(),
                expected_revision: saved.revision,
                text: saved.text.clone(),
                attachments: Vec::new(),
            },
        )
        .unwrap();
    drop(store);
    let store = Store::open(directory.path().join("store.db")).unwrap();
    assert_eq!(store.recover_steering().unwrap(), 1);
    assert_eq!(store.recover_steering().unwrap(), 0);
    assert_eq!(
        store.steering(applied.id).unwrap().unwrap().receipt.state,
        SteeringState::Applied
    );
    assert_eq!(
        store.steering(pending.id).unwrap().unwrap().receipt.state,
        SteeringState::Cancelled
    );
    assert_eq!(store.draft(&submission.key).unwrap(), saved);
    let checkpoint = store.checkpoint(applied.session_id).unwrap().unwrap();
    assert!(checkpoint.messages.to_string().contains(&applied.text));
    assert!(!checkpoint.messages.to_string().contains(&pending.text));
}
