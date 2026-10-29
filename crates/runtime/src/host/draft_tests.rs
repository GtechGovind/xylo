//! Real durable draft admission with bounded in-memory worker transports. These
//! tests exercise the host, never a provider account or an inference process.
use super::*;
use crate::drafts::{DraftKey, DraftSave, DraftSubmission, DraftWrite};

#[tokio::test]
async fn guidance_is_queued_before_dispatch_and_only_applied_receipt_clears_the_draft() {
    let (_directory, host, events) = super::tests::state();
    let (turn, draft) = input(&host);
    host.store.register_user_turn(&turn).unwrap();
    host.store
        .save_checkpoint(turn.session_id, turn.turn_id, &serde_json::json!([]))
        .unwrap();
    let (mut incoming, _exit) = mock_worker(&host, turn.session_id, 4).await;
    {
        let workers = host.workers.lock().await;
        let worker = &workers[&turn.session_id];
        worker.active.store(true, Ordering::Release);
        *worker.run_turn.lock() = Some(turn.turn_id);
    }
    let id = Uuid::new_v4();
    host.steer_draft(id, draft.clone()).await.unwrap();
    let HostEvent::SteeringResult {
        request_id,
        result: Ok(queued),
    } = events.recv().await.unwrap()
    else {
        panic!("Missing correlated guidance receipt")
    };
    assert_eq!(request_id, id);
    assert_eq!(queued.receipt.state, SteeringState::Queued);
    assert_eq!(host.store.draft(&draft.key).unwrap().text, draft.text);
    let WorkerCommand::SteerDurable { guidance } = incoming.recv().await.unwrap().payload else {
        panic!("Missing durable guidance command")
    };
    assert_eq!(guidance, queued.request);
    let applied = host
        .store
        .apply_steering(turn.session_id, turn.turn_id, &serde_json::json!([]))
        .unwrap();
    assert_eq!(applied.applied.len(), 1);
    steering_control::emit_receipt(
        host.store.clone(),
        &host.events,
        turn.session_id,
        applied.applied[0].receipt.clone(),
    )
    .await
    .unwrap();
    let HostEvent::SteeringResult {
        result: Ok(applied),
        ..
    } = events.recv().await.unwrap()
    else {
        panic!("Missing application receipt")
    };
    assert_eq!(applied.receipt.state, SteeringState::Applied);
    assert!(host.store.draft(&draft.key).unwrap().is_empty());
    assert_eq!(
        host.store
            .checkpoint(turn.session_id)
            .unwrap()
            .unwrap()
            .messages[0]["content"],
        draft.text.trim()
    );
    // A lost UI acknowledgement is recoverable after the task has ended,
    // without another transport write or a duplicate user message.
    host.workers.lock().await.remove(&turn.session_id);
    host.steer_draft(id, draft).await.unwrap();
    assert!(
        matches!(events.recv().await.unwrap(), HostEvent::SteeringResult { result: Ok(outcome), .. } if outcome.receipt.state == SteeringState::Applied)
    );
    assert_eq!(host.store.messages(turn.session_id, 10).unwrap().len(), 2);
}

#[tokio::test]
async fn guidance_transport_failure_keeps_the_saved_input_without_claiming_delivery() {
    let (_directory, host, events) = super::tests::state();
    let (turn, draft) = input(&host);
    host.store.register_user_turn(&turn).unwrap();
    let (incoming, _exit) = mock_worker(&host, turn.session_id, 1).await;
    {
        let workers = host.workers.lock().await;
        let worker = &workers[&turn.session_id];
        worker.active.store(true, Ordering::Release);
        *worker.run_turn.lock() = Some(turn.turn_id);
    }
    drop(incoming);
    let id = Uuid::new_v4();
    host.steer_draft(id, draft.clone()).await.unwrap();
    assert!(
        matches!(events.recv().await.unwrap(), HostEvent::SteeringResult { request_id, result: Err(_) } if request_id == id)
    );
    assert!(host.store.steering(id).unwrap().is_none());
    assert_eq!(host.store.draft(&draft.key).unwrap().text, draft.text);
}

