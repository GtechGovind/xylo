//! Worker-side verification and acknowledgement of persisted guidance identities.
use super::*;
use xylo_core::{SteeringReceipt, SteeringRequest, SteeringState};
pub(super) async fn receive(
    store: Store,
    active: Option<&ActiveTask>,
    guidance: SteeringRequest,
) -> Result<SteeringReceipt> {
    let id = guidance.id;
    let lookup = store.clone();
    let saved = tokio::task::spawn_blocking(move || lookup.steering(id)).await??;
    let Some(saved) = saved else {
        return Ok(rejected(&guidance, "Guidance was not durably queued"));
    };
    if saved.request != guidance {
        return Ok(rejected(
            &guidance,
            "Guidance identity does not match its saved request",
        ));
    }
    if saved.receipt.state != SteeringState::Queued {
        return Ok(saved.receipt);
    }
    if let Some(task) = active
        && task.session_id == guidance.session_id
        && task.turn_id == guidance.turn_id
        && task.busy.load(Ordering::Acquire)
        && !task.cancel.is_cancelled()
    {
        task.engine.notify_steering();
        return Ok(saved.receipt);
    }
    let session = guidance.session_id;
    let turn = guidance.turn_id;
    tokio::task::spawn_blocking(move || {
        store.finish_pending_steering(
            session,
            Some(turn),
            "Task ended before guidance was added",
        )?;
        // Application can commit between our first lookup and task completion.
        // Re-read the immutable journal outcome instead of fabricating Cancelled.
        store
            .steering(id)?
            .map(|outcome| outcome.receipt)
            .context("Guidance disappeared while settling its outcome")
    })
    .await?
}
fn rejected(guidance: &SteeringRequest, reason: &str) -> SteeringReceipt {
    SteeringReceipt {
        id: guidance.id,
        session_id: guidance.session_id,
        turn_id: guidance.turn_id,
        state: SteeringState::Cancelled,
        reason: Some(reason.into()),
    }
}
pub(super) async fn finish(
    store: Store,
    session: uuid::Uuid,
    turn: uuid::Uuid,
    output: &mpsc::Sender<Envelope<EngineEvent>>,
    sequence: &AtomicU64,
) -> Result<()> {
    let outcomes = tokio::task::spawn_blocking(move || {
        store.finish_pending_steering(session, Some(turn), "Task ended before guidance was added")
    })
    .await??;
    for outcome in outcomes {
        send(
            output,
            sequence,
            outcome.receipt.id,
            Some(session),
            EngineEvent::Steering {
                receipt: outcome.receipt,
            },
        )
        .await?;
    }
    Ok(())
}
