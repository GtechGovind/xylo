//! Active-turn authorization for private worker credential renewal.
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{future::Future, time::Duration};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use xylo_core::{ProviderConfig, ProviderCredentialUpdate, WorkerCredential};
use xylo_services::{CredentialVault, providers::ChatGptSignInService};

pub(super) struct RunCredentials {
    request: Uuid,
    config: ProviderConfig,
    pub cancel: CancellationToken,
    state: Mutex<ProviderCredentialUpdate>,
}
impl RunCredentials {
    pub fn new(
        request: Uuid,
        config: ProviderConfig,
        credential: WorkerCredential,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            request,
            config,
            cancel,
            state: Mutex::new(ProviderCredentialUpdate {
                generation: 0,
                credential,
            }),
        }
    }
    pub async fn resolve(
        &self,
        request: Uuid,
        generation: u64,
        force: bool,
        accounts: ChatGptSignInService,
        vault: CredentialVault,
    ) -> Result<ProviderCredentialUpdate> {
        let config = self.config.clone();
        let cancel = self.cancel.clone();
        self.resolve_with(request, generation, force, move |rejected| async move {
            match rejected {
                Some(hash) => accounts
                    .refresh_rejected_provider_credential(&config, vault, &hash, &cancel)
                    .await
                    .map(Some),
                None => {
                    accounts
                        .resolve_provider_credential(&config, vault, &cancel)
                        .await
                }
            }
        })
        .await
    }
    async fn resolve_with<F, Fut>(
        &self,
        request: Uuid,
        generation: u64,
        force: bool,
        refresh: F,
    ) -> Result<ProviderCredentialUpdate>
    where
        F: FnOnce(Option<String>) -> Fut,
        Fut: Future<Output = Result<Option<String>>>,
    {
        ensure!(
            request == self.request,
            "Credential request belongs to another turn"
        );
        ensure!(!self.cancel.is_cancelled(), "Credential request cancelled");
        let mut state = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => anyhow::bail!("Credential request cancelled"),
            state = self.state.lock() => state,
        };
        ensure!(
            generation <= state.generation,
            "Unknown credential generation"
        );
        // A sibling may have renewed already; still consult the vault so a
        // subsequent sign-out or account replacement cannot revive cached access.
        let force = force && generation == state.generation;
        let rejected = if force {
            let value: serde_json::Value = serde_json::from_str(
                state
                    .credential
                    .0
                    .as_deref()
                    .context("Missing account credential")?,
            )?;
            ensure!(
                value["type"] == "chatgpt",
                "This provider does not support account renewal"
            );
            let token = value["access_token"]
                .as_str()
                .filter(|token| !token.is_empty())
                .context("Missing account token")?;
            Some(hex::encode(Sha256::digest(token.as_bytes())))
        } else {
            None
        };
        let credential = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => anyhow::bail!("Credential request cancelled"),
            result = tokio::time::timeout(Duration::from_secs(45), refresh(rejected)) => result.context("Credential renewal timed out")??,
        };
        ensure!(!self.cancel.is_cancelled(), "Credential request cancelled");
        if let Some(original) = state
            .credential
            .0
            .as_deref()
            .and_then(|record| serde_json::from_str::<serde_json::Value>(record).ok())
            && original["type"] == "chatgpt"
        {
            let renewed: serde_json::Value =
                serde_json::from_str(credential.as_deref().context("Account was removed")?)?;
            for field in [
                "type",
                "issuer",
                "subject",
                "client_id",
                "ext_agent_host_id",
            ] {
                ensure!(
                    original[field] == renewed[field],
                    "Account changed during the active turn"
                );
            }
        }
        if credential != state.credential.0 || force {
            state.generation = state
                .generation
                .checked_add(1)
                .context("Credential generation exhausted")?;
            state.credential = WorkerCredential(credential);
        }
        Ok(state.clone())
    }
}

#[cfg(test)]
mod tests;
