//! Host-owned provider credential renewal over the worker's private pipe.
use crate::model::ModelError;
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use xylo_core::ProviderCredentialUpdate;

/// Asks the host for the current (or a forcibly renewed) provider credential.
#[async_trait]
pub trait ProviderCredentialResolver: Send + Sync {
    /// Returns the host's credential. `force` asks for a renewal past
    /// `generation`, after the provider rejected that generation.
    async fn resolve(
        &self,
        generation: u64,
        force: bool,
        cancel: CancellationToken,
    ) -> Result<ProviderCredentialUpdate, ModelError>;
}
