//! Host-owned, active-run-bound provider renewal over the worker's private pipe.
use async_trait::async_trait;
use hermes_core::AgentError;
use tokio_util::sync::CancellationToken;
use xylo_core::ProviderCredentialUpdate;

#[async_trait]
pub trait ProviderCredentialResolver: Send + Sync {
    async fn resolve(
        &self,
        generation: u64,
        force: bool,
        cancel: CancellationToken,
    ) -> Result<ProviderCredentialUpdate, AgentError>;
}
