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
            Some(format!("{:x}", Sha256::digest(token.as_bytes())))
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
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    fn record(token: &str) -> String {
        serde_json::json!({"type":"chatgpt","access_token":token}).to_string()
    }
    fn run() -> RunCredentials {
        RunCredentials::new(
            Uuid::new_v4(),
            ProviderConfig::default(),
            WorkerCredential(Some(record("old"))),
            CancellationToken::new(),
        )
    }
    #[tokio::test]
    async fn concurrent_expiry_rejections_share_one_refresh_generation() {
        let run = Arc::new(run());
        let calls = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let run = run.clone();
            let calls = calls.clone();
            tasks.push(tokio::spawn(async move {
                run.resolve_with(run.request, 0, true, move |hash| async move {
                    if let Some(hash) = hash {
                        assert_eq!(hash, format!("{:x}", Sha256::digest(b"old")));
                        calls.fetch_add(1, Ordering::SeqCst);
                    }
                    tokio::task::yield_now().await;
                    Ok(Some(record("new")))
                })
                .await
                .unwrap()
            }));
        }
        for task in tasks {
            let response = task.await.unwrap();
            assert_eq!(response.generation, 1);
            assert_eq!(response.credential.0, Some(record("new")));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn stale_run_future_generation_and_cancel_never_touch_credentials() {
        let run = run();
        assert!(
            run.resolve_with(Uuid::new_v4(), 0, true, |_| async { panic!("stale run") })
                .await
                .is_err()
        );
        assert!(
            run.resolve_with(run.request, 1, true, |_| async {
                panic!("future generation")
            })
            .await
            .is_err()
        );
        run.cancel.cancel();
        assert!(
            run.resolve_with(run.request, 0, true, |_| async { panic!("cancelled run") })
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn cancellation_during_refresh_prevents_publication() {
        let run = run();
        let cancel = run.cancel.clone();
        assert!(
            run.resolve_with(run.request, 0, true, move |_| async move {
                cancel.cancel();
                Ok(Some(record("new")))
            })
            .await
            .is_err()
        );
        assert_eq!(run.state.lock().await.generation, 0);
    }
    #[tokio::test]
    async fn stale_generation_rechecks_signout_after_another_request_renewed() {
        let run = run();
        run.resolve_with(run.request, 0, true, |_| async { Ok(Some(record("new"))) })
            .await
            .unwrap();
        assert!(
            run.resolve_with(run.request, 0, true, |_| async {
                anyhow::bail!("signed out")
            })
            .await
            .is_err()
        );
    }
    #[tokio::test]
    async fn account_replacement_cannot_redirect_an_existing_turn() {
        let run = run();
        let replacement = serde_json::json!({"type":"chatgpt","access_token":"new", "subject":"different-account"}).to_string();
        assert!(
            run.resolve_with(run.request, 0, false, |_| async { Ok(Some(replacement)) })
                .await
                .is_err()
        );
        assert_eq!(run.state.lock().await.generation, 0);
    }
    #[tokio::test]
    async fn ordinary_preflight_rechecks_current_vault_even_without_generation_change() {
        let run = run();
        for _ in 0..2 {
            let response = run
                .resolve_with(run.request, 0, false, |hash| async move {
                    assert!(hash.is_none());
                    Ok(Some(record("old")))
                })
                .await
                .unwrap();
            assert_eq!(response.generation, 0);
        }
        assert!(
            run.resolve_with(run.request, 0, false, |_| async {
                anyhow::bail!("signed out")
            })
            .await
            .is_err()
        );
    }
}
