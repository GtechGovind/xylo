//! Checkpoint application of identified user guidance, without upstream's
//! single-slot interrupt redirect or tool-output trust markers.
use hermes_core::{AgentError, Message};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use xylo_core::EngineEvent;
use xylo_services::Store;
#[derive(Clone)]
pub(crate) struct SteeringControl {
    store: Store,
    pending: Arc<AtomicBool>,
}
impl SteeringControl {
    pub(crate) fn new(store: Store) -> Self {
        Self {
            store,
            pending: Arc::new(AtomicBool::new(true)),
        }
    }
    pub(crate) fn notify(&self) {
        self.pending.store(true, Ordering::Release);
    }
    pub(crate) fn apply(
        &self,
        session: Uuid,
        turn: Uuid,
        messages: &mut Vec<Message>,
        events: &mpsc::Sender<EngineEvent>,
        cancel: &CancellationToken,
    ) -> Result<usize, AgentError> {
        if cancel.is_cancelled() || !self.pending.swap(false, Ordering::AcqRel) {
            return Ok(0);
        }
        let checkpoint =
            serde_json::to_value(&messages).map_err(|error| AgentError::Io(error.to_string()))?;
        let result =
            tokio::task::block_in_place(|| self.store.apply_steering(session, turn, &checkpoint))
                .map_err(|error| AgentError::Io(error.to_string()))?;
        let count = result.applied.len();
        if count > 0 {
            *messages = serde_json::from_value(result.messages)
                .map_err(|error| AgentError::Io(error.to_string()))?;
        }
        if result.has_more {
            self.notify();
        }
        for applied in result.applied {
            // Durable commit precedes acknowledgement. Backpressure is bounded by
            // the worker event channel; never turn a full queue into false success.
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(events.send(EngineEvent::Steering {
                    receipt: applied.receipt,
                }))
            })
            .map_err(|_| AgentError::Io("Guidance acknowledgement channel closed".into()))?;
        }
        Ok(count)
    }
}
