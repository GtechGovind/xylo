use super::*;
use crate::{ActionResolutionOutcome, ActionReviewForm, ActionReviewFormWrite};
use xylo_services::ActionIntent;

fn seed(host: &HostState) -> (ActionReviewScope, Uuid) {
    let scope = ActionReviewScope {
        session_id: Uuid::new_v4(),
        workspace: host.paths.data.canonicalize().unwrap(),
    };
    host.store
        .upsert_conversation(&Conversation {
            id: scope.session_id,
            title: "Observed recovery".into(),
            workspace: scope.workspace.clone(),
            updated_at: now_ms(),
        })
        .unwrap();
    let id = Uuid::new_v4();
    let guard = host.store.action_execution_lease(id, true).unwrap();
    host.store
        .begin_action(&ActionIntent {
            id,
            session_id: scope.session_id,
            turn_id: Uuid::new_v4(),
            name: "file_write".into(),
            arguments: serde_json::json!({"path":"partial.txt","content":"requested"}),
            writes: true,
            idempotency_key: format!("root:{id}"),
        })
        .unwrap();
    host.store.mark_action_running(id).unwrap();
    std::fs::write(
        scope.workspace.join("partial.txt"),
        "retained partial effect",
    )
    .unwrap();
    host.store.mark_action_unknown_with_result(id,&serde_json::json!({"status":"failed","artifacts":[scope.workspace.join("partial.txt")]}),"Lost acknowledgement").unwrap();
    drop(guard);
    (scope, id)
}
fn request(host: &HostState, scope: ActionReviewScope, id: Uuid) -> ActionResolutionRequest {
    ActionResolutionRequest {
        operation_id: Uuid::new_v4(),
        expected_revision: host
            .store
            .action_review(scope.clone(), id)
            .unwrap()
            .action
            .revision,
        scope,
        action_id: id,
        outcome: ActionResolutionOutcome::DidNotComplete,
        evidence: "I inspected the retained partial file.".into(),
    }
}
fn client() -> (HostClient, Receiver<HostCommand>) {
    let (commands, incoming) = async_channel::bounded(16);
    (
        HostClient {
            commands,
            discovery: Default::default(),
        },
        incoming,
    )
}
async fn dispatch(host: &Arc<HostState>, incoming: &Receiver<HostCommand>) {
    let command = incoming.recv().await.unwrap();
    if host.prepare_command(&command).await.unwrap() {
        host.handle(command).await.unwrap();
    }
}

#[tokio::test]
async fn typed_host_review_lost_ack_exact_replay_forms_and_wrong_scope_are_correlated() {
    let (_dir, host, events) = super::super::tests::state();
    let (scope, id) = seed(&host);
    let (client, incoming) = client();
    let list_id = Uuid::new_v4();
    client
        .action_reviews(list_id, scope.clone(), None, 1)
        .unwrap();
    dispatch(&host, &incoming).await;
    match events.recv().await.unwrap() {
        HostEvent::ActionReviews { request_id, result } => {
            assert_eq!(request_id, list_id);
            assert_eq!(result.unwrap().items[0].id, id)
        }
        _ => panic!("Expected correlated review page"),
    }
    let proposed = request(&host, scope.clone(), id);
    let form = ActionReviewForm {
        selected: Some(id),
        evidence: proposed.evidence.clone(),
        outcome: Some(proposed.outcome),
        pending: Some(proposed.clone()),
    };
    let save_id = Uuid::new_v4();
    client
        .save_action_review_form(
            save_id,
            ActionReviewFormWrite {
                scope: scope.clone(),
                expected_revision: 0,
                form,
            },
        )
        .unwrap();
    dispatch(&host, &incoming).await;
    assert!(
        matches!(events.recv().await.unwrap(),HostEvent::ActionReviewFormSaved{request_id,result:Ok(_)}if request_id==save_id)
    );
    let resolve_id = Uuid::new_v4();
    client.resolve_action(resolve_id, proposed.clone()).unwrap();
    dispatch(&host, &incoming).await;
    let committed = match events.recv().await.unwrap() {
        HostEvent::ActionResolved { request_id, result } => {
            assert_eq!(request_id, resolve_id);
            result.unwrap()
        }
        _ => panic!("Expected resolution receipt"),
    };
    // The first acknowledgement could have been lost at the UI boundary. A read
    // and exact replay return the durable receipt, even while later work queues.
    host.queued
        .lock()
        .insert(scope.session_id, (Uuid::new_v4(), CancellationToken::new()));
    let replay_id = Uuid::new_v4();
    client.resolve_action(replay_id, proposed.clone()).unwrap();
    dispatch(&host, &incoming).await;
    match events.recv().await.unwrap() {
        HostEvent::ActionResolved { request_id, result } => {
            assert_eq!(request_id, replay_id);
            assert_eq!(result.unwrap(), committed)
        }
        _ => panic!("Expected idempotent receipt"),
    }
    let lookup_id = Uuid::new_v4();
    client
        .action_resolution(lookup_id, scope.clone(), proposed.operation_id)
        .unwrap();
    dispatch(&host, &incoming).await;
    match events.recv().await.unwrap() {
        HostEvent::ActionResolution { request_id, result } => {
            assert_eq!(request_id, lookup_id);
            assert_eq!(result.unwrap(), Some(committed))
        }
        _ => panic!("Expected lookup"),
    }
    assert_eq!(
        std::fs::read_to_string(scope.workspace.join("partial.txt")).unwrap(),
        "retained partial effect"
    );
    host.queued.lock().remove(&scope.session_id);
    let other = tempfile::tempdir().unwrap();
    let wrong = ActionReviewScope {
        workspace: other.path().canonicalize().unwrap(),
        ..scope
    };
    let wrong_id = Uuid::new_v4();
    client.action_review(wrong_id, wrong, id).unwrap();
    dispatch(&host, &incoming).await;
    assert!(
        matches!(events.recv().await.unwrap(),HostEvent::ActionReview{request_id,result}if request_id==wrong_id&&result.is_err())
    );
}

