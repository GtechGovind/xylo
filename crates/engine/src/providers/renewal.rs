//! Renewal of short-lived ChatGPT plan credentials for model requests.
//!
//! A still-valid credential is reused without asking the host. The host is
//! asked only when the credential is near expiry or the provider rejected it
//! as expired; concurrent requests wait on that single renewal because the
//! state lock is held while the host answers. Only a request that produced no
//! output is replayed, and only once.
use super::*;
use crate::ProviderCredentialResolver;
use crate::model::StreamEvent;
use tokio::sync::Mutex;
use xylo_core::WorkerCredential;

/// Only [`NativeProvider`]'s parsed HTTP 401 with the exact `token_expired`
/// code produces this marker; raw provider text never makes a request
/// eligible for renewal.
pub(super) const TOKEN_EXPIRED: &str = "ChatGPT access token expired (HTTP 401)";

/// Credentials closer than this to expiry are renewed before use.
const RENEWAL_MARGIN_MS: i64 = 5 * 60_000;

type Factory = Arc<
    dyn Fn(&ProviderConfig, WorkerCredential) -> Result<Arc<dyn ModelProvider>, ModelError>
        + Send
        + Sync,
>;

struct State {
    generation: u64,
    credential: WorkerCredential,
    provider: Option<Arc<dyn ModelProvider>>,
}

#[derive(Clone)]
struct RenewableProvider {
    config: ProviderConfig,
    state: Arc<Mutex<State>>,
    resolver: Arc<dyn ProviderCredentialResolver>,
    factory: Factory,
}

/// Builds the provider for a host-authorized credential. ChatGPT plan
/// credentials renew through `resolver`; every other credential is static.
pub(crate) fn build_renewable(
    config: &ProviderConfig,
    credential: WorkerCredential,
    resolver: Option<Arc<dyn ProviderCredentialResolver>>,
) -> Result<Arc<dyn ModelProvider>> {
    let chatgpt = record(&credential).is_some_and(|value| value["type"] == "chatgpt");
    let Some(resolver) = resolver.filter(|_| chatgpt) else {
        return super::build(config, credential.0);
    };
    let endpoint = normalize_provider(config)?;
    ensure!(
        config.family == ApiFamily::OpenAiResponses
            && endpoint.base_url.as_str().trim_end_matches('/') == "https://api.openai.com/v1",
        "ChatGPT renewal requires the official public Responses API"
    );
    Ok(Arc::new(RenewableProvider {
        config: config.clone(),
        state: Arc::new(Mutex::new(State {
            generation: 0,
            credential,
            provider: None,
        })),
        resolver,
        factory: Arc::new(|config, credential| {
            super::build(config, credential.0).map_err(|_| {
                ModelError::Authentication(
                    "The renewed connection is invalid. Reconnect in Providers.".into(),
                )
            })
        }),
    }))
}

fn record(credential: &WorkerCredential) -> Option<Value> {
    credential
        .0
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
}

fn near_expiry(credential: &WorkerCredential) -> bool {
    record(credential)
        .and_then(|value| value["expires_at_ms"].as_i64())
        .is_none_or(|expires| expires <= xylo_core::now_ms() + RENEWAL_MARGIN_MS)
}

fn same_account(previous: &WorkerCredential, renewed: &WorkerCredential) -> bool {
    let (Some(previous), Some(renewed)) = (record(previous), record(renewed)) else {
        return false;
    };
    renewed["type"] == "chatgpt"
        && ["subject", "client_id", "issuer", "ext_agent_host_id"]
            .iter()
            .all(|field| {
                previous[*field].as_str().is_some_and(|value| {
                    !value.is_empty() && renewed[*field].as_str() == Some(value)
                })
            })
}

fn expired_rejection(error: &ModelError) -> bool {
    matches!(error, ModelError::Authentication(message) if message == TOKEN_EXPIRED)
}

impl RenewableProvider {
    /// The provider to use, its credential generation and whether that
    /// generation is newer than the one the caller last used. `rejected`
    /// names the generation the provider just refused.
    async fn prepare(
        &self,
        rejected: Option<u64>,
        cancel: &CancellationToken,
    ) -> Result<(Arc<dyn ModelProvider>, u64, bool), ModelError> {
        let mut state = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ModelError::Interrupted),
            state = self.state.lock() => state,
        };
        if let Some(provider) = &state.provider
            && !near_expiry(&state.credential)
        {
            match rejected {
                None => return Ok((provider.clone(), state.generation, false)),
                // Another request already renewed past the rejected generation.
                Some(generation) if generation < state.generation => {
                    return Ok((provider.clone(), state.generation, true));
                }
                Some(_) => {}
            }
        }
        if rejected.is_none() && state.provider.is_none() && !near_expiry(&state.credential) {
            let provider = (self.factory)(&self.config, state.credential.clone())?;
            state.provider = Some(provider.clone());
            return Ok((provider, state.generation, false));
        }
        let force = rejected == Some(state.generation);
        let update = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ModelError::Interrupted),
            result = self.resolver.resolve(state.generation, force, cancel.clone()) => result?,
        };
        if cancel.is_cancelled() {
            return Err(ModelError::Interrupted);
        }
        if update.generation < state.generation
            || (force && update.generation == state.generation)
            || !same_account(&state.credential, &update.credential)
        {
            return Err(ModelError::Authentication(
                "The renewed account did not match this task. Reconnect in Providers.".into(),
            ));
        }
        let refreshed = update.generation != state.generation;
        let provider = match &state.provider {
            Some(provider) if !refreshed && update.credential.0 == state.credential.0 => {
                provider.clone()
            }
            _ => (self.factory)(&self.config, update.credential.clone())?,
        };
        state.generation = update.generation;
        state.credential = update.credential;
        state.provider = Some(provider.clone());
        Ok((provider, state.generation, refreshed))
    }
}

impl ModelProvider for RenewableProvider {
    fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> EventStream {
        let this = self.clone();
        Box::pin(async_stream::try_stream! {
            let (provider, generation, refreshed) = this.prepare(None, &cancel).await?;
            let mut stream = provider.stream(request.clone(), cancel.clone());
            let mut emitted = false;
            let mut retried = refreshed;
            loop {
                let next = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => Err(ModelError::Interrupted),
                    next = stream.next() => Ok(next),
                }?;
                match next {
                    None => break,
                    Some(Err(error)) if !emitted && !retried && expired_rejection(&error) => {
                        drop(stream);
                        let (renewed, _, _) = this.prepare(Some(generation), &cancel).await?;
                        stream = renewed.stream(request.clone(), cancel.clone());
                        retried = true;
                    }
                    Some(event) => {
                        let event: StreamEvent = event?;
                        emitted = true;
                        yield event;
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests;
