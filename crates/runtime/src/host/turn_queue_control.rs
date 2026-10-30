//! Per-session ordered queue operations. A queued intent does not touch the
//! current worker; only durable admission may move it into the transcript.
use super::*;
use crate::turn_queue::{QueueReceipt, QueuedTurn};

pub(super) struct PreparedStop {
    session: Uuid,
    fence: Option<(Arc<std::sync::atomic::AtomicU64>, u64)>,
    pending: Option<Uuid>,
}
#[derive(Default)]
pub(super) struct TurnQueueControl {
    gates: Mutex<HashMap<Uuid, std::sync::Weak<Mutex<()>>>>,
    fences: parking_lot::Mutex<
        HashMap<
            Uuid,
            (
                std::sync::Weak<AtomicBool>,
                std::sync::Weak<std::sync::atomic::AtomicU64>,
            ),
        >,
    >,
}
impl TurnQueueControl {
    pub(super) fn register(
        &self,
        session: Uuid,
        stopped: &Arc<AtomicBool>,
        epoch: &Arc<std::sync::atomic::AtomicU64>,
    ) {
        let mut fences = self.fences.lock();
        fences.retain(|_, (stop, _)| stop.strong_count() > 0);
        fences.insert(session, (Arc::downgrade(stopped), Arc::downgrade(epoch)));
    }
    pub(super) fn stop(&self, target: Option<Uuid>) {
        let mut fences = self.fences.lock();
        fences.retain(|_, (stop, _)| stop.strong_count() > 0);
        for (session, (stop, epoch)) in fences.iter() {
            if target.is_none_or(|target| target == *session)
                && let (Some(stop), Some(epoch)) = (stop.upgrade(), epoch.upgrade())
            {
                stop.store(true, Ordering::Release);
                epoch.fetch_add(1, Ordering::AcqRel);
            }
        }
    }
    pub(super) fn fence(&self, session: Uuid) -> Option<(Arc<std::sync::atomic::AtomicU64>, u64)> {
        let fences = self.fences.lock();
        let (_, epoch) = fences.get(&session)?;
        let epoch = epoch.upgrade()?;
        let expected = epoch.load(Ordering::Acquire);
        Some((epoch, expected))
    }

