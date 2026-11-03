//! Ordered, bounded worker-to-host activity transport; nothing is written to disk.
use super::*;
use tokio::sync::broadcast::{Receiver, error::TryRecvError};
use xylo_services::LiveActionUpdate;

pub(super) struct Forwarder {
    store: Store,
    updates: Receiver<LiveActionUpdate>,
    session: uuid::Uuid,
    turn: uuid::Uuid,
    incomplete: bool,
}

impl Forwarder {
    pub(super) fn new(store: Store, session: uuid::Uuid, turn: uuid::Uuid) -> Self {
        let updates = store.subscribe_live_actions();
        Self {
            store,
            updates,
            session,
            turn,
            incomplete: false,
        }
    }

    pub(super) async fn drain(&mut self, ending: &RunEnding) -> Result<()> {
        loop {
            match self.updates.try_recv() {
                Ok(update) => {
                    if update.scope() != (self.session, self.turn) {
                        continue;
                    }
                    let payload = serde_json::to_value(update)?;
                    // Leave room for the envelope and JSONL delimiter. Do not
                    // truncate evidence and then present it as a verified result.
                    if serde_json::to_vec(&payload)?.len() > MAX_PROTOCOL_BYTES - 1024 {
                        self.incomplete(ending).await?;
                        continue;
                    }
                    self.send(ending, payload).await?;
                }
                Err(TryRecvError::Lagged(_)) => self.incomplete(ending).await?,
                Err(TryRecvError::Empty) => return Ok(()),
                Err(TryRecvError::Closed) => {
                    self.incomplete(ending).await?;
                    return Ok(());
                }
            }
        }
    }

    pub(super) async fn complete(&self, ending: &RunEnding) -> Result<()> {
        self.send(
            ending,
            serde_json::to_value(LiveActionUpdate::TurnComplete {
                session_id: self.session,
                turn_id: self.turn,
            })?,
        )
        .await
    }

    async fn incomplete(&mut self, ending: &RunEnding) -> Result<()> {
        if self.incomplete {
            return Ok(());
        }
        self.incomplete = true;
        self.store
            .mark_live_action_incomplete(self.session, self.turn)?;
        self.send(
            ending,
            serde_json::to_value(LiveActionUpdate::Incomplete {
                session_id: self.session,
                turn_id: self.turn,
            })?,
        )
        .await
    }

    async fn send(&self, ending: &RunEnding, update: serde_json::Value) -> Result<()> {
        super::send(
            &ending.output,
            &ending.sequence,
            ending.request_id,
            Some(self.session),
            EngineEvent::LiveActivity {
                turn_id: self.turn,
                update,
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xylo_services::ActionIntent;

    fn ending(
        store: Store,
        session: uuid::Uuid,
        turn: uuid::Uuid,
    ) -> (RunEnding, mpsc::Receiver<Envelope<EngineEvent>>) {
        let (output, receiver) = mpsc::channel(1024);
        (
            RunEnding {
                busy: Arc::new(AtomicBool::new(true)),
                store,
                session,
                turn,
                output,
                sequence: Arc::new(AtomicU64::new(1)),
                request_id: uuid::Uuid::new_v4(),
                session_id: Some(session),
            },
            receiver,
        )
    }

    fn action(session: uuid::Uuid, turn: uuid::Uuid) -> ActionIntent {
        let id = uuid::Uuid::new_v4();
        ActionIntent {
            id,
            session_id: session,
            turn_id: turn,
            name: "file_read".into(),
            arguments: serde_json::json!({"path":"sample.txt"}),
            writes: false,
            idempotency_key: id.to_string(),
        }
    }

    #[tokio::test]
    async fn lag_marks_turn_incomplete_before_the_final_marker() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("test.db")).unwrap();
        let (session, turn) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        let mut forward = Forwarder::new(store.clone(), session, turn);
        let (ending, mut received) = ending(store.clone(), session, turn);
        for _ in 0..300 {
            store.begin_action(&action(session, turn)).unwrap();
        }
        forward.drain(&ending).await.unwrap();
        forward.complete(&ending).await.unwrap();
        assert!(!store.live_action_evidence_complete(session, turn).unwrap());
        let mut incomplete = false;
        let mut complete = false;
        while let Ok(event) = received.try_recv() {
            assert!(!complete, "No evidence may follow the final marker");
            let EngineEvent::LiveActivity { turn_id, update } = event.payload else {
                panic!("Expected private activity")
            };
            assert_eq!(turn_id, turn);
            match serde_json::from_value::<LiveActionUpdate>(update).unwrap() {
                LiveActionUpdate::Incomplete { .. } => incomplete = true,
                LiveActionUpdate::TurnComplete { .. } => {
                    assert!(incomplete);
                    complete = true;
                }
                _ => {}
            }
        }
        assert!(complete);
    }

    #[tokio::test]
    async fn foreign_turn_updates_are_not_forwarded() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("test.db")).unwrap();
        let (session, turn) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        let mut forward = Forwarder::new(store.clone(), session, turn);
        let (ending, mut received) = ending(store.clone(), session, turn);
        store
            .begin_action(&action(session, uuid::Uuid::new_v4()))
            .unwrap();
        store
            .begin_action(&action(uuid::Uuid::new_v4(), turn))
            .unwrap();
        forward.drain(&ending).await.unwrap();
        assert!(received.try_recv().is_err());
    }
}
