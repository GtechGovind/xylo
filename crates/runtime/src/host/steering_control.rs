//! Durable mid-turn guidance. Reserving transport precedes the journal write;
//! only the engine's durable application receipt may clear a captured draft.
use super::*;

impl HostClient {
    pub fn steer_draft(
        &self,
        request_id: Uuid,
        submission: crate::drafts::DraftSubmission,
    ) -> Result<()> {
        self.enqueue(HostCommand::SteerDraft(request_id, submission))
    }
}
impl HostState {
    pub(super) async fn steer_draft(
        &self,
        id: Uuid,
        submission: crate::drafts::DraftSubmission,
    ) -> Result<()> {
        // A retry after task completion still receives its original receipt.
        let mut emitted = false;
        let result = async {
            if let Some(previous) = crate::steering::lookup(self.store.clone(), id).await? {
                ensure!(
                    previous.request.session_id == submission.key.session_id
                        && previous.request.text == submission.text.trim()
                        && previous.draft.key == submission.key,
                    "Guidance identity belongs to different input"
                );
                if previous.receipt.state != SteeringState::Queued {
                    return Ok(previous);
                }
                let active = {
                    let workers = self.workers.lock().await;
                    workers
                        .get(&previous.request.session_id)
                        .is_some_and(|worker| {
                            worker.active.load(Ordering::Acquire)
                                && *worker.run_turn.lock() == Some(previous.request.turn_id)
                                && !worker.stop.is_cancelled()
                        })
                };
                if !active {
                    crate::steering::finish(
                        self.store.clone(),
                        previous.request.session_id,
                        Some(previous.request.turn_id),
                        "The task ended before this guidance could be applied".into(),
                    )
                    .await?;
                    return crate::steering::lookup(self.store.clone(), id)
                        .await?
                        .context("Guidance recovery was not journaled");
                }
            }
            let session = submission.key.session_id;
            let (commands, stop, turn) = {
                let workers = self.workers.lock().await;
                let worker = workers
                    .get(&session)
                    .context("This task has ended; send a new message to continue")?;
                ensure!(
                    worker.active.load(Ordering::Acquire),
                    "This task has ended; send a new message to continue"
                );
                let turn =
                    (*worker.run_turn.lock()).context("This task is not ready for guidance")?;
                (worker.commands.clone(), worker.stop.clone(), turn)
            };
            let permit = tokio::time::timeout(Duration::from_millis(100), commands.reserve_owned())
                .await
                .context("This task is busy; your guidance remains in the composer")?
                .context("This task stopped before guidance could be sent")?;
            ensure!(
                !stop.is_cancelled(),
                "This task stopped before guidance could be sent"
            );
            let request = SteeringRequest {
                id,
                session_id: session,
                turn_id: turn,
                text: submission.text.trim().to_owned(),
            };
            let outcome = crate::steering::queue(self.store.clone(), request, submission).await?;
            // Establish the canonical identity before a fast worker can report
            // Applied. Queued intentionally leaves the composer's text intact.
            self.events
                .send(HostEvent::SteeringResult {
                    request_id: id,
                    result: Ok(outcome.clone()),
                })
                .await?;
            emitted = true;
            if outcome.receipt.state == SteeringState::Queued {
                let still_active = {
                    let workers = self.workers.lock().await;
                    workers.get(&session).is_some_and(|worker| {
                        worker.active.load(Ordering::Acquire)
                            && *worker.run_turn.lock() == Some(turn)
                            && !worker.stop.is_cancelled()
                    })
                };
                if still_active {
                    permit.send(Envelope::new(
                        Uuid::new_v4(),
                        Some(session),
                        0,
                        WorkerCommand::SteerDurable {
                            guidance: outcome.request.clone(),
                        },
                    ));
                } else {
                    let receipts = crate::steering::finish(
                        self.store.clone(),
                        session,
                        Some(turn),
                        "The task ended before this guidance could be applied".into(),
                    )
                    .await?;
                    for receipt in receipts {
                        self.events
                            .send(HostEvent::SteeringResult {
                                request_id: receipt.request.id,
                                result: Ok(receipt),
                            })
                            .await?;
                    }
                }
            }
            Ok(outcome)
        }
        .await;
        // Queue success was already emitted before dispatch. Existing receipts
        // and failures take this path so every submitted UI request settles.
        match result {
            Ok(outcome) => {
                if !emitted {
                    self.events
                        .send(HostEvent::SteeringResult {
                            request_id: id,
                            result: Ok(outcome),
                        })
                        .await?;
                }
            }
            Err(error) => {
                self.events
                    .send(HostEvent::SteeringResult {
                        request_id: id,
                        result: Err(error.to_string()),
                    })
                    .await?;
            }
        }
        Ok(())
    }
}

/// Called only for a trusted worker pipe or after that process has been reaped.
/// The worker never supplies draft content; the host rereads its own journal.
pub(super) async fn emit_receipt(
    store: Store,
    events: &Sender<HostEvent>,
    session: Uuid,
    receipt: SteeringReceipt,
) -> Result<()> {
    ensure!(
        receipt.session_id == session,
        "Guidance receipt belongs to another task"
    );
    let outcome = crate::steering::lookup(store, receipt.id)
        .await?
        .context("Guidance receipt was not journaled")?;
    ensure!(
        outcome.receipt == receipt,
        "Guidance receipt differs from the journal"
    );
    events
        .send(HostEvent::SteeringResult {
            request_id: receipt.id,
            result: Ok(outcome),
        })
        .await?;
    Ok(())
}

pub(super) async fn finish_turn(
    store: Store,
    events: &Sender<HostEvent>,
    session: Uuid,
    turn: Option<Uuid>,
) -> Result<()> {
    crate::steering::finish(
        store.clone(),
        session,
        turn,
        "The task ended before this guidance could be applied".into(),
    )
    .await?;
    let outcomes =
        tokio::task::spawn_blocking(move || store.steering_for_turn(session, turn, 128)).await??;
    for outcome in outcomes {
        events
            .send(HostEvent::SteeringResult {
                request_id: outcome.request.id,
                result: Ok(outcome),
            })
            .await?;
    }
    Ok(())
}
