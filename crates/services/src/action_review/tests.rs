use super::*;
use crate::{ActionDecision, ActionIntent};

struct Fixture {
    directory: tempfile::TempDir,
    store: Store,
    scope: ActionReviewScope,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().canonicalize().unwrap();
        let store = Store::open(workspace.join("workbench.db")).unwrap();
        let scope = ActionReviewScope {
            session_id: Uuid::new_v4(),
            workspace,
        };
        store
            .upsert_conversation(&Conversation {
                id: scope.session_id,
                title: "Recovery".into(),
                workspace: scope.workspace.clone(),
                updated_at: now_ms(),
            })
            .unwrap();
        Self {
            directory,
            store,
            scope,
        }
    }
    fn unknown(&self, name: &str, qualified: Option<bool>) -> Uuid {
        let id = Uuid::new_v4();
        let guard = qualified.map(|q| self.store.action_execution_lease(id, q).unwrap());
        let intent = ActionIntent {
            id,
            session_id: self.scope.session_id,
            turn_id: Uuid::new_v4(),
            name: name.into(),
            arguments: json!({"path":"partial.txt","content":"requested content","operation_id":Uuid::new_v4()}),
            writes: true,
            idempotency_key: format!("root:{id}"),
        };
        assert_eq!(
            self.store.begin_action(&intent).unwrap(),
            ActionDecision::Execute
        );
        self.store.mark_action_running(id).unwrap();
        self.store.mark_action_unknown_with_result(id,&json!({"status":"outcome_unknown","summary":"Only the first part may have been written","artifacts":[self.scope.workspace.join("partial.txt")]}),"Interrupted after a partial write").unwrap();
        drop(guard);
        id
    }
    fn request(&self, id: Uuid, outcome: ActionResolutionOutcome) -> ActionResolutionRequest {
        ActionResolutionRequest {
            operation_id: Uuid::new_v4(),
            scope: self.scope.clone(),
            action_id: id,
            expected_revision: self
                .store
                .action_review(self.scope.clone(), id)
                .unwrap()
                .action
                .revision,
            outcome,
            evidence: "I inspected partial.txt and the requested outcome.".into(),
        }
    }
}

