//! Credential replies are transient private-stdin commands, never UI events.
use async_trait::async_trait;
use std::sync::Mutex;
use std::{
    collections::HashMap,
    sync::{Arc, atomic::AtomicU64},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use xylo_core::{EngineEvent, Envelope, ProviderCredentialUpdate};
use xylo_engine::ProviderCredentialError as AgentError;
use xylo_engine::ProviderCredentialResolver;

type Reply = Result<ProviderCredentialUpdate, String>;
pub(crate) struct CredentialBroker {
    pending: Arc<Mutex<HashMap<Uuid, oneshot::Sender<Reply>>>>,
    output: mpsc::Sender<Envelope<EngineEvent>>,
    sequence: Arc<AtomicU64>,
    run_request_id: Uuid,
    session_id: Uuid,
}
struct Pending {
    id: Uuid,
    entries: Arc<Mutex<HashMap<Uuid, oneshot::Sender<Reply>>>>,
}
impl Drop for Pending {
    fn drop(&mut self) {
        self.entries
            .lock()
            .expect("Credential reply map poisoned")
            .remove(&self.id);
    }
}
impl CredentialBroker {
    pub(crate) fn new(
        output: mpsc::Sender<Envelope<EngineEvent>>,
        sequence: Arc<AtomicU64>,
        run_request_id: Uuid,
        session_id: Uuid,
    ) -> Self {
        Self {
            pending: Default::default(),
            output,
            sequence,
            run_request_id,
            session_id,
        }
    }
    pub(crate) fn reply(
        &self,
        run_request_id: Uuid,
        session_id: Option<Uuid>,
        id: Uuid,
        result: Reply,
    ) {
        if run_request_id != self.run_request_id || session_id != Some(self.session_id) {
            return;
        }
        if let Some(reply) = self
            .pending
            .lock()
            .expect("Credential reply map poisoned")
            .remove(&id)
        {
            let _ = reply.send(result);
        }
    }
}
#[async_trait]
impl ProviderCredentialResolver for CredentialBroker {
    async fn resolve(
        &self,
        generation: u64,
        force: bool,
        cancel: CancellationToken,
    ) -> Result<ProviderCredentialUpdate, AgentError> {
        let interrupted = || AgentError::Interrupted {
            message: Some("Provider renewal cancelled".into()),
        };
        if cancel.is_cancelled() {
            return Err(interrupted());
        }
        let id = Uuid::new_v4();
        let (sender, receiver) = oneshot::channel();
        {
            let mut pending = self.pending.lock().expect("Credential reply map poisoned");
            if pending.len() >= 4 {
                return Err(AgentError::AuthFailed(
                    "Provider renewal is busy. Retry shortly.".into(),
                ));
            }
            pending.insert(id, sender);
        }
        let _pending = Pending {
            id,
            entries: self.pending.clone(),
        };
        let result = tokio::time::timeout(Duration::from_secs(65), async {
            tokio::select! {
                _ = cancel.cancelled() => return Err(interrupted()),
                result = super::send(&self.output, &self.sequence, self.run_request_id, Some(self.session_id), EngineEvent::ProviderCredentialRequested { request_id: id, generation, force }) => {
                    result.map_err(|_| AgentError::AuthFailed("The host could not renew this connection. Reconnect in Providers.".into()))?;
                }
            }
            tokio::select! {
                _ = cancel.cancelled() => Err(interrupted()),
                result = receiver => result.map_err(|_| AgentError::AuthFailed("The host could not renew this connection. Reconnect in Providers.".into()))?.map_err(AgentError::AuthFailed),
            }
        }).await.map_err(|_| AgentError::Timeout("Provider renewal timed out. Reconnect in Providers.".into()))?;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Arc<CredentialBroker>, mpsc::Receiver<Envelope<EngineEvent>>) {
        let (output, incoming) = mpsc::channel(8);
        (
            Arc::new(CredentialBroker::new(
                output,
                Arc::new(AtomicU64::new(0)),
                Uuid::new_v4(),
                Uuid::new_v4(),
            )),
            incoming,
        )
    }
    async fn request(incoming: &mut mpsc::Receiver<Envelope<EngineEvent>>) -> Uuid {
        let event = incoming.recv().await.unwrap();
        assert!(
            !serde_json::to_string(&event)
                .unwrap()
                .contains("access_token")
        );
        let EngineEvent::ProviderCredentialRequested { request_id, .. } = event.payload else {
            panic!("Expected renewal request")
        };
        request_id
    }
    #[tokio::test]
    async fn only_matching_active_run_and_inner_request_can_deliver_a_private_reply() {
        let (broker, mut incoming) = fixture();
        let running = broker.clone();
        let task =
            tokio::spawn(async move { running.resolve(0, true, CancellationToken::new()).await });
        let request = request(&mut incoming).await;
        broker.reply(
            Uuid::new_v4(),
            Some(broker.session_id),
            request,
            Err("wrong run".into()),
        );
        broker.reply(
            broker.run_request_id,
            Some(Uuid::new_v4()),
            request,
            Err("wrong session".into()),
        );
        broker.reply(
            broker.run_request_id,
            Some(broker.session_id),
            Uuid::new_v4(),
            Err("wrong inner request".into()),
        );
        assert_eq!(broker.pending.lock().unwrap().len(), 1);
        broker.reply(
            broker.run_request_id,
            Some(broker.session_id),
            request,
            Ok(ProviderCredentialUpdate {
                generation: 1,
                credential: xylo_core::WorkerCredential(None),
            }),
        );
        assert_eq!(task.await.unwrap().unwrap().generation, 1);
        assert!(broker.pending.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn cancellation_drops_waiter_and_a_late_credential_cannot_revive_it() {
        let (broker, mut incoming) = fixture();
        let running = broker.clone();
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move { running.resolve(0, true, task_cancel).await });
        let request = request(&mut incoming).await;
        cancel.cancel();
        assert!(matches!(
            task.await.unwrap(),
            Err(AgentError::Interrupted { .. })
        ));
        assert!(broker.pending.lock().unwrap().is_empty());
        broker.reply(
            broker.run_request_id,
            Some(broker.session_id),
            request,
            Ok(ProviderCredentialUpdate {
                generation: 1,
                credential: xylo_core::WorkerCredential(None),
            }),
        );
        assert!(broker.pending.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn abandoned_provider_future_releases_its_private_reply_slot() {
        let (broker, mut incoming) = fixture();
        let running = broker.clone();
        let task =
            tokio::spawn(async move { running.resolve(0, true, CancellationToken::new()).await });
        request(&mut incoming).await;
        task.abort();
        let _ = task.await;
        assert!(broker.pending.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn private_request_capacity_is_bounded_and_cancelled_slots_are_reusable() {
        let (broker, mut incoming) = fixture();
        let cancel = CancellationToken::new();
        let mut tasks = Vec::new();
        for _ in 0..4 {
            let running = broker.clone();
            let task_cancel = cancel.clone();
            tasks.push(tokio::spawn(async move {
                running.resolve(0, false, task_cancel).await
            }));
            request(&mut incoming).await;
        }
        assert!(matches!(
            broker.resolve(0, false, CancellationToken::new()).await,
            Err(AgentError::AuthFailed(_))
        ));
        assert_eq!(broker.pending.lock().unwrap().len(), 4);
        cancel.cancel();
        for task in tasks {
            assert!(matches!(
                task.await.unwrap(),
                Err(AgentError::Interrupted { .. })
            ));
        }
        assert!(broker.pending.lock().unwrap().is_empty());
    }
}