#[tokio::test]
async fn reaped_task_recovers_applied_guidance_even_when_its_pipe_receipt_was_lost() {
    let (_directory, host, events) = super::tests::state();
    let (turn, draft) = input(&host);
    host.store.register_user_turn(&turn).unwrap();
    host.store
        .save_checkpoint(turn.session_id, turn.turn_id, &serde_json::json!([]))
        .unwrap();
    let guidance = SteeringRequest {
        id: Uuid::new_v4(),
        session_id: turn.session_id,
        turn_id: turn.turn_id,
        text: draft.text.trim().into(),
    };
    host.store.queue_steering(&guidance, &draft).unwrap();
    host.store
        .apply_steering(turn.session_id, turn.turn_id, &serde_json::json!([]))
        .unwrap();
    steering_control::finish_turn(
        host.store.clone(),
        &host.events,
        turn.session_id,
        Some(turn.turn_id),
    )
    .await
    .unwrap();
    assert!(
        matches!(events.recv().await.unwrap(), HostEvent::SteeringResult { result: Ok(outcome), .. } if outcome.request.id == guidance.id && outcome.receipt.state == SteeringState::Applied)
    );
    assert!(host.store.draft(&draft.key).unwrap().is_empty());
}
fn input(host: &HostState) -> (TurnRequest, DraftSubmission) {
    let session = Uuid::new_v4();
    let key = DraftKey {
        session_id: session,
        workspace: Some(host.paths.data.clone()),
    };
    let saved = host
        .store
        .save_draft(&DraftWrite {
            key: key.clone(),
            expected_revision: 0,
            mutation_id: Uuid::new_v4(),
            text: "Keep the unsent draft αβ 🦀".into(),
            attachments: Vec::new(),
        })
        .unwrap();
    let DraftSave::Saved(saved) = saved else {
        panic!("Unexpected conflict")
    };
    let turn = TurnRequest {
        session_id: session,
        turn_id: Uuid::new_v4(),
        prompt: saved.text.clone(),
        workspace: host.paths.data.clone(),
        provider: ProviderConfig {
            model: "loopback-fixture".into(),
            ..ProviderConfig::default()
        },
        permissions: PermissionMode::ReadOnly,
        attachments: Vec::new(),
    };
    let draft = DraftSubmission {
        key,
        expected_revision: saved.revision,
        text: saved.text,
        attachments: Vec::new(),
    };
    (turn, draft)
}
async fn mock_worker(
    host: &HostState,
    session: Uuid,
    capacity: usize,
) -> (
    mpsc::Receiver<Envelope<WorkerCommand>>,
    tokio::sync::watch::Sender<bool>,
) {
    let (commands, incoming) = mpsc::channel(capacity);
    let (exited_tx, exited) = tokio::sync::watch::channel(false);
    host.workers.lock().await.insert(
        session,
        WorkerHandle {
            commands,
            active: Arc::new(AtomicBool::new(false)),
            lease: Arc::new(Mutex::new(None)),
            stop: CancellationToken::new(),
            exited,
            run_request: Arc::new(parking_lot::Mutex::new(None)),
            run_turn: Arc::new(parking_lot::Mutex::new(None)),
            model_activity: Arc::new(Mutex::new(None)),
        },
    );
    (incoming, exited_tx)
}
#[tokio::test]
async fn invalid_provider_and_cancel_before_admission_keep_the_exact_saved_draft() {
    let (_directory, host, events) = super::tests::state();
    let (mut turn, draft) = input(&host);
    turn.provider.model.clear();
    let id = Uuid::new_v4();
    let command = HostCommand::DraftSubmit(id, turn.clone(), draft.clone());
    assert!(host.prepare_command(&command).await.unwrap());
    host.handle(command).await.unwrap();
    assert!(
        matches!(events.recv().await.unwrap(),HostEvent::DraftAdmitted{request_id,result:Err(_)} if request_id==id)
    );
    assert_eq!(host.store.draft(&draft.key).unwrap().text, draft.text);
    assert!(host.store.messages(turn.session_id, 10).unwrap().is_empty());
    turn.provider.model = "loopback-fixture".into();
    let id = Uuid::new_v4();
    let command = HostCommand::DraftSubmit(id, turn.clone(), draft.clone());
    assert!(host.prepare_command(&command).await.unwrap());
    host.worker_command(Some(turn.session_id), WorkerCommand::Cancel)
        .await
        .unwrap();
    host.handle(command).await.unwrap();
    let mut correlated = false;
    while let Ok(event) = events.try_recv() {
        if matches!(event,HostEvent::DraftAdmitted{request_id,result:Err(_)} if request_id==id) {
            correlated = true;
        }
    }
    assert!(correlated);
    assert_eq!(host.store.draft(&draft.key).unwrap().text, draft.text);
    assert!(host.store.messages(turn.session_id, 10).unwrap().is_empty());
    assert!(host.queued.lock().is_empty());
}
#[tokio::test]
async fn full_worker_queue_fails_before_durable_user_registration_or_clear() {
    let (_directory, host, _events) = super::tests::state();
    let (turn, draft) = input(&host);
    let (_incoming, _exit) = mock_worker(&host, turn.session_id, 1).await;
    host.workers.lock().await[&turn.session_id]
        .commands
        .try_send(Envelope::new(
            Uuid::new_v4(),
            Some(turn.session_id),
            0,
            WorkerCommand::Shutdown,
        ))
        .unwrap();
    assert!(
        host.worker_command_with_draft(
            Some(turn.session_id),
            WorkerCommand::Run { turn: turn.clone() },
            Some(draft.clone())
        )
        .await
        .is_err()
    );
    assert_eq!(host.store.draft(&draft.key).unwrap().text, draft.text);
    assert!(host.store.messages(turn.session_id, 10).unwrap().is_empty());
}
#[tokio::test]
async fn successful_dispatch_returns_admission_even_when_postcommit_metadata_receiver_is_closed() {
    let (_directory, host, events) = super::tests::state();
    let (turn, draft) = input(&host);
    let (mut incoming, _exit) = mock_worker(&host, turn.session_id, 4).await;
    events.close();
    let receipt = host
        .worker_command_with_draft(
            Some(turn.session_id),
            WorkerCommand::Run { turn: turn.clone() },
            Some(draft.clone()),
        )
        .await
        .unwrap()
        .unwrap();
    assert!(receipt.cleared);
    assert!(host.store.draft(&draft.key).unwrap().is_empty());
    assert_eq!(host.store.messages(turn.session_id, 10).unwrap().len(), 1);
    assert!(
        matches!(incoming.recv().await.unwrap().payload,WorkerCommand::RunAuthorized { turn: dispatched, credential: WorkerCredential(None) } if dispatched.turn_id == turn.turn_id && dispatched.session_id == turn.session_id && dispatched.workspace == turn.workspace)
    );
}
#[tokio::test]
async fn completed_turn_retry_returns_durable_receipt_without_dispatch_or_clearing_newer_input() {
    let (_directory, host, events) = super::tests::state();
    let (turn, draft) = input(&host);
    host.store.admit_draft_turn(&turn, &draft).unwrap();
    host.store
        .complete_turn(
            turn.session_id,
            turn.turn_id,
            &serde_json::json!([]),
            "Completed",
            &Usage::default(),
        )
        .unwrap();
    let empty = host.store.draft(&draft.key).unwrap();
    let saved = host
        .store
        .save_draft(&DraftWrite {
            key: draft.key.clone(),
            expected_revision: empty.revision,
            mutation_id: Uuid::new_v4(),
            text: "Newer composer text".into(),
            attachments: Vec::new(),
        })
        .unwrap();
    let DraftSave::Saved(newer) = saved else {
        panic!("Unexpected conflict")
    };
    let receipt = host
        .worker_command_with_draft(
            Some(turn.session_id),
            WorkerCommand::Run { turn: turn.clone() },
            Some(draft.clone()),
        )
        .await
        .unwrap()
        .unwrap();
    assert!(!receipt.cleared);
    assert_eq!(receipt.draft, newer);
    assert!(host.workers.lock().await.is_empty());
    assert!(matches!(
        events.recv().await.unwrap(),
        HostEvent::Engine(Envelope {
            payload: EngineEvent::Completed { .. },
            ..
        })
    ));
    assert_eq!(host.store.messages(turn.session_id, 10).unwrap().len(), 2);
}
#[tokio::test]
async fn normal_shutdown_waits_for_an_inflight_operation_gate_before_teardown() {
    let (_directory, host, _events) = super::tests::state();
    let held = host.operations.read().await;
    let stopping = host.clone();
    let task = tokio::spawn(async move {
        stopping.shutdown().await;
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        !task.is_finished(),
        "Shutdown must drain existing draft handlers even without an update installation"
    );
    assert!(host.stopping.load(Ordering::Acquire));
    drop(held);
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn cancellation_during_commit_reaches_a_full_single_slot_worker_after_reserved_run() {
    let (_directory, host, _events) = super::tests::state();
    let (turn, draft) = input(&host);
    let (mut incoming, _exit) = mock_worker(&host, turn.session_id, 1).await;
    let sender = host.workers.lock().await[&turn.session_id].commands.clone();
    let connection = rusqlite::Connection::open(&host.paths.database).unwrap();
    connection.execute_batch("BEGIN IMMEDIATE").unwrap();
    let running = host.clone();
    let session = turn.session_id;
    let turn_id = turn.turn_id;
    let workspace = turn.workspace.clone();
    let task = tokio::spawn(async move {
        running
            .worker_command_with_draft(Some(session), WorkerCommand::Run { turn }, Some(draft))
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while sender.capacity() != 0 || !crate::drafts::admission_in_progress(turn_id) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    host.worker_command(Some(session), WorkerCommand::Cancel)
        .await
        .unwrap();
    connection.execute_batch("COMMIT").unwrap();
    let run = tokio::time::timeout(Duration::from_secs(3), incoming.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(run.payload, WorkerCommand::RunAuthorized { turn: dispatched, credential: WorkerCredential(None) } if dispatched.turn_id == turn_id && dispatched.session_id == session && dispatched.workspace == workspace)
    );
    let cancellation = tokio::time::timeout(Duration::from_secs(1), incoming.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(cancellation.payload, WorkerCommand::Cancel));
    assert!(task.await.unwrap().unwrap().unwrap().cleared);
}