#[test]
fn explicit_outcomes_preserve_original_observation_audit_and_restart_replay() {
    for outcome in [
        ActionResolutionOutcome::Completed,
        ActionResolutionOutcome::DidNotComplete,
    ] {
        let f = Fixture::new();
        let id = f.unknown("file_write", Some(true));
        let original = f.store.action(id).unwrap().unwrap();
        let request = f.request(id, outcome);
        let receipt = f
            .store
            .resolve_action(request.clone(), &CancellationToken::new())
            .unwrap();
        let action = f.store.action(id).unwrap().unwrap();
        let result = action.result.unwrap();
        assert_eq!(result["original_observation"], original.result.unwrap());
        assert_eq!(
            result["artifacts"][0],
            json!(f.scope.workspace.join("partial.txt"))
        );
        assert_eq!(
            result["operator_observation"]["native_execution_verified"],
            false
        );
        assert_eq!(
            result["status"],
            if outcome == ActionResolutionOutcome::Completed {
                "user_confirmed"
            } else {
                "failed"
            }
        );
        assert_eq!(
            action.state,
            if outcome == ActionResolutionOutcome::Completed {
                "succeeded"
            } else {
                "failed"
            }
        );
        let reopened = Store::open(f.directory.path().join("workbench.db")).unwrap();
        assert_eq!(
            reopened
                .resolve_action(request.clone(), &CancellationToken::new())
                .unwrap(),
            receipt
        );
        assert_eq!(
            reopened
                .action_resolution(f.scope.clone(), request.operation_id)
                .unwrap(),
            Some(receipt)
        );
        let audit_id = request.operation_id.to_string();
        reopened
            .call(move |conn| {
                let (raw, hash): (String, String) = conn.query_row(
                    "SELECT original,original_sha256 FROM action_resolutions WHERE operation_id=?1",
                    [&audit_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                assert_eq!(format!("{:x}", Sha256::digest(raw.as_bytes())), hash);
                assert_eq!(
                    serde_json::from_str::<JournalAction>(&raw)?.state,
                    "outcome_unknown"
                );
                assert!(
                    conn.execute(
                        "UPDATE action_resolutions SET original='changed' WHERE operation_id=?1",
                        [&audit_id]
                    )
                    .is_err()
                );
                assert!(
                    conn.execute(
                        "DELETE FROM action_resolutions WHERE operation_id=?1",
                        [audit_id]
                    )
                    .is_err()
                );
                Ok(())
            })
            .unwrap();
        let mut wrong = request;
        wrong.evidence.push_str(" changed");
        assert!(
            reopened
                .resolve_action(wrong, &CancellationToken::new())
                .is_err()
        );
    }
}

#[test]
fn wrong_chat_workspace_stale_revision_and_cancel_never_record_an_outcome() {
    let f = Fixture::new();
    let id = f.unknown("file_write", Some(true));
    let request = f.request(id, ActionResolutionOutcome::Completed);
    let other = Uuid::new_v4();
    f.store
        .upsert_conversation(&Conversation {
            id: other,
            title: "Other".into(),
            workspace: f.scope.workspace.clone(),
            updated_at: 0,
        })
        .unwrap();
    let mut wrong = request.clone();
    wrong.scope.session_id = other;
    assert!(
        f.store
            .resolve_action(wrong, &CancellationToken::new())
            .is_err()
    );
    let other_dir = tempfile::tempdir().unwrap();
    let mut wrong = request.clone();
    wrong.scope.workspace = other_dir.path().canonicalize().unwrap();
    assert!(
        f.store
            .resolve_action(wrong, &CancellationToken::new())
            .is_err()
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(f.store.resolve_action(request.clone(), &cancel).is_err());
    f.store
        .call(move |conn| {
            conn.execute(
                "UPDATE actions SET error='new evidence' WHERE id=?1",
                [id.to_string()],
            )?;
            Ok(())
        })
        .unwrap();
    assert!(
        f.store
            .resolve_action(request.clone(), &CancellationToken::new())
            .is_err()
    );
    assert!(
        f.store
            .action_resolution(f.scope.clone(), request.operation_id)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        f.store.action(id).unwrap().unwrap().state,
        "outcome_unknown"
    );
}

#[test]
fn actual_os_lease_blocks_resolution_and_legacy_or_native_ownership_stays_unqualified() {
    let f = Fixture::new();
    let id = f.unknown("file_write", Some(true));
    let request = f.request(id, ActionResolutionOutcome::Completed);
    let active = f.store.lock_action_execution(id).unwrap();
    assert!(
        f.store
            .action_review(f.scope.clone(), id)
            .unwrap()
            .action
            .busy
    );
    assert!(
        f.store
            .resolve_action(request.clone(), &CancellationToken::new())
            .is_err()
    );
    drop(active);
    assert!(
        !f.store
            .action_review(f.scope.clone(), id)
            .unwrap()
            .action
            .busy
    );
    f.store
        .resolve_action(request, &CancellationToken::new())
        .unwrap();
    for (name, protocol) in [
        ("file_write", None),
        ("run_command", Some(false)),
        ("environment_execute", Some(true)),
    ] {
        let id = f.unknown(name, protocol);
        let detail = f.store.action_review(f.scope.clone(), id).unwrap();
        assert!(!detail.action.busy);
        assert_eq!(
            detail.action.blocked_reason.as_deref(),
            Some("Verify this action in the service where it ran")
        );
        assert!(
            f.store
                .resolve_action(
                    f.request(id, ActionResolutionOutcome::Completed),
                    &CancellationToken::new()
                )
                .is_err()
        );
    }
}

#[test]
fn snapshot_pages_are_keyset_bounded_and_new_or_changed_rows_require_refresh() {
    let f = Fixture::new();
    let mut initial = Vec::new();
    for _ in 0..7 {
        initial.push(f.unknown("file_write", Some(true)));
    }
    f.store
        .call(|conn| {
            conn.execute("UPDATE actions SET updated_at=123", [])?;
            Ok(())
        })
        .unwrap();
    let first = f
        .store
        .action_reviews(f.scope.clone(), None, 2, &CancellationToken::new())
        .unwrap();
    assert_eq!(first.items.len(), 2);
    assert!(first.has_more);
    let newer = f.unknown("file_write", Some(true));
    let changed = initial
        .iter()
        .copied()
        .find(|id| first.items.iter().all(|item| item.id != *id))
        .unwrap();
    f.store
        .call(move |conn| {
            conn.execute(
                "UPDATE actions SET error='changed' WHERE id=?1",
                [changed.to_string()],
            )?;
            Ok(())
        })
        .unwrap();
    let mut all = first.items;
    let mut cursor = first.cursor;
    while cursor.is_some() {
        let page = f
            .store
            .action_reviews(f.scope.clone(), cursor, 2, &CancellationToken::new())
            .unwrap();
        all.extend(page.items);
        cursor = page.cursor;
    }
    let ids = all
        .iter()
        .map(|v| v.id)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(all.len(), ids.len());
    assert!(!ids.contains(&newer));
    assert!(!ids.contains(&changed));
    assert_eq!(ids.len(), 6);
    assert_eq!(
        f.store
            .action_reviews(f.scope.clone(), None, usize::MAX, &CancellationToken::new())
            .unwrap()
            .items
            .len(),
        8
    );
}

#[test]
fn large_observations_are_bounded_for_display_and_never_discarded_from_the_journal() {
    let f = Fixture::new();
    let id = f.unknown("file_write", Some(true));
    let value =
        json!({"status":"failed","summary":"x".repeat(1024*1024),"artifacts":["partial.txt"]});
    let stored = value.clone();
    f.store
        .call(move |conn| {
            conn.execute(
                "UPDATE actions SET result=?1 WHERE id=?2",
                params![stored.to_string(), id.to_string()],
            )?;
            Ok(())
        })
        .unwrap();
    let detail = f.store.action_review(f.scope.clone(), id).unwrap();
    assert!(detail.result.unwrap().truncated);
    assert!(
        serde_json::to_vec(&f.store.action_review(f.scope.clone(), id).unwrap())
            .unwrap()
            .len()
            < 32768
    );
    f.store
        .resolve_action(
            f.request(id, ActionResolutionOutcome::Completed),
            &CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(
        f.store.action(id).unwrap().unwrap().result.unwrap()["original_observation"],
        value
    );
}

#[test]
fn durable_review_form_cas_retains_exact_pending_identity_after_restart() {
    let f = Fixture::new();
    let id = f.unknown("file_write", Some(true));
    let request = f.request(id, ActionResolutionOutcome::DidNotComplete);
    let form = ActionReviewForm {
        selected: Some(id),
        evidence: request.evidence.clone(),
        outcome: Some(request.outcome),
        pending: Some(request.clone()),
    };
    let saved = f
        .store
        .save_action_review_form(ActionReviewFormWrite {
            scope: f.scope.clone(),
            expected_revision: 0,
            form: form.clone(),
        })
        .unwrap();
    assert!(saved.saved);
    let reopened = Store::open(f.directory.path().join("workbench.db")).unwrap();
    assert_eq!(
        reopened.action_review_form(f.scope.clone()).unwrap().form,
        form
    );
    let stale = reopened
        .save_action_review_form(ActionReviewFormWrite {
            scope: f.scope.clone(),
            expected_revision: 0,
            form: ActionReviewForm::default(),
        })
        .unwrap();
    assert!(!stale.saved);
    assert_eq!(stale.snapshot.form.pending, Some(request.clone()));

    let mut newer_note = form.clone();
    newer_note.evidence = "A newer note typed while the original acknowledgment was pending".into();
    let noted = reopened
        .save_action_review_form(ActionReviewFormWrite {
            scope: f.scope.clone(),
            expected_revision: saved.snapshot.revision,
            form: newer_note.clone(),
        })
        .unwrap();
    assert!(noted.saved);
    let mut cleared = newer_note.clone();
    cleared.pending = None;
    let mut replacement = newer_note.clone();
    replacement.pending.as_mut().unwrap().operation_id = Uuid::new_v4();
    let mut altered = newer_note.clone();
    altered
        .pending
        .as_mut()
        .unwrap()
        .evidence
        .push_str(" changed");
    for attempted in [cleared.clone(), replacement, altered] {
        assert!(
            reopened
                .save_action_review_form(ActionReviewFormWrite {
                    scope: f.scope.clone(),
                    expected_revision: noted.snapshot.revision,
                    form: attempted,
                })
                .is_err(),
            "A matching form CAS must not discard or replace an unresolved exact intent"
        );
        assert_eq!(
            reopened.action_review_form(f.scope.clone()).unwrap(),
            noted.snapshot
        );
    }

    let receipt = reopened
        .resolve_action(request.clone(), &CancellationToken::new())
        .unwrap();
    assert_eq!(receipt.operation_id, request.operation_id);
    let after_receipt = reopened
        .save_action_review_form(ActionReviewFormWrite {
            scope: f.scope.clone(),
            expected_revision: noted.snapshot.revision,
            form: cleared,
        })
        .unwrap();
    assert!(after_receipt.saved);
    assert!(after_receipt.snapshot.form.pending.is_none());
    assert_eq!(after_receipt.snapshot.form.evidence, newer_note.evidence);
    assert_eq!(
        Store::open(f.directory.path().join("workbench.db"))
            .unwrap()
            .action_review_form(f.scope.clone())
            .unwrap(),
        after_receipt.snapshot,
        "The confirmed-clear and newer user note must survive restart together"
    );
}

#[test]
fn striped_locks_are_stable_bounded_and_collisions_serialize_conservatively() {
    let f = Fixture::new();
    let mut stripes = std::collections::HashMap::new();
    let (id, other) = (1..=4097)
        .find_map(|number| {
            let id = Uuid::from_u128(number);
            let stripe = super::leases::stripe(id);
            stripes.insert(stripe, id).map(|previous| (previous, id))
        })
        .expect("4097 identities must collide in 4096 stripes");
    let lease = f.store.lock_action_execution(id).unwrap();
    assert!(f.store.lock_action_execution(other).is_err());
    drop(lease);
    drop(f.store.lock_action_execution(other).unwrap());
    assert_eq!(
        std::fs::read_dir(&*f.store.action_lease_root)
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn unqualified_legacy_and_native_actions_never_gain_completion_from_an_empty_stripe() {
    let fixture = Fixture::new();
    for (name, protocol) in [
        ("file_write", None),
        ("memory_remember", None),
        ("skill_propose", None),
        ("file_write", Some(false)),
        ("run_command", Some(true)),
        ("environment_execute", Some(true)),
        ("plugin_run_installed", Some(true)),
        ("document_save_copy", Some(true)),
        ("connector_call", Some(true)),
        ("image_generate", Some(true)),
    ] {
        let id = fixture.unknown(name, protocol);
        // Even a caller that wrongly marked a native action protocol-qualified
        // cannot turn a free local stripe into proof about its subprocess/server.
        let original = serde_json::to_value(fixture.store.action(id).unwrap().unwrap()).unwrap();
        drop(fixture.store.lock_action_execution(id).unwrap());
        let request = fixture.request(id, ActionResolutionOutcome::Completed);
        let detail = fixture
            .store
            .action_review(fixture.scope.clone(), id)
            .unwrap();
        assert!(
            !detail.action.busy,
            "{name}: unqualified is distinct from live"
        );
        assert!(detail.action.blocked_reason.is_some(), "{name}");
        assert!(
            fixture
                .store
                .resolve_action(request.clone(), &CancellationToken::new())
                .is_err(),
            "{name}"
        );
        assert_eq!(
            serde_json::to_value(fixture.store.action(id).unwrap().unwrap()).unwrap(),
            original,
            "Rejected review must preserve the full original action: {name}"
        );
        assert!(
            fixture
                .store
                .action_resolution(fixture.scope.clone(), request.operation_id)
                .unwrap()
                .is_none(),
            "{name}"
        );
    }
}

struct ChildLeaseHolder(std::process::Child);
impl Drop for ChildLeaseHolder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn pending_observation_fences_a_competing_operation_and_stale_admission_keeps_notes() {
    let fixture = Fixture::new();
    let id = fixture.unknown("file_write", Some(true));
    let first = fixture.request(id, ActionResolutionOutcome::Completed);
    let form = ActionReviewForm {
        selected: Some(id),
        evidence: first.evidence.clone(),
        outcome: Some(first.outcome),
        pending: Some(first.clone()),
    };
    fixture
        .store
        .save_action_review_form(ActionReviewFormWrite {
            scope: fixture.scope.clone(),
            expected_revision: 0,
            form: form.clone(),
        })
        .unwrap();
    let original = serde_json::to_value(fixture.store.action(id).unwrap().unwrap()).unwrap();
    let mut competing = first.clone();
    competing.operation_id = Uuid::new_v4();
    competing.evidence = "A competing window proposed another observation".into();
    assert!(
        fixture
            .store
            .resolve_action(competing.clone(), &CancellationToken::new())
            .is_err()
    );
    assert_eq!(
        serde_json::to_value(fixture.store.action(id).unwrap().unwrap()).unwrap(),
        original
    );
    assert!(
        fixture
            .store
            .action_resolution(fixture.scope.clone(), competing.operation_id)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        fixture
            .store
            .action_review_form(fixture.scope.clone())
            .unwrap()
            .form,
        form
    );
    let receipt = fixture
        .store
        .resolve_action(first.clone(), &CancellationToken::new())
        .unwrap();
    assert_eq!(receipt.operation_id, first.operation_id);
    assert!(
        fixture
            .store
            .resolve_action(competing, &CancellationToken::new())
            .is_err()
    );

    // The opposite transaction ordering must not admit an already stale intent:
    // another observation commits before this window persists its pending ID.
    let other = Fixture::new();
    let id = other.unknown("file_write", Some(true));
    let stale = other.request(id, ActionResolutionOutcome::Completed);
    let note = ActionReviewForm {
        selected: Some(id),
        evidence: "New typing that must survive a stale confirmation".into(),
        outcome: Some(stale.outcome),
        pending: None,
    };
    let saved_note = other
        .store
        .save_action_review_form(ActionReviewFormWrite {
            scope: other.scope.clone(),
            expected_revision: 0,
            form: note.clone(),
        })
        .unwrap();
    let mut winner = stale.clone();
    winner.operation_id = Uuid::new_v4();
    other
        .store
        .resolve_action(winner, &CancellationToken::new())
        .unwrap();
    let mut stale_form = note;
    stale_form.pending = Some(stale.clone());
    assert!(
        other
            .store
            .save_action_review_form(ActionReviewFormWrite {
                scope: other.scope.clone(),
                expected_revision: saved_note.snapshot.revision,
                form: stale_form,
            })
            .is_err()
    );
    assert_eq!(
        other.store.action_review_form(other.scope.clone()).unwrap(),
        saved_note.snapshot
    );
    assert!(
        other
            .store
            .action_resolution(other.scope.clone(), stale.operation_id)
            .unwrap()
            .is_none()
    );
}

#[test]
fn real_process_store_lease_blocks_review_until_owner_death_and_preserves_evidence() {
    use std::{
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    let fixture = Fixture::new();
    let id = fixture.unknown("file_write", Some(true));
    let request = fixture.request(id, ActionResolutionOutcome::Completed);
    let original = serde_json::to_value(fixture.store.action(id).unwrap().unwrap()).unwrap();
    let ready = fixture.directory.path().join("child-ready");
    let mut child = ChildLeaseHolder(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "action_review::tests::child_process_holds_store_execution_lease",
                "--ignored",
                "--nocapture",
            ])
            .env(
                "WORKBENCH_LEASE_TEST_DATABASE",
                fixture.directory.path().join("workbench.db"),
            )
            .env("WORKBENCH_LEASE_TEST_ACTION", id.to_string())
            .env("WORKBENCH_LEASE_TEST_READY", &ready)
            .env("RUST_BACKTRACE", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "Lease-holder child stopped before reporting readiness"
        );
        assert!(
            Instant::now() < deadline,
            "Lease-holder child did not become ready"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let second = Store::open(fixture.directory.path().join("workbench.db")).unwrap();
    let detail = second.action_review(fixture.scope.clone(), id).unwrap();
    assert!(detail.action.busy);
    assert!(second.lock_action_execution(id).is_err());
    assert!(
        second
            .resolve_action(request.clone(), &CancellationToken::new())
            .is_err()
    );
    assert_eq!(
        serde_json::to_value(second.action(id).unwrap().unwrap()).unwrap(),
        original
    );
    assert!(
        second
            .action_resolution(fixture.scope.clone(), request.operation_id)
            .unwrap()
            .is_none()
    );

    child.0.kill().unwrap();
    let status = child.0.wait().unwrap();
    assert!(
        !status.success(),
        "The owner was terminated rather than cooperatively releasing its lease"
    );
    assert!(
        !second
            .action_review(fixture.scope.clone(), id)
            .unwrap()
            .action
            .busy
    );
    let receipt = second
        .resolve_action(request.clone(), &CancellationToken::new())
        .unwrap();
    assert_eq!(receipt.source, "operator_review");
    let reviewed = second.action(id).unwrap().unwrap();
    let result = reviewed.result.unwrap();
    assert_eq!(result["status"], "user_confirmed");
    assert_eq!(result["original_observation"], original["result"]);
    assert_eq!(result["artifacts"], original["result"]["artifacts"]);
    assert_eq!(
        result["operator_observation"]["native_execution_verified"],
        false
    );
    assert_eq!(
        second
            .resolve_action(request, &CancellationToken::new())
            .unwrap(),
        receipt
    );
}

#[test]
#[ignore = "Subprocess helper invoked only by the bounded owner-death regression"]
fn child_process_holds_store_execution_lease() {
    let database =
        PathBuf::from(std::env::var_os("WORKBENCH_LEASE_TEST_DATABASE").expect("Child database"));
    let id = Uuid::parse_str(&std::env::var("WORKBENCH_LEASE_TEST_ACTION").expect("Child action"))
        .unwrap();
    let ready = PathBuf::from(
        std::env::var_os("WORKBENCH_LEASE_TEST_READY").expect("Child readiness file"),
    );
    let store = Store::open(database).unwrap();
    let _lease = store.lock_action_execution(id).unwrap();
    std::fs::write(ready, b"lease held").unwrap();
    // The parent normally terminates this process. A fixed upper lifetime also
    // prevents an orphaned test child holding authority after a parent crash.
    std::thread::sleep(std::time::Duration::from_secs(15));
}

#[cfg(unix)]
#[test]
fn action_lease_symlinks_and_hardlinks_are_rejected_without_following_them() {
    use std::os::unix::{fs::PermissionsExt, fs::symlink};
    let f = Fixture::new();
    let id = Uuid::from_bytes([8; 16]);
    drop(f.store.lock_action_execution(id).unwrap());
    let slot = std::fs::read_dir(&*f.store.action_lease_root)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::remove_file(&slot).unwrap();
    let target = f.directory.path().join("unrelated");
    std::fs::write(&target, "preserved").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
    symlink(&target, &slot).unwrap();
    assert!(f.store.lock_action_execution(id).is_err());
    std::fs::remove_file(&slot).unwrap();
    std::fs::hard_link(&target, &slot).unwrap();
    assert!(f.store.lock_action_execution(id).is_err());
    assert_eq!(std::fs::read_to_string(target).unwrap(), "preserved");
}
