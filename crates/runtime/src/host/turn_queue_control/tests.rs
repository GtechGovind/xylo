use super::*;
fn queued(host: &HostState) -> (TurnRequest, crate::drafts::DraftSubmission) {
    let key = xylo_services::DraftKey {
        session_id: Uuid::new_v4(),
        workspace: Some(host.paths.data.clone()),
    };
    let current = host.store.draft(&key).unwrap();
    let xylo_services::DraftSave::Saved(saved) = host
        .store
        .save_draft(&xylo_services::DraftWrite {
            key: key.clone(),
            expected_revision: current.revision,
            mutation_id: Uuid::new_v4(),
            text: "next task".into(),
            attachments: vec![],
        })
        .unwrap()
    else {
        panic!("draft conflict")
    };
    let sub = crate::drafts::DraftSubmission {
        key,
        expected_revision: saved.revision,
        text: saved.text,
        attachments: saved.attachments,
    };
    let turn = TurnRequest {
        session_id: sub.key.session_id,
        turn_id: Uuid::new_v4(),
        prompt: sub.text.clone(),
        workspace: host.paths.data.clone(),
        provider: ProviderConfig {
            model: "fixture".into(),
            ..Default::default()
        },
        permissions: PermissionMode::ReadOnly,
        attachments: vec![],
        approved_rules: Vec::new(),
    };
    (turn, sub)
}
#[tokio::test]
async fn idle_enqueue_preserves_manual_send_and_never_registers_transcript() {
    let (_dir, host, events) = super::super::tests::state();
    let (turn, sub) = queued(&host);
    let session = turn.session_id;
    let id = turn.turn_id;
    host.handle_queue_draft(Uuid::new_v4(), turn, sub)
        .await
        .unwrap();
    let HostEvent::QueueAdded {
        result: Ok(receipt),
        ..
    } = events.recv().await.unwrap()
    else {
        panic!("missing queue receipt")
    };
    assert!(receipt.cleared);
    assert!(receipt.item.paused);
    assert!(host.store.messages(session, 10).unwrap().is_empty());
    assert!(host.workers.lock().await.is_empty());
    assert_eq!(host.store.turn_queue(session).unwrap()[0].id, id);
}
#[tokio::test]
async fn stopped_completion_pauses_tail_and_stale_settlement_cannot_touch_new_run() {
    let (_dir, host, _events) = super::super::tests::state();
    let (turn, sub) = queued(&host);
    let session = turn.session_id;
    let id = turn.turn_id;
    host.store.queue_turn(&turn, &sub).unwrap();
    let epoch = Arc::new(std::sync::atomic::AtomicU64::new(3));
    let stopped = Arc::new(AtomicBool::new(true));
    host.settle_turn_queue(session, true, epoch.clone(), 3, stopped.clone())
        .await;
    assert!(host.store.queued_turn(session, id).unwrap().unwrap().paused);
    host.store
        .set_queued_turn_paused(session, id, false, None)
        .unwrap();
    epoch.store(4, Ordering::Release);
    stopped.store(false, Ordering::Release);
    host.settle_turn_queue(session, false, epoch, 3, stopped)
        .await;
    assert!(
        !host.store.queued_turn(session, id).unwrap().unwrap().paused,
        "old failure changed the newer run's tail"
    );
    assert!(host.workers.lock().await.is_empty());
}
#[tokio::test]
async fn update_refusal_returns_correlated_queue_receipt_without_clearing_draft() {
    let (_dir, host, events) = super::super::tests::state();
    let (turn, sub) = queued(&host);
    let request = Uuid::new_v4();
    host.installing.store(true, Ordering::Release);
    assert!(
        !host
            .prepare_command(&HostCommand::QueueDraft(request, turn, sub.clone()))
            .await
            .unwrap()
    );
    assert!(
        matches!(events.recv().await.unwrap(),HostEvent::QueueAdded {request_id,result:Err(_)} if request_id==request)
    );
    assert_eq!(host.store.draft(&sub.key).unwrap().text, sub.text);
}
#[tokio::test]
async fn successful_completion_admits_fifo_with_current_policy_before_worker_dispatch() {
    let (_dir, host, events) = super::super::tests::state();
    let (mut turn, sub) = queued(&host);
    turn.permissions = PermissionMode::ApproveForMe;
    let session = turn.session_id;
    let id = turn.turn_id;
    host.store.queue_turn(&turn, &sub).unwrap();
    let (mut incoming, _exit) = crate::host::tests::draft::mock_worker(&host, session, 2).await;
    host.settings.lock().await.permissions = PermissionMode::ReadOnly;
    let (epoch, stopped) = {
        let workers = host.workers.lock().await;
        let worker = &workers[&session];
        (worker.queue_epoch.clone(), worker.queue_stopped.clone())
    };
    host.settle_turn_queue(session, true, epoch, 0, stopped)
        .await;
    let HostEvent::QueuedTurnStarted { item, .. } = super::super::tests::next_event(&events).await
    else {
        panic!("admission receipt must precede dispatch")
    };
    assert_eq!(item.id, id);
    let WorkerCommand::RunAuthorized { turn, .. } = incoming.recv().await.unwrap().payload else {
        panic!("queued turn was not dispatched")
    };
    assert_eq!(turn.turn_id, id);
    assert_eq!(turn.permissions, PermissionMode::ReadOnly);
    assert_eq!(host.store.messages(session, 10).unwrap().len(), 1);
    assert!(host.store.turn_queue(session).unwrap().is_empty());
    // A repeated old terminal receipt must not replay the admitted turn.
    assert!(incoming.try_recv().is_err());
}
#[tokio::test]
async fn stop_after_manual_send_preparation_cancels_before_session_gate_admission() {
    let (_dir, host, _events) = super::super::tests::state();
    let (turn, sub) = queued(&host);
    let session = turn.session_id;
    let id = turn.turn_id;
    host.store.queue_turn(&turn, &sub).unwrap();
    let (mut incoming, _exit) = crate::host::tests::draft::mock_worker(&host, session, 2).await;
    let request = Uuid::new_v4();
    let command = HostCommand::RunQueuedTurn(request, session, id);
    assert!(host.prepare_command(&command).await.unwrap());
    let stop = host.capture_stop(HostCommand::Worker(Some(session), WorkerCommand::Cancel));
    assert!(host.prepare_command(&stop).await.unwrap());
    // The older manual Send has not yet entered its async handler. Stop
    // must cancel its prepared reservation rather than be reset by it.
    host.handle(command).await.unwrap();
    assert!(incoming.try_recv().is_err());
    assert!(host.store.messages(session, 10).unwrap().is_empty());
    assert!(host.store.queued_turn(session, id).unwrap().unwrap().paused);
    assert!(host.queued.lock().is_empty());
}
#[tokio::test]
async fn older_prepared_stop_cannot_cancel_a_later_explicit_queued_send() {
    let (_dir, host, _events) = super::super::tests::state();
    let (turn, sub) = queued(&host);
    let session = turn.session_id;
    let id = turn.turn_id;
    host.store.queue_turn(&turn, &sub).unwrap();
    let (mut incoming, _exit) = crate::host::tests::draft::mock_worker(&host, session, 3).await;
    let stop = host.capture_stop(HostCommand::Worker(Some(session), WorkerCommand::Cancel));
    assert!(host.prepare_command(&stop).await.unwrap());
    let send = HostCommand::RunQueuedTurn(Uuid::new_v4(), session, id);
    assert!(host.prepare_command(&send).await.unwrap());
    host.handle(send).await.unwrap();
    assert!(matches!(
        incoming.recv().await.unwrap().payload,
        WorkerCommand::RunAuthorized { .. }
    ));
    host.handle(stop).await.unwrap();
    assert!(
        incoming.try_recv().is_err(),
        "older Stop cancelled the explicitly resumed run"
    );
    assert!(
        !host.workers.lock().await[&session]
            .queue_stopped
            .load(Ordering::Acquire)
    );
}
#[tokio::test]
async fn queued_turn_takes_a_looser_current_mode_at_admission() {
    let (_dir, host, _events) = super::super::tests::state();
    let (turn, sub) = queued(&host);
    assert_eq!(turn.permissions, PermissionMode::ReadOnly);
    let session = turn.session_id;
    host.store.queue_turn(&turn, &sub).unwrap();
    let (mut incoming, _exit) = crate::host::tests::draft::mock_worker(&host, session, 2).await;
    host.settings.lock().await.permissions = PermissionMode::FullAccess;
    let (epoch, stopped) = {
        let workers = host.workers.lock().await;
        let worker = &workers[&session];
        (worker.queue_epoch.clone(), worker.queue_stopped.clone())
    };
    host.settle_turn_queue(session, true, epoch, 0, stopped)
        .await;
    let WorkerCommand::RunAuthorized { turn, .. } = incoming.recv().await.unwrap().payload else {
        panic!("queued turn was not dispatched")
    };
    assert_eq!(turn.permissions, PermissionMode::FullAccess);
}
#[tokio::test]
async fn settlement_during_an_update_install_pauses_instead_of_starting_a_worker() {
    let (_dir, host, _events) = super::super::tests::state();
    let (turn, sub) = queued(&host);
    let session = turn.session_id;
    let id = turn.turn_id;
    host.store.queue_turn(&turn, &sub).unwrap();
    host.installing.store(true, Ordering::Release);
    host.settle_turn_queue(
        session,
        true,
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
        0,
        Arc::new(AtomicBool::new(false)),
    )
    .await;
    let item = host.store.queued_turn(session, id).unwrap().unwrap();
    assert!(item.paused);
    assert_eq!(item.reason.as_deref(), Some(INSTALLING_REASON));
    assert!(host.workers.lock().await.is_empty());
}
#[tokio::test]
async fn pause_reasons_distinguish_stop_from_failure() {
    let (_dir, host, _events) = super::super::tests::state();
    let (turn, sub) = queued(&host);
    let session = turn.session_id;
    let id = turn.turn_id;
    host.store.queue_turn(&turn, &sub).unwrap();
    let epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
    host.settle_turn_queue(
        session,
        false,
        epoch.clone(),
        0,
        Arc::new(AtomicBool::new(false)),
    )
    .await;
    let item = host.store.queued_turn(session, id).unwrap().unwrap();
    assert_eq!(item.reason.as_deref(), Some(FAILED_REASON));
    host.settle_turn_queue(session, true, epoch, 0, Arc::new(AtomicBool::new(true)))
        .await;
    let item = host.store.queued_turn(session, id).unwrap().unwrap();
    assert_eq!(item.reason.as_deref(), Some(STOPPED_REASON));
}
#[tokio::test]
async fn generic_stop_refuses_multiple_owners_without_marking_either_worker() {
    let (_dir, host, _events) = super::super::tests::state();
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let (_a, _exit_a) = crate::host::tests::draft::mock_worker(&host, a, 2).await;
    let (_b, _exit_b) = crate::host::tests::draft::mock_worker(&host, b, 2).await;
    {
        let workers = host.workers.lock().await;
        for worker in workers.values() {
            worker.active.store(true, Ordering::Release);
        }
    }
    assert!(matches!(
        host.capture_stop(HostCommand::Worker(None, WorkerCommand::Cancel)),
        HostCommand::RefusedCancel
    ));
    let workers = host.workers.lock().await;
    assert!(
        workers
            .values()
            .all(|worker| !worker.queue_stopped.load(Ordering::Acquire))
    );
}