#[tokio::test]
async fn host_busy_and_cancelled_resolution_preserve_unknown_and_have_no_receipt() {
    let (_dir, host, events) = super::super::tests::state();
    let (scope, id) = seed(&host);
    let (client, incoming) = client();
    let proposed = request(&host, scope.clone(), id);
    host.queued
        .lock()
        .insert(scope.session_id, (Uuid::new_v4(), CancellationToken::new()));
    let busy_id = Uuid::new_v4();
    client.resolve_action(busy_id, proposed.clone()).unwrap();
    dispatch(&host, &incoming).await;
    assert!(
        matches!(events.recv().await.unwrap(),HostEvent::ActionResolved{request_id,result:Err(_)}if request_id==busy_id)
    );
    host.queued.lock().remove(&scope.session_id);
    let cancel_id = Uuid::new_v4();
    client.resolve_action(cancel_id, proposed.clone()).unwrap();
    let command = incoming.recv().await.unwrap();
    assert!(host.prepare_command(&command).await.unwrap());
    client.cancel_capability(cancel_id).unwrap();
    let cancel = incoming.recv().await.unwrap();
    assert!(!host.prepare_command(&cancel).await.unwrap());
    host.handle(command).await.unwrap();
    assert!(
        matches!(events.recv().await.unwrap(),HostEvent::ActionResolved{request_id,result:Err(_)}if request_id==cancel_id)
    );
    assert!(
        host.store
            .action_resolution(scope, proposed.operation_id)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        host.store.action(id).unwrap().unwrap().state,
        "outcome_unknown"
    );
}

#[tokio::test]
async fn wrong_project_turn_admission_preserves_history_checkpoint_and_original_scope() {
    let (_dir, host, events) = super::super::tests::state();
    let (scope, action) = seed(&host);
    let conversation = host.store.conversation(scope.session_id).unwrap().unwrap();
    let turn_id = Uuid::new_v4();
    let checkpoint = serde_json::json!([{"role":"user","content":"original checkpoint"}]);
    host.store
        .save_checkpoint(scope.session_id, turn_id, &checkpoint)
        .unwrap();
    let history = host.store.messages(scope.session_id, 100).unwrap();
    let other = tempfile::tempdir().unwrap();
    let (client, incoming) = client();
    client
        .send_to(
            scope.session_id,
            WorkerCommand::Run {
                turn: TurnRequest {
                    session_id: scope.session_id,
                    turn_id: Uuid::new_v4(),
                    workspace: other.path().canonicalize().unwrap(),
                    prompt: "Must not redirect the existing chat".into(),
                    // A deliberately invalid provider proves rejection precedes
                    // provider setup, worker creation and native/model dispatch.
                    provider: ProviderConfig {
                        endpoint: "invalid fixture endpoint".into(),
                        ..Default::default()
                    },
                    permissions: PermissionMode::ReadOnly,
                    attachments: vec![],
                },
            },
        )
        .unwrap();
    let command = incoming.recv().await.unwrap();
    assert!(host.prepare_command(&command).await.unwrap());
    let error = host.handle(command).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("chat belongs to another project")
    );
    assert!(
        host.queued.lock().is_empty(),
        "Admission must release its queue guard"
    );
    assert!(host.workers.lock().await.is_empty());
    assert!(host.helpers.lock().await.is_empty());
    assert!(events.try_recv().is_err());
    assert_eq!(
        host.store
            .conversation(scope.session_id)
            .unwrap()
            .unwrap()
            .workspace,
        conversation.workspace
    );
    assert_eq!(
        serde_json::to_value(host.store.messages(scope.session_id, 100).unwrap()).unwrap(),
        serde_json::to_value(history).unwrap()
    );
    let retained = host.store.checkpoint(scope.session_id).unwrap().unwrap();
    assert_eq!(retained.turn_id, turn_id);
    assert_eq!(retained.messages, checkpoint);
    assert_eq!(
        host.store.action(action).unwrap().unwrap().state,
        "outcome_unknown"
    );
    assert_eq!(
        std::fs::read_to_string(scope.workspace.join("partial.txt")).unwrap(),
        "retained partial effect"
    );
}
