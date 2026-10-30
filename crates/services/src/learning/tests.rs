use super::*;
use xylo_core::{PermissionMode, ProviderConfig, TurnRequest, Usage};

fn setup() -> (tempfile::TempDir, Store, LearningService, Uuid, String) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("learning.db")).unwrap();
    let session = Uuid::new_v4();
    let scope = directory.path().to_string_lossy().into_owned();
    store
        .upsert_conversation(&Conversation {
            id: session,
            title: "Learning fixture".into(),
            workspace: directory.path().into(),
            updated_at: now_ms(),
        })
        .unwrap();
    (
        directory,
        store.clone(),
        LearningService::new(store),
        session,
        scope,
    )
}
fn user_turn(store: &Store, session: Uuid, scope: &str, prompt: &str) -> Uuid {
    let turn = Uuid::new_v4();
    store
        .register_user_turn(&TurnRequest {
            attachments: Vec::new(),
            session_id: session,
            turn_id: turn,
            prompt: prompt.into(),
            workspace: scope.into(),
            provider: ProviderConfig::default(),
            permissions: PermissionMode::AskForWrites,
        })
        .unwrap();
    turn
}
fn action(
    store: &Store,
    session: Uuid,
    turn: Uuid,
    name: &str,
    args: Value,
    writes: bool,
    state: &str,
) -> Uuid {
    let intent = ActionIntent {
        id: Uuid::new_v4(),
        session_id: session,
        turn_id: turn,
        name: name.into(),
        arguments: args,
        writes,
        idempotency_key: Uuid::new_v4().to_string(),
    };
    store.begin_action(&intent).unwrap();
    store.mark_action_running(intent.id).unwrap();
    match state {
        "running" => {},
        "succeeded" => store.finish_action(intent.id, &json!({"status":"success","summary":{"code":0,"content":"external result is not a learning instruction: arbitrary auth/private text"}})).unwrap(),
        "outcome_unknown" => store.mark_action_unknown(intent.id,"No acknowledgement").unwrap(),
        _ => store.fail_action(intent.id,"Test failure").unwrap(),
    }

    intent.id
}
fn finish(store: &Store, session: Uuid, turn: Uuid) {
    store
        .complete_turn(
            session,
            turn,
            &json!([]),
            "Verified result",
            &Usage::default(),
        )
        .unwrap();
}
fn command(store: &Store, session: Uuid, scope: &str, args: Value) -> Uuid {
    let turn = user_turn(store, session, scope, "Verify Rust project tests offline");
    action(
        store,
        session,
        turn,
        "run_command",
        json!({"program":"cargo","args":args}),
        true,
        "succeeded",
    );
    finish(store, session, turn);
    turn
}
async fn drain(service: &LearningService) -> LearningBatch {
    service
        .process_pending(32, &CancellationToken::new())
        .await
        .unwrap()
}