    pub(super) async fn gate(&self, session: Uuid) -> Result<tokio::sync::OwnedMutexGuard<()>> {
        let gate = {
            let mut gates = self.gates.lock().await;
            gates.retain(|_, gate| gate.strong_count() > 0);
            if let Some(gate) = gates.get(&session).and_then(std::sync::Weak::upgrade) {
                gate
            } else {
                ensure!(gates.len() < 64, "Too many queue operations are pending");
                let gate = Arc::new(Mutex::new(()));
                gates.insert(session, Arc::downgrade(&gate));
                gate
            }
        };
        Ok(gate.lock_owned().await)
    }
}
impl HostClient {
    pub fn queue_draft(
        &self,
        request_id: Uuid,
        turn: TurnRequest,
        submission: crate::drafts::DraftSubmission,
    ) -> Result<()> {
        self.enqueue(HostCommand::QueueDraft(request_id, turn, submission))
    }
    pub fn list_turn_queue(&self, request_id: Uuid, session: Uuid) -> Result<()> {
        self.enqueue(HostCommand::ListTurnQueue(request_id, session))
    }
    pub fn remove_queued_turn(&self, request_id: Uuid, session: Uuid, item: Uuid) -> Result<()> {
        self.enqueue(HostCommand::RemoveQueuedTurn(request_id, session, item))
    }
    pub fn run_queued_turn(&self, request_id: Uuid, session: Uuid, item: Uuid) -> Result<()> {
        self.enqueue(HostCommand::RunQueuedTurn(request_id, session, item))
    }
}
pub(super) fn tighter(captured: PermissionMode, current: PermissionMode) -> PermissionMode {
    fn rank(mode: PermissionMode) -> u8 {
        match mode {
            PermissionMode::ReadOnly => 0,
            PermissionMode::AskForWrites => 1,
            PermissionMode::AcceptEdits => 2,
            PermissionMode::Auto => 3,
        }
    }
    if rank(current) < rank(captured) {
        current
    } else {
        captured
    }
}
impl HostState {
    pub(super) fn capture_stop(&self, command: HostCommand) -> HostCommand {
        let HostCommand::Worker(target, WorkerCommand::Cancel) = command else {
            return command;
        };
        let target = if let Some(session) = target {
            Some(session)
        } else {
            let Ok(workers) = self.workers.try_lock() else {
                return HostCommand::RefusedCancel;
            };
            let mut sessions = workers
                .iter()
                .filter(|(_, worker)| worker.active.load(Ordering::Acquire))
                .map(|(session, _)| *session)
                .collect::<Vec<_>>();
            sessions.extend(self.queued.lock().keys().copied());
            sessions.sort();
            sessions.dedup();
            if sessions.len() != 1 {
                return HostCommand::RefusedCancel;
            }
            Some(sessions[0])
        };
        self.turn_queue.stop(target);
        let mut sessions = target.into_iter().collect::<Vec<_>>();
        if target.is_none() {
            sessions.extend(self.turn_queue.fences.lock().keys().copied());
        }
        let queued = self.queued.lock();
        for (session, (_, token)) in queued.iter() {
            if target.is_none_or(|target| target == *session) {
                token.cancel();
                sessions.push(*session);
            }
        }
        sessions.sort();
        sessions.dedup();
        HostCommand::PreparedCancel(
            sessions
                .into_iter()
                .map(|session| PreparedStop {
                    session,
                    fence: self.turn_queue.fence(session),
                    pending: queued.get(&session).map(|(id, _)| *id),
                })
                .collect(),
        )
    }
    fn stop_is_current(&self, stop: &PreparedStop) -> bool {
        if let Some((epoch, expected)) = &stop.fence {
            let Some((current, _)) = self.turn_queue.fence(stop.session) else {
                return false;
            };
            if !Arc::ptr_eq(epoch, &current) || epoch.load(Ordering::Acquire) != *expected {
                return false;
            }
            self.queued
                .lock()
                .get(&stop.session)
                .is_none_or(|(id, _)| Some(*id) == stop.pending)
        } else {
            // A startup reservation without a worker is the only captured
            // owner; its disappearance cannot authorize stopping a later run.
            let pending = self.queued.lock();
            match pending.get(&stop.session) {
                Some((id, _)) => Some(*id) == stop.pending,
                None => stop.pending.is_some() && self.turn_queue.fence(stop.session).is_none(),
            }
        }
    }
    pub(super) async fn publish_prepared_startup_stops(
        &self,
        stops: &[PreparedStop],
    ) -> Result<()> {
        for stop in stops {
            if stop.pending.is_some() {
                self.events
                    .send(HostEvent::Engine(Envelope::new(
                        Uuid::new_v4(),
                        Some(stop.session),
                        0,
                        EngineEvent::Cancelled,
                    )))
                    .await?;
            }
        }
        Ok(())
    }
    pub(super) async fn handle_prepared_stops(
        self: &Arc<Self>,
        stops: Vec<PreparedStop>,
    ) -> Result<()> {
        for stop in stops {
            if !self.stop_is_current(&stop) {
                continue;
            }
            if stop.pending.is_none() {
                let workers = self.workers.lock().await;
                if !self.stop_is_current(&stop) {
                    continue;
                }
                if let Some(worker) = workers.get(&stop.session) {
                    let message =
                        Envelope::new(Uuid::new_v4(), Some(stop.session), 0, WorkerCommand::Cancel);
                    if worker.commands.try_send(message).is_err() {
                        worker.stop.cancel();
                    }
                }
            }
            let host = self.clone();
            tokio::spawn(async move {
                let Ok(_guard) = host.turn_queue.gate(stop.session).await else {
                    return;
                };
                if !host.stop_is_current(&stop) {
                    return;
                }
                let store = host.store.clone();
                let session = stop.session;
                let _ = tokio::task::spawn_blocking(move || {
                    store.pause_turn_queue(
                        session,
                        "The task stopped. Send a queued message when ready.",
                    )
                })
                .await;
                let _ = host.emit_turn_queue(Uuid::nil(), session, None).await;
            });
        }
        Ok(())
    }
    pub(super) async fn emit_turn_queue(
        &self,
        request_id: Uuid,
        session: Uuid,
        failure: Option<String>,
    ) -> Result<()> {
        let result = if let Some(error) = failure {
            Err(error)
        } else {
            let store = self.store.clone();
            tokio::task::spawn_blocking(move || store.turn_queue(session))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| error.to_string()))
        };
        self.events
            .send(HostEvent::TurnQueue {
                request_id,
                session_id: session,
                result,
            })
            .await?;
        Ok(())
    }
    pub(super) async fn handle_queue_draft(
        &self,
        request_id: Uuid,
        turn: TurnRequest,
        submission: crate::drafts::DraftSubmission,
    ) -> Result<()> {
        let session = turn.session_id;
        let guard = self.turn_queue.gate(session).await;
        let result: Result<QueueReceipt> = async {
            let _guard = guard
                .as_ref()
                .map_err(|_| anyhow::anyhow!("Too many queue operations are pending"))?;
            ensure!(
                turn.session_id == submission.key.session_id,
                "Queued message belongs to another chat"
            );
            turn.provider.validate()?;
            xylo_services::providers::normalize_provider(&turn.provider)?;
            let workspace = tokio::fs::canonicalize(&turn.workspace)
                .await
                .context("The selected project is unavailable")?;
            ensure!(workspace.is_dir(), "The selected project is unavailable");
            let running = self.queued.lock().contains_key(&session)
                || self
                    .workers
                    .lock()
                    .await
                    .get(&session)
                    .is_some_and(|worker| {
                        worker.active.load(Ordering::Acquire)
                            && !worker.queue_stopped.load(Ordering::Acquire)
                    });
            let store = self.store.clone();
            let artifacts = self.paths.artifacts.clone();
            tokio::task::spawn_blocking(move || {
                xylo_services::attachments::AttachmentStore::new(artifacts)?
                    .validate(session, &turn.attachments)?;
                let mut receipt = store.queue_turn(&turn, &submission)?;
                if !running {
                    // The task can finish between the UI click and this enqueue.
                    // A durable enqueue still succeeds; dispatch requires Send.
                    let reason =
                        "The task ended before this message was queued. Send it when ready."
                            .to_owned();
                    if store
                        .set_queued_turn_paused(
                            session,
                            receipt.item.id,
                            true,
                            Some(reason.clone()),
                        )
                        .is_ok()
                    {
                        receipt.item.paused = true;
                        receipt.item.reason = Some(reason);
                    }
                }
                Ok(receipt)
            })
            .await?
        }
        .await;
        self.events
            .send(HostEvent::QueueAdded {
                request_id,
                result: result.map_err(|error| error.to_string()),
            })
            .await?;
        if guard.is_ok() {
            self.emit_turn_queue(Uuid::nil(), session, None).await?;
        }
        Ok(())
    }
    pub(super) async fn handle_list_turn_queue(
        &self,
        request_id: Uuid,
        session: Uuid,
    ) -> Result<()> {
        match self.turn_queue.gate(session).await {
            Ok(_guard) => self.emit_turn_queue(request_id, session, None).await,
            Err(error) => {
                self.emit_turn_queue(request_id, session, Some(error.to_string()))
                    .await
            }
        }
    }
    pub(super) async fn handle_remove_queued_turn(
        &self,
        request_id: Uuid,
        session: Uuid,
        item: Uuid,
    ) -> Result<()> {
        let guard = self.turn_queue.gate(session).await;
        let result = async {
            let _guard = guard
                .as_ref()
                .map_err(|_| anyhow::anyhow!("Too many queue operations are pending"))?;
            let store = self.store.clone();
            tokio::task::spawn_blocking(move || store.remove_queued_turn(session, item)).await?
        }
        .await;
        self.emit_turn_queue(
            request_id,
            session,
            result.err().map(|error: anyhow::Error| error.to_string()),
        )
        .await
    }
    pub(super) async fn handle_run_queued_turn(
        self: &Arc<Self>,
        request_id: Uuid,
        session: Uuid,
        item: Uuid,
    ) -> Result<()> {
        let guard = self.turn_queue.gate(session).await;
        let result = async {
            let _guard = guard
                .as_ref()
                .map_err(|_| anyhow::anyhow!("Too many queue operations are pending"))?;
            self.run_queue_locked(session, item, false).await
        }
        .await;
        {
            let mut queued = self.queued.lock();
            if queued.get(&session).is_some_and(|(id, _)| *id == item) {
                queued.remove(&session);
            }
        }
        self.emit_turn_queue(
            request_id,
            session,
            result.err().map(|error: anyhow::Error| error.to_string()),
        )
        .await
    }
    async fn run_queue_locked(
        self: &Arc<Self>,
        session: Uuid,
        item: Uuid,
        automatic: bool,
    ) -> Result<()> {
        let store = self.store.clone();
        let selected = tokio::task::spawn_blocking(move || store.queued_turn(session, item))
            .await??
            .context("This queued message is no longer available")?;
        if automatic && selected.paused {
            return Ok(());
        }
        ensure!(
            !self.stopping.load(Ordering::Acquire),
            "Application is closing"
        );
        ensure!(
            self.queued
                .lock()
                .get(&session)
                .is_none_or(|(id, _)| *id == item),
            "This chat already has a task starting"
        );
        {
            let workers = self.workers.lock().await;
            ensure!(
                !workers
                    .get(&session)
                    .is_some_and(|worker| worker.active.load(Ordering::Acquire)),
                "This chat already has a running task"
            );
        }
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            store.set_queued_turn_paused(
                session,
                item,
                true,
                Some("Starting queued message…".into()),
            )
        })
        .await??;
        let mut turn = selected.turn.clone();
        turn.permissions = tighter(turn.permissions, self.settings.lock().await.permissions);
        let admitted = Arc::new(AtomicBool::new(false));
        let result = self
            .worker_command_with_queue_receipt(
                Some(session),
                WorkerCommand::Run { turn },
                None,
                None,
                Some((selected, admitted.clone(), automatic)),
            )
            .await
            .and_then(|_| {
                ensure!(
                    admitted.load(Ordering::Acquire),
                    "Task stopped before admission"
                );
                Ok(())
            });
        if let Err(error) = result {
            let store = self.store.clone();
            let _ = tokio::task::spawn_blocking(move || {
                store.set_queued_turn_paused(
                    session,
                    item,
                    true,
                    Some(
                        "Could not start this message. Check Providers and try sending it again."
                            .into(),
                    ),
                )
            })
            .await;
            return Err(error);
        }
        Ok(())
    }
    pub(super) async fn publish_queued_admission(
        &self,
        item: &Option<(QueuedTurn, Arc<AtomicBool>, bool)>,
    ) {
        if let Some((item, _, _)) = item {
            let _ = self
                .events
                .send(HostEvent::QueuedTurnStarted {
                    session_id: item.turn.session_id,
                    item: item.clone(),
                })
                .await;
        }
    }
    pub(super) fn settle_turn_queue(
        self: &Arc<Self>,
        session: Uuid,
        successful: bool,
        epoch: Arc<std::sync::atomic::AtomicU64>,
        expected: u64,
        stopped: Arc<AtomicBool>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>> {
        let host = self.clone();
        Box::pin(async move {
            let Ok(_guard) = host.turn_queue.gate(session).await else {
                return;
            };
            if epoch.load(Ordering::Acquire) != expected {
                return;
            }
            if !successful || stopped.load(Ordering::Acquire) {
                let store = host.store.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    store.pause_turn_queue(
                        session,
                        "The task stopped. Send a queued message when ready.",
                    )
                })
                .await;
            } else {
                let store = host.store.clone();
                let rows = tokio::task::spawn_blocking(move || store.turn_queue(session)).await;
                if let Ok(Ok(rows)) = rows
                    && let Some(item) = rows.first()
                    && !item.paused
                {
                    if host.run_queue_locked(session, item.id, true).await.is_err() {
                        tracing::debug!(%session,"Queued message could not start; message retained for retry");
                    }
                }
            }
            let _ = host.emit_turn_queue(Uuid::nil(), session, None).await;
        })
    }
}

