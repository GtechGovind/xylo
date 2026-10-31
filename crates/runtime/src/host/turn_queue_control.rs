//! Per-session ordered queue operations. A queued intent does not touch the
//! current worker; only durable admission may move it into the transcript.
use super::*;
use crate::turn_queue::{QueueReceipt, QueuedTurn};

pub(super) struct PreparedStop {
    session: Uuid,
    fence: Option<(Arc<std::sync::atomic::AtomicU64>, u64)>,
    pending: Option<Uuid>,
}
/// Weak views of one worker's queue state, readable without the workers lock.
struct Fence {
    stopped: std::sync::Weak<AtomicBool>,
    epoch: std::sync::Weak<std::sync::atomic::AtomicU64>,
    active: std::sync::Weak<AtomicBool>,
}

#[derive(Default)]
pub(super) struct TurnQueueControl {
    gates: Mutex<HashMap<Uuid, std::sync::Weak<Mutex<()>>>>,
    fences: parking_lot::Mutex<HashMap<Uuid, Fence>>,
}
impl TurnQueueControl {
    pub(super) fn register(&self, session: Uuid, worker: &super::worker_control::WorkerHandle) {
        let mut fences = self.fences.lock();
        fences.retain(|_, fence| fence.stopped.strong_count() > 0);
        fences.insert(
            session,
            Fence {
                stopped: Arc::downgrade(&worker.queue_stopped),
                epoch: Arc::downgrade(&worker.queue_epoch),
                active: Arc::downgrade(&worker.active),
            },
        );
    }
    pub(super) fn stop(&self, target: Option<Uuid>) {
        let mut fences = self.fences.lock();
        fences.retain(|_, fence| fence.stopped.strong_count() > 0);
        for (session, fence) in fences.iter() {
            if target.is_none_or(|target| target == *session)
                && let (Some(stop), Some(epoch)) = (fence.stopped.upgrade(), fence.epoch.upgrade())
            {
                stop.store(true, Ordering::Release);
                epoch.fetch_add(1, Ordering::AcqRel);
            }
        }
    }
    pub(super) fn fence(&self, session: Uuid) -> Option<(Arc<std::sync::atomic::AtomicU64>, u64)> {
        let fences = self.fences.lock();
        let epoch = fences.get(&session)?.epoch.upgrade()?;
        let expected = epoch.load(Ordering::Acquire);
        Some((epoch, expected))
    }
    /// Sessions with a running turn. Never waits for the workers lock, so a
    /// Stop is captured even while admission holds it.
    pub(super) fn active_sessions(&self) -> Vec<Uuid> {
        self.fences
            .lock()
            .iter()
            .filter(|(_, fence)| {
                fence
                    .active
                    .upgrade()
                    .is_some_and(|active| active.load(Ordering::Acquire))
            })
            .map(|(session, _)| *session)
            .collect()
    }
    fn sessions(&self) -> Vec<Uuid> {
        self.fences.lock().keys().copied().collect()
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
impl HostState {
    /// The one Stop implementation, captured synchronously at intake so no
    /// later admission or dispatch can overtake it. A Stop without a target
    /// applies only when exactly one chat is running or starting.
    pub(super) fn capture_stop(&self, command: HostCommand) -> HostCommand {
        let HostCommand::Worker(target, WorkerCommand::Cancel) = command else {
            return command;
        };
        let target = if let Some(session) = target {
            Some(session)
        } else {
            let mut sessions = self.turn_queue.active_sessions();
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
            sessions.extend(self.turn_queue.sessions());
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
    /// Cancels the running turn of each captured session and pauses its
    /// queue. A pending admission was already cancelled at capture; it reports
    /// its own `Cancelled` unless it had dispatched, so the UI sees exactly one.
    pub(super) async fn handle_prepared_stops(
        self: &Arc<Self>,
        stops: Vec<PreparedStop>,
    ) -> Result<()> {
        for stop in stops {
            if !self.stop_is_current(&stop) {
                continue;
            }
            self.stop_active_run(stop.session).await;
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
                    store.pause_turn_queue(session, STOPPED_REASON)
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
        // Admission applies the current permission mode and standing approvals.
        let turn = selected.turn.clone();
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
    /// After a run ends, starts the next queued message or pauses the queue
    /// with the reason shown on each item. Holds the `operations` read gate so
    /// no worker starts while an update installs.
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
            let _operation = host.operations.read().await;
            let Ok(_guard) = host.turn_queue.gate(session).await else {
                return;
            };
            if epoch.load(Ordering::Acquire) != expected {
                return;
            }
            let pause = if stopped.load(Ordering::Acquire) {
                Some(STOPPED_REASON)
            } else if !successful {
                Some(FAILED_REASON)
            } else if host.installing.load(Ordering::Acquire) {
                Some(INSTALLING_REASON)
            } else {
                None
            };
            if let Some(reason) = pause {
                let store = host.store.clone();
                let _ =
                    tokio::task::spawn_blocking(move || store.pause_turn_queue(session, reason))
                        .await;
            } else {
                let store = host.store.clone();
                let rows = tokio::task::spawn_blocking(move || store.turn_queue(session)).await;
                if let Ok(Ok(rows)) = rows
                    && let Some(item) = rows.first()
                    && !item.paused
                    && host.run_queue_locked(session, item.id, true).await.is_err()
                {
                    tracing::debug!(%session, "Queued message could not start; message retained for retry");
                }
            }
            let _ = host.emit_turn_queue(Uuid::nil(), session, None).await;
        })
    }
}

/// Queue pause reasons, shown on each paused item.
const STOPPED_REASON: &str = "You stopped the task. Resume the queue when ready.";
const FAILED_REASON: &str = "The task ended with an error. Resume the queue when ready.";
const INSTALLING_REASON: &str = "Paused while an update installs. Resume the queue when ready.";

#[cfg(test)]
mod tests;
