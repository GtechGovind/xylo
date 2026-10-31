//! Durable user guidance applied at role-safe loop boundaries.
//!
//! The store inserts queued guidance into the checkpoint and records it as
//! applied in one transaction; only then is the receipt acknowledged. The loop
//! calls [`SteeringControl::apply`] only between complete tool rounds, so a
//! guidance message never separates a tool call from its result.
use crate::model::Message;
use anyhow::{Context, Result};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::mpsc;
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
            // Guidance may already be queued for a turn that is resuming.
            pending: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Marks that guidance may be waiting.
    pub(crate) fn notify(&self) {
        self.pending.store(true, Ordering::Release);
    }

    /// Applies queued guidance to `messages` and acknowledges it. Returns how
    /// many guidance messages were added.
    pub(crate) async fn apply(
        &self,
        session: Uuid,
        turn: Uuid,
        messages: &mut Vec<Message>,
        events: &mpsc::Sender<EngineEvent>,
    ) -> Result<usize> {
        if !self.pending.swap(false, Ordering::AcqRel) {
            return Ok(0);
        }
        let checkpoint = serde_json::to_value(&*messages)?;
        let store = self.store.clone();
        let result =
            tokio::task::spawn_blocking(move || store.apply_steering(session, turn, &checkpoint))
                .await??;
        let count = result.applied.len();
        if count > 0 {
            *messages = serde_json::from_value(result.messages)
                .context("Guidance checkpoint is not a message list")?;
        }
        if result.has_more {
            self.notify();
        }
        for applied in result.applied {
            // The durable commit precedes acknowledgement.
            events
                .send(EngineEvent::Steering {
                    receipt: applied.receipt,
                })
                .await
                .context("Guidance acknowledgement channel closed")?;
        }
        Ok(count)
    }
}