#[cfg(test)]
mod tests {
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
    async fn successful_completion_admits_fifo_with_latest_policy_before_worker_dispatch() {
        let (_dir, host, events) = super::super::tests::state();
        let (mut turn, sub) = queued(&host);
        turn.permissions = PermissionMode::Auto;
        let session = turn.session_id;
        let id = turn.turn_id;
        host.store.queue_turn(&turn, &sub).unwrap();
        let (mut incoming, _exit) = super::super::draft_tests::mock_worker(&host, session, 2).await;
        host.settings.lock().await.permissions = PermissionMode::ReadOnly;
        let (epoch, stopped) = {
            let workers = host.workers.lock().await;
            let worker = &workers[&session];
            (worker.queue_epoch.clone(), worker.queue_stopped.clone())
        };
        host.settle_turn_queue(session, true, epoch, 0, stopped)
            .await;
        let HostEvent::QueuedTurnStarted { item, .. } = events.recv().await.unwrap() else {
            panic!("admission receipt must precede dispatch")
        };
        assert_eq!(item.id, id);
        let WorkerCommand::RunAuthorized { turn, .. } = incoming.recv().await.unwrap().payload
        else {
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
        let (mut incoming, _exit) = super::super::draft_tests::mock_worker(&host, session, 2).await;
        let request = Uuid::new_v4();
        let command = HostCommand::RunQueuedTurn(request, session, id);
        assert!(host.prepare_command(&command).await.unwrap());
        assert!(
            host.prepare_command(&HostCommand::Worker(Some(session), WorkerCommand::Cancel))
                .await
                .unwrap()
        );
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
        let (mut incoming, _exit) = super::super::draft_tests::mock_worker(&host, session, 3).await;
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
    async fn generic_stop_refuses_multiple_owners_without_marking_either_worker() {
        let (_dir, host, _events) = super::super::tests::state();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let (_a, _exit_a) = super::super::draft_tests::mock_worker(&host, a, 2).await;
        let (_b, _exit_b) = super::super::draft_tests::mock_worker(&host, b, 2).await;
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
}