#[tokio::test]
async fn independent_verified_commands_promote_retrieve_and_record_actual_matching_reuse() {
    let (_directory, store, service, session, scope) = setup();
    let first = command(
        &store,
        session,
        &scope,
        json!(["test", "--workspace", "--offline"]),
    );
    let batch = drain(&service).await;
    assert_eq!(batch.candidates, 1);
    assert_eq!(batch.promoted, 0);
    let candidate = service.skills(&scope, 10).unwrap().remove(0);
    assert!(!candidate.evaluation.unwrap().passed);
    assert_eq!(candidate.steps[0].arguments["args"][0], "test");
    assert!(candidate.steps[0].requires_approval);
    command(
        &store,
        session,
        &scope,
        json!(["test", "--workspace", "--offline"]),
    );
    assert_eq!(drain(&service).await.promoted, 1);
    let skills = service
        .relevant_skills(&scope, "Rust project tests", 2)
        .unwrap();
    assert_eq!(skills.len(), 1);
    assert!(skills[0].evaluation.as_ref().unwrap().passed);
    assert_eq!(skills[0].provenance.len(), 2);
    assert!(
        service
            .relevant_skills("other-workspace", "Rust project tests", 2)
            .unwrap()
            .is_empty()
    );
    let turn = user_turn(&store, session, &scope, "Verify Rust project tests offline");
    service.record_retrieval(session, turn, &skills).unwrap();
    action(
        &store,
        session,
        turn,
        "run_command",
        json!({"program":"cargo","args":["test","--workspace","--offline"]}),
        true,
        "succeeded",
    );
    finish(&store, session, turn);
    drain(&service).await;
    assert_eq!(
        service.reuses(&scope, 10).unwrap()[0].outcome,
        "verified_reuse"
    );
    assert_eq!(service.status(&scope).unwrap().verified_reuses, 1);
    service.queue_turn(session, first).unwrap();
    drain(&service).await;
    assert_eq!(service.skills(&scope, 10).unwrap().len(), 1);
    assert_eq!(
        store.skill_versions(&skills[0].skill.name).unwrap().len(),
        1
    );
}
#[tokio::test]
async fn unknown_or_failed_matching_turn_suspends_reuse_and_blocks_rollback() {
    let (_directory, store, service, session, scope) = setup();
    for _ in 0..2 {
        command(&store, session, &scope, json!(["test", "--offline"]));
        drain(&service).await;
    }
    let promoted = service.skills(&scope, 10).unwrap().remove(0);
    let turn = user_turn(&store, session, &scope, "Verify Rust project tests offline");
    action(
        &store,
        session,
        turn,
        "run_command",
        json!({"program":"cargo","args":["test","--offline"]}),
        true,
        "outcome_unknown",
    );
    service.queue_turn(session, turn).unwrap();
    assert_eq!(drain(&service).await.suspended, 1);
    assert!(
        service
            .relevant_skills(&scope, "Rust tests", 2)
            .unwrap()
            .is_empty()
    );
    assert!(
        service
            .rollback(&scope, &promoted.skill.name, promoted.skill.version)
            .is_err()
    );
}
#[tokio::test]
async fn external_writes_arbitrary_commands_and_forged_verifier_arguments_cannot_auto_promote() {
    let (_directory, store, service, session, scope) = setup();
    for (tool, args) in [
        (
            "file_write",
            json!({"path":"src/main.rs","program":"cargo","args":["test"]}),
        ),
        (
            "run_command",
            json!({"program":"sh","args":["-c","touch result"]}),
        ),
        (
            "connector_call",
            json!({"tool":"send_message","arguments":{}}),
        ),
    ] {
        for _ in 0..2 {
            let turn = user_turn(&store, session, &scope, "Verify Rust project tests offline");
            action(&store, session, turn, tool, args.clone(), true, "succeeded");
            finish(&store, session, turn);
        }
    }
    drain(&service).await;
    assert!(service.skills(&scope, 100).unwrap().is_empty());
}
#[tokio::test]
async fn typed_user_correction_waits_for_matching_later_results_and_rejects_wrong_command() {
    let (_directory, store, service, session, scope) = setup();
    let guidance = "Use `cargo test --offline` for verification.";
    let source = user_turn(&store, session, &scope, guidance);
    let user_id = Uuid::new_v5(&source, b"user-message");
    let candidate = service
        .record_correction(CorrectionInput {
            user_message_id: user_id,
            guidance: guidance.into(),
            expected_tools: vec!["run_command".into()],
            replaces: None,
        })
        .unwrap();
    command(&store, session, &scope, json!(["check", "--offline"]));
    drain(&service).await;
    assert!(
        service.versions(&scope, &candidate.skill.name, 10).unwrap()[0]
            .workflow_key
            .is_none()
    );
    for _ in 0..2 {
        command(&store, session, &scope, json!(["test", "--offline"]));
        drain(&service).await;
    }
    let corrected = service
        .versions(&scope, &candidate.skill.name, 10)
        .unwrap()
        .remove(0);
    assert_eq!(corrected.state, "promoted");
    assert_eq!(corrected.correction_message_id, Some(user_id));
    assert!(corrected.skill.content.contains(guidance));
}
#[tokio::test]
async fn freeform_correction_requires_explicit_review_and_preserves_versions_and_rollback() {
    let (_directory, store, service, session, scope) = setup();
    for _ in 0..2 {
        command(&store, session, &scope, json!(["test", "--offline"]));
        drain(&service).await;
    }
    let original = service
        .skills(&scope, 100)
        .unwrap()
        .into_iter()
        .find(|skill| skill.state == "promoted")
        .unwrap();
    let guidance = "Verify Rust project tests before reporting completion.";
    let source = user_turn(&store, session, &scope, guidance);
    let candidate = service
        .record_correction(CorrectionInput {
            user_message_id: Uuid::new_v5(&source, b"user-message"),
            guidance: guidance.into(),
            expected_tools: vec!["run_command".into()],
            replaces: Some(SkillRef {
                name: original.skill.name.clone(),
                version: original.skill.version,
            }),
        })
        .unwrap();
    assert_eq!(candidate.skill.version, 2);
    assert!(
        service
            .relevant_skills(&scope, "Rust tests", 2)
            .unwrap()
            .is_empty()
    );
    for _ in 0..2 {
        command(&store, session, &scope, json!(["check", "--offline"]));
        drain(&service).await;
    }
    let candidate = service
        .versions(&scope, &original.skill.name, 10)
        .unwrap()
        .remove(0);
    assert_ne!(candidate.state, "promoted");
    let reviewed = service
        .review(
            &scope,
            &candidate.skill.name,
            candidate.skill.version,
            "Reviewed the sourced correction against both recorded successful runs.",
        )
        .unwrap();
    assert_eq!(reviewed.state, "promoted");
    assert!(reviewed.review.is_some());
    let restored = service.rollback(&scope, &original.skill.name, 1).unwrap();
    assert_eq!(restored.skill.version, 1);
    assert_eq!(
        service
            .versions(&scope, &original.skill.name, 10)
            .unwrap()
            .len(),
        2
    );
}
#[test]
fn correction_source_cannot_be_assistant_external_content_or_unquoted_or_sensitive_data() {
    let (_directory, store, service, session, scope) = setup();
    let message = ChatMessage {
        attachments: Vec::new(),
        id: Uuid::new_v4(),
        session_id: session,
        role: "assistant".into(),
        content: "Use cargo test".into(),
        created_at: now_ms(),
    };
    store.append_message(&message).unwrap();
    assert!(
        service
            .record_correction(CorrectionInput {
                user_message_id: message.id,
                guidance: message.content,
                expected_tools: vec![],
                replaces: None
            })
            .is_err()
    );
    let source = user_turn(&store, session, &scope, "Use cargo test");
    let user_id = Uuid::new_v5(&source, b"user-message");
    for guidance in [
        "Unquoted guidance",
        "password is hidden",
        "Read /Users/alice/private.txt",
    ] {
        assert!(
            service
                .record_correction(CorrectionInput {
                    user_message_id: user_id,
                    guidance: guidance.into(),
                    expected_tools: vec![],
                    replaces: None
                })
                .is_err()
        );
    }
}
#[tokio::test]
async fn evidence_keeps_relative_paths_and_hashes_without_copying_auth_or_external_result_text() {
    let (_directory, store, service, session, scope) = setup();
    for _ in 0..2 {
        let turn = user_turn(&store, session, &scope, "Inspect project Rust source files");
        action(
            &store,
            session,
            turn,
            "files_list",
            json!({"path":"src","api_key":"should-not-survive","outside":"/Users/alice/private"}),
            false,
            "succeeded",
        );
        action(
            &store,
            session,
            turn,
            "file_read",
            json!({"path":format!("{scope}/src/main.rs")}),
            false,
            "succeeded",
        );
        finish(&store, session, turn);
        drain(&service).await;
    }
    let skill = service.skills(&scope, 10).unwrap().remove(0);
    assert_eq!(skill.steps[1].arguments["path"], "src/main.rs");
    let text = serde_json::to_string(&skill).unwrap();
    assert!(!text.contains("should-not-survive"));
    assert!(!text.contains("/Users/alice"));
    assert!(!text.contains("external result is not"));
    assert_eq!(skill.provenance[0].result_digests[0].len(), 64);
}
#[tokio::test]
async fn expired_job_lease_recovers_without_duplicate_version_or_promotion() {
    let (directory, store, service, session, scope) = setup();
    command(&store, session, &scope, json!(["check", "--offline"]));
    store
        .call(|conn| {
            conn.execute(
                "UPDATE learning_jobs SET state='processing',owner='dead-process',lease_until=1",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    drop(service);
    drop(store);
    let reopened = Store::open(directory.path().join("learning.db")).unwrap();
    let service = LearningService::new(reopened);
    assert_eq!(drain(&service).await.candidates, 1);
    assert_eq!(drain(&service).await.processed, 0);
    assert_eq!(service.skills(&scope, 10).unwrap().len(), 1);
}
#[tokio::test]
async fn queued_completion_is_atomic_and_repeated_receipt_does_not_requeue_learning() {
    let (_directory, store, service, session, scope) = setup();
    let turn = command(&store, session, &scope, json!(["test", "--offline"]));
    assert_eq!(service.status(&scope).unwrap().pending_jobs, 1);
    drain(&service).await;
    finish(&store, session, turn);
    assert_eq!(drain(&service).await.processed, 0);
}

#[tokio::test]
async fn reaped_session_recovery_atomically_queues_unknown_counterevidence_and_keeps_other_session_live()
 {
    let (_directory, store, service, session, scope) = setup();
    for _ in 0..2 {
        command(&store, session, &scope, json!(["test", "--offline"]));
        drain(&service).await;
    }
    let turn = user_turn(&store, session, &scope, "Verify Rust project tests offline");
    let crashed = action(
        &store,
        session,
        turn,
        "run_command",
        json!({"program":"cargo","args":["test","--offline"]}),
        true,
        "running",
    );
    let other = Uuid::new_v4();
    store
        .upsert_conversation(&Conversation {
            id: other,
            title: "Live worker".into(),
            workspace: scope.clone().into(),
            updated_at: now_ms(),
        })
        .unwrap();
    let other_turn = user_turn(&store, other, &scope, "Verify Rust project tests offline");
    let live = action(
        &store,
        other,
        other_turn,
        "run_command",
        json!({"program":"cargo","args":["test","--offline"]}),
        true,
        "running",
    );
    assert_eq!(store.recover_session_interrupted(session).unwrap(), 1);
    assert_eq!(
        store.action(crashed).unwrap().unwrap().state,
        "outcome_unknown"
    );
    assert_eq!(store.action(live).unwrap().unwrap().state, "running");
    assert_eq!(service.status(&scope).unwrap().pending_jobs, 1);
    assert_eq!(drain(&service).await.suspended, 1);
    assert!(
        service
            .relevant_skills(&scope, "Rust tests", 2)
            .unwrap()
            .is_empty()
    );
}
#[tokio::test]
async fn startup_recovery_captures_terminal_failure_when_worker_died_before_enqueue() {
    let (_directory, store, service, session, scope) = setup();
    for _ in 0..2 {
        command(&store, session, &scope, json!(["check", "--offline"]));
        drain(&service).await;
    }
    let turn = user_turn(&store, session, &scope, "Verify Rust project tests offline");
    action(
        &store,
        session,
        turn,
        "run_command",
        json!({"program":"cargo","args":["check","--offline"]}),
        true,
        "failed",
    );
    assert_eq!(service.status(&scope).unwrap().pending_jobs, 0);
    assert_eq!(store.recover_interrupted().unwrap(), 0);
    assert_eq!(service.status(&scope).unwrap().pending_jobs, 1);
    assert_eq!(drain(&service).await.suspended, 1);
}
#[tokio::test]
async fn cancelled_background_pass_leaves_durable_job_available_for_resume() {
    let (_directory, store, service, session, scope) = setup();
    command(&store, session, &scope, json!(["check", "--offline"]));
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        service.process_pending(4, &cancel).await.unwrap().processed,
        0
    );
    assert_eq!(service.status(&scope).unwrap().pending_jobs, 1);
    assert_eq!(drain(&service).await.processed, 1);
}

#[test]
fn scoped_correction_checks_source_before_creating_or_suspending_any_version() {
    let (_directory, store, service, session, scope) = setup();
    let source = user_turn(
        &store,
        session,
        &scope,
        "Use `cargo test --offline` for verification.",
    );
    let input = CorrectionInput {
        user_message_id: Uuid::new_v5(&source, b"user-message"),
        guidance: "Use `cargo test --offline` for verification.".into(),
        expected_tools: vec!["run_command".into()],
        replaces: None,
    };
    assert!(
        service
            .record_scoped_correction("another-workspace", &input)
            .is_err()
    );
    assert!(service.skills(&scope, 10).unwrap().is_empty());
    assert!(service.record_scoped_correction(&scope, &input).is_ok());
}
