//! Renew only rejected model requests, never an agent turn or tool action.
use super::*;
use crate::ProviderCredentialResolver;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use xylo_core::WorkerCredential;

// Only NativeProvider's parsed HTTP401 + exact code can produce this marker.
// Raw provider messages never participate in renewal eligibility.
pub(super) const TOKEN_EXPIRED: &str = "ChatGPT access token expired (HTTP 401)";
type Factory = Arc<
    dyn Fn(&ProviderConfig, WorkerCredential) -> Result<Arc<dyn LlmProvider>, AgentError>
        + Send
        + Sync,
>;
struct State {
    generation: u64,
    credential: WorkerCredential,
    provider: Option<Arc<dyn LlmProvider>>,
}
#[derive(Clone)]
struct RenewableProvider {
    config: ProviderConfig,
    state: Arc<Mutex<State>>,
    resolver: Arc<dyn ProviderCredentialResolver>,
    cancel: CancellationToken,
    factory: Factory,
}
pub(crate) fn build_renewable(
    config: &ProviderConfig,
    credential: WorkerCredential,
    resolver: Option<Arc<dyn ProviderCredentialResolver>>,
    cancel: CancellationToken,
) -> Result<Arc<dyn LlmProvider>> {
    let chatgpt = credential
        .0
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .is_some_and(|value| value["type"] == "chatgpt");
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
        cancel,
        factory: Arc::new(|config, credential| {
            super::build(config, credential.0).map_err(|_| {
                AgentError::AuthFailed(
                    "The renewed connection is invalid. Reconnect in Providers.".into(),
                )
            })
        }),
    }))
}
fn interrupted() -> AgentError {
    AgentError::Interrupted {
        message: Some("Provider renewal cancelled".into()),
    }
}
fn same_account(previous: &WorkerCredential, renewed: &WorkerCredential) -> bool {
    let parse = |credential: &WorkerCredential| {
        credential
            .0
            .as_deref()
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
    };
    let (Some(previous), Some(renewed)) = (parse(previous), parse(renewed)) else {
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
fn expired_rejection(error: &AgentError) -> bool {
    matches!(error,AgentError::AuthFailed(message) if message == TOKEN_EXPIRED)
}
impl RenewableProvider {
    async fn prepare(
        &self,
        force_generation: Option<u64>,
    ) -> Result<(Arc<dyn LlmProvider>, u64, bool), AgentError> {
        let mut state = tokio::select! { biased; _=self.cancel.cancelled()=>return Err(interrupted()), state=self.state.lock()=>state };
        let force = force_generation == Some(state.generation);
        // Every new model request checks the authoritative host vault, even
        // while our cached bearer is young: sign-out/account changes are local.
        let update = tokio::select! { biased;
            _=self.cancel.cancelled()=>return Err(interrupted()),
            result=self.resolver.resolve(state.generation,force,self.cancel.clone())=>result?,
        };
        if self.cancel.is_cancelled() {
            return Err(interrupted());
        }
        if update.generation < state.generation
            || (force && update.generation == state.generation)
            || !same_account(&state.credential, &update.credential)
        {
            return Err(AgentError::AuthFailed(
                "The renewed account did not match this task. Reconnect in Providers.".into(),
            ));
        }
        let refreshed = update.generation != state.generation;
        if refreshed || update.credential.0 != state.credential.0 {
            let provider = (self.factory)(&self.config, update.credential.clone())?;
            state.generation = update.generation;
            state.credential = update.credential;
            state.provider = Some(provider);
        }
        if state.provider.is_none() {
            state.provider = Some((self.factory)(&self.config, state.credential.clone())?);
        }
        Ok((
            state.provider.as_ref().unwrap().clone(),
            state.generation,
            refreshed,
        ))
    }
}
#[async_trait]
impl LlmProvider for RenewableProvider {
    async fn chat_completion(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        max_tokens: Option<u32>,
        temperature: Option<f64>,
        model: Option<&str>,
        extra: Option<&Value>,
    ) -> Result<LlmResponse, AgentError> {
        let (provider, generation, refreshed) = self.prepare(None).await?;
        let result = tokio::select! { biased; _=self.cancel.cancelled()=>return Err(interrupted()), result=provider.chat_completion(messages,tools,max_tokens,temperature,model,extra)=>result };
        match result {
            Err(error) if !refreshed && expired_rejection(&error) => {
                let (provider, _, _) = self.prepare(Some(generation)).await?;
                tokio::select! { biased; _=self.cancel.cancelled()=>Err(interrupted()), result=provider.chat_completion(messages,tools,max_tokens,temperature,model,extra)=>result }
            }
            result => result,
        }
    }
    fn chat_completion_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        max_tokens: Option<u32>,
        temperature: Option<f64>,
        model: Option<&str>,
        extra: Option<&Value>,
    ) -> BoxStream<'static, Result<StreamChunk, AgentError>> {
        let this = self.clone();
        let messages = messages.to_vec();
        let tools = tools.to_vec();
        let model = model.map(str::to_owned);
        let extra = extra.cloned();
        Box::pin(async_stream::try_stream! {
            let (mut provider,generation,refreshed) = this.prepare(None).await?;
            let mut stream = provider.chat_completion_stream(&messages,&tools,max_tokens,temperature,model.as_deref(),extra.as_ref());
            let mut emitted = false;
            let mut retried = false;
            loop {
                let next = tokio::select! { biased; _=this.cancel.cancelled()=>Err(interrupted()), next=stream.next()=>Ok(next) }?;
                match next {
                    None => break,
                    Some(Err(error)) if !emitted && !retried && !refreshed && expired_rejection(&error) => {
                        drop(stream);
                        let (renewed,_,_) = this.prepare(Some(generation)).await?;
                        provider = renewed;
                        stream = provider.chat_completion_stream(&messages,&tools,max_tokens,temperature,model.as_deref(),extra.as_ref());
                        retried = true;
                    }
                    Some(chunk) => { let chunk=chunk?; emitted=true; yield chunk; }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use xylo_core::ProviderCredentialUpdate;
    fn record(token: &str, expires: i64) -> WorkerCredential {
        WorkerCredential(Some(serde_json::json!({"type":"chatgpt","subject":"fixture-account","client_id":"fixture-client","issuer":"https://auth.openai.com","ext_agent_host_id":"fixture-host","access_token":token,"expires_at_ms":expires,"scopes":["chatgpt.tokens.use.direct"]}).to_string()))
    }
    struct Resolver {
        calls: Arc<AtomicUsize>,
        current: Mutex<ProviderCredentialUpdate>,
    }
    #[async_trait]
    impl ProviderCredentialResolver for Resolver {
        async fn resolve(
            &self,
            generation: u64,
            force: bool,
            _: CancellationToken,
        ) -> Result<ProviderCredentialUpdate, AgentError> {
            let mut current = self.current.lock().await;
            let expires = serde_json::from_str::<Value>(current.credential.0.as_deref().unwrap())
                .unwrap()["expires_at_ms"]
                .as_i64()
                .unwrap();
            if (force && generation == current.generation)
                || expires <= xylo_core::now_ms() + 60_000
            {
                self.calls.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await;
                current.generation += 1;
                current.credential = record("new", xylo_core::now_ms() + 3_600_000);
            }
            Ok(current.clone())
        }
    }
    struct Provider {
        fresh: bool,
        calls: Arc<AtomicUsize>,
        partial: bool,
        generic: bool,
        fail_fresh: bool,
    }
    #[async_trait]
    impl LlmProvider for Provider {
        async fn chat_completion(
            &self,
            _: &[Message],
            _: &[ToolSchema],
            _: Option<u32>,
            _: Option<f64>,
            _: Option<&str>,
            _: Option<&Value>,
        ) -> Result<LlmResponse, AgentError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            if !self.fresh || self.fail_fresh {
                return Err(AgentError::AuthFailed(
                    if self.generic {
                        "Provider returned HTTP 401 (invalid_api_key)"
                    } else {
                        TOKEN_EXPIRED
                    }
                    .into(),
                ));
            }
            Ok(LlmResponse {
                message: Message::assistant("fresh answer"),
                usage: None,
                model: "fixture".into(),
                finish_reason: Some("stop".into()),
            })
        }
        fn chat_completion_stream(
            &self,
            _: &[Message],
            _: &[ToolSchema],
            _: Option<u32>,
            _: Option<f64>,
            _: Option<&str>,
            _: Option<&Value>,
        ) -> BoxStream<'static, Result<StreamChunk, AgentError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let fresh = self.fresh;
            let partial = self.partial;
            let generic = self.generic;
            let fail = self.fail_fresh;
            Box::pin(async_stream::stream! {
                tokio::task::yield_now().await;
                if partial { yield Ok(StreamChunk{delta:Some(hermes_core::StreamDelta{content:Some("visible partial".into()),tool_calls:None,extra:None}),finish_reason:None,usage:None}); }
                if !fresh || fail { yield Err(AgentError::AuthFailed(if generic { "Provider returned HTTP 401 (invalid_api_key)" } else { TOKEN_EXPIRED }.into())); }
                else { yield Ok(StreamChunk{delta:Some(hermes_core::StreamDelta{content:Some("fresh answer".into()),tool_calls:None,extra:None}),finish_reason:Some("stop".into()),usage:None}); }
            })
        }
    }
    fn fixture(
        partial: bool,
        generic: bool,
        fail_fresh: bool,
        expired: bool,
    ) -> (RenewableProvider, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let renewals = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        (
            RenewableProvider {
                config: ProviderConfig::default(),
                state: Arc::new(Mutex::new(State {
                    generation: 0,
                    credential: record(
                        "old",
                        if expired {
                            0
                        } else {
                            xylo_core::now_ms() + 3_600_000
                        },
                    ),
                    provider: None,
                })),
                resolver: Arc::new(Resolver {
                    calls: renewals.clone(),
                    current: Mutex::new(ProviderCredentialUpdate {
                        generation: 0,
                        credential: record(
                            "old",
                            if expired {
                                0
                            } else {
                                xylo_core::now_ms() + 3_600_000
                            },
                        ),
                    }),
                }),
                cancel: CancellationToken::new(),
                factory: Arc::new(move |_, credential| {
                    let value: Value =
                        serde_json::from_str(credential.0.as_deref().unwrap()).unwrap();
                    Ok(Arc::new(Provider {
                        fresh: value["access_token"] == "new",
                        calls: observed.clone(),
                        partial,
                        generic,
                        fail_fresh,
                    }))
                }),
            },
            calls,
            renewals,
        )
    }
    #[tokio::test]
    async fn explicit_expired_rejection_renews_once_and_retries_only_that_request() {
        let (provider, calls, renewals) = fixture(false, false, false, false);
        let reply = provider
            .chat_completion(&[Message::user("fixture")], &[], None, None, None, None)
            .await
            .unwrap();
        assert_eq!(reply.message.content.as_deref(), Some("fresh answer"));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(renewals.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn expired_before_next_model_call_is_renewed_before_dispatch() {
        let (provider, calls, renewals) = fixture(false, false, false, true);
        provider
            .chat_completion(&[], &[], None, None, None, None)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(renewals.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn partial_stream_and_generic_auth_failure_are_never_replayed() {
        for (partial, generic) in [(true, false), (false, true)] {
            let (provider, calls, renewals) = fixture(partial, generic, false, false);
            let result: Vec<_> = provider
                .chat_completion_stream(&[], &[], None, None, None, None)
                .collect()
                .await;
            assert!(result.last().unwrap().is_err());
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(renewals.load(Ordering::SeqCst), 0);
        }
    }
    #[tokio::test]
    async fn renewed_request_rejection_is_terminal_without_a_refresh_loop() {
        let (provider, calls, renewals) = fixture(false, false, true, false);
        assert!(
            provider
                .chat_completion(&[], &[], None, None, None, None)
                .await
                .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(renewals.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn cancelled_request_does_not_dispatch_or_refresh() {
        let (provider, calls, renewals) = fixture(false, false, false, true);
        provider.cancel.cancel();
        assert!(matches!(
            provider
                .chat_completion(&[], &[], None, None, None, None)
                .await,
            Err(AgentError::Interrupted { .. })
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(renewals.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn rejected_stream_renews_before_the_first_chunk_and_yields_only_fresh_output() {
        let (provider, calls, renewals) = fixture(false, false, false, false);
        let result: Vec<_> = provider
            .chat_completion_stream(&[], &[], None, None, None, None)
            .collect()
            .await;
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0]
                .as_ref()
                .unwrap()
                .delta
                .as_ref()
                .unwrap()
                .content
                .as_deref(),
            Some("fresh answer")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(renewals.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn shared_parent_and_child_requests_use_one_renewal_generation() {
        let (provider, calls, renewals) = fixture(false, false, false, false);
        let parent = provider.clone();
        let child = provider.clone();
        let (a, b) = tokio::join!(
            parent.chat_completion(&[], &[], None, None, None, None),
            child.chat_completion(&[], &[], None, None, None, None)
        );
        assert_eq!(a.unwrap().message.content.as_deref(), Some("fresh answer"));
        assert_eq!(b.unwrap().message.content.as_deref(), Some("fresh answer"));
        assert_eq!(renewals.load(Ordering::SeqCst), 1);
        assert!((3..=4).contains(&calls.load(Ordering::SeqCst)));
    }
    #[tokio::test]
    async fn proactive_renewal_spends_the_request_budget_without_a_second_force_refresh() {
        let (provider, calls, renewals) = fixture(false, false, true, true);
        assert!(
            provider
                .chat_completion(&[], &[], None, None, None, None)
                .await
                .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(renewals.load(Ordering::SeqCst), 1);
    }
    struct WaitingResolver {
        entered: Arc<tokio::sync::Notify>,
    }
    #[async_trait]
    impl ProviderCredentialResolver for WaitingResolver {
        async fn resolve(
            &self,
            generation: u64,
            force: bool,
            _: CancellationToken,
        ) -> Result<ProviderCredentialUpdate, AgentError> {
            if !force {
                return Ok(ProviderCredentialUpdate {
                    generation,
                    credential: record("old", xylo_core::now_ms() + 3_600_000),
                });
            }
            self.entered.notify_one();
            std::future::pending().await
        }
    }
    #[tokio::test]
    async fn cancellation_during_renewal_never_replays_the_model_request() {
        let (mut provider, calls, _) = fixture(false, false, false, false);
        let entered = Arc::new(tokio::sync::Notify::new());
        provider.resolver = Arc::new(WaitingResolver {
            entered: entered.clone(),
        });
        let cancel = provider.cancel.clone();
        let pending = tokio::spawn(async move {
            provider
                .chat_completion(&[], &[], None, None, None, None)
                .await
        });
        entered.notified().await;
        cancel.cancel();
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(1), pending)
                .await
                .unwrap()
                .unwrap(),
            Err(AgentError::Interrupted { .. })
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
    struct WrongAccount;
    #[async_trait]
    impl ProviderCredentialResolver for WrongAccount {
        async fn resolve(
            &self,
            generation: u64,
            _: bool,
            _: CancellationToken,
        ) -> Result<ProviderCredentialUpdate, AgentError> {
            let mut value: Value = serde_json::from_str(
                record("new", xylo_core::now_ms() + 3_600_000)
                    .0
                    .as_deref()
                    .unwrap(),
            )
            .unwrap();
            value["subject"] = "different-account".into();
            Ok(ProviderCredentialUpdate {
                generation: generation + 1,
                credential: WorkerCredential(Some(value.to_string())),
            })
        }
    }
    #[tokio::test]
    async fn renewed_account_identity_mismatch_fails_closed_without_dispatch() {
        let (mut provider, calls, _) = fixture(false, false, false, false);
        provider.resolver = Arc::new(WrongAccount);
        let error = provider
            .chat_completion(&[], &[], None, None, None, None)
            .await
            .unwrap_err();
        assert!(matches!(error, AgentError::AuthFailed(_)));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    struct AccountState {
        signed_out: Arc<std::sync::atomic::AtomicBool>,
        credential: WorkerCredential,
    }
    #[async_trait]
    impl ProviderCredentialResolver for AccountState {
        async fn resolve(
            &self,
            generation: u64,
            _: bool,
            _: CancellationToken,
        ) -> Result<ProviderCredentialUpdate, AgentError> {
            if self.signed_out.load(Ordering::SeqCst) {
                return Err(AgentError::AuthFailed(
                    "Reconnect this account in Providers.".into(),
                ));
            }
            Ok(ProviderCredentialUpdate {
                generation,
                credential: self.credential.clone(),
            })
        }
    }
    #[tokio::test]
    async fn sign_out_blocks_a_fresh_cached_bearer_before_the_next_model_request() {
        let (mut provider, calls, _) = fixture(false, false, false, false);
        let credential = record("new", xylo_core::now_ms() + 3_600_000);
        provider.state.lock().await.credential = credential.clone();
        let signed_out = Arc::new(std::sync::atomic::AtomicBool::new(false));
        provider.resolver = Arc::new(AccountState {
            signed_out: signed_out.clone(),
            credential,
        });
        provider
            .chat_completion(&[], &[], None, None, None, None)
            .await
            .unwrap();
        signed_out.store(true, Ordering::SeqCst);
        assert!(matches!(
            provider
                .chat_completion(&[], &[], None, None, None, None)
                .await,
            Err(AgentError::AuthFailed(_))
        ));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "Sign-out must block dispatch even before the cached token's expiry"
        );
    }
}
