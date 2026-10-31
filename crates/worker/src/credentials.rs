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
use xylo_engine::ModelError;
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
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
    ) -> Result<ProviderCredentialUpdate, ModelError> {
        let interrupted = || ModelError::Interrupted;
        if cancel.is_cancelled() {
            return Err(interrupted());
        }
        let id = Uuid::new_v4();
        let (sender, receiver) = oneshot::channel();
        {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if pending.len() >= 4 {
                return Err(ModelError::Authentication(
                    "Provider renewal is busy. Retry shortly.".into(),
                ));
            }
            pending.insert(id, sender);
        }
        let _pending = Pending {
            id,
            entries: self.pending.clone(),
        };
        tokio::time::timeout(Duration::from_secs(65), async {
            tokio::select! {
                _ = cancel.cancelled() => return Err(interrupted()),
                result = super::send(&self.output, &self.sequence, self.run_request_id, Some(self.session_id), EngineEvent::ProviderCredentialRequested { request_id: id, generation, force }) => {
                    result.map_err(|_| ModelError::Authentication("The host could not renew this connection. Reconnect in Providers.".into()))?;
                }
            }
            tokio::select! {
                _ = cancel.cancelled() => Err(interrupted()),
                result = receiver => result.map_err(|_| ModelError::Authentication("The host could not renew this connection. Reconnect in Providers.".into()))?.map_err(ModelError::Authentication),
            }
        }).await.map_err(|_| ModelError::Timeout("Provider renewal timed out. Reconnect in Providers.".into()))?
    }
}

#[cfg(test)]
mod tests;
