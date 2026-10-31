use super::*;
use crate::model::{FinishReason, Message};
use std::sync::atomic::{AtomicUsize, Ordering};
use xylo_core::ProviderCredentialUpdate;

fn credential(token: &str, expires: i64) -> WorkerCredential {
    WorkerCredential(Some(serde_json::json!({"type":"chatgpt","subject":"fixture-account","client_id":"fixture-client","issuer":"https://auth.openai.com","ext_agent_host_id":"fixture-host","access_token":token,"expires_at_ms":expires,"scopes":["chatgpt.tokens.use.direct"]}).to_string()))
}
fn hour() -> i64 {
    xylo_core::now_ms() + 3_600_000
}
fn request() -> ModelRequest {
    ModelRequest {
        messages: vec![Message::user("fixture")],
        tools: Vec::new(),
        max_tokens: 16,
        reasoning: crate::model::Reasoning::Configured,
    }
}

struct Resolver {
    calls: Arc<AtomicUsize>,
    current: Mutex<ProviderCredentialUpdate>,
}
#[async_trait::async_trait]
impl ProviderCredentialResolver for Resolver {
    async fn resolve(
        &self,
        generation: u64,
        force: bool,
        _: CancellationToken,
    ) -> Result<ProviderCredentialUpdate, ModelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut current = self.current.lock().await;
        if (force && generation == current.generation) || near_expiry(&current.credential) {
            tokio::task::yield_now().await;
            current.generation += 1;
            current.credential = credential("new", hour());
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
impl ModelProvider for Provider {
    fn stream(&self, _: ModelRequest, _: CancellationToken) -> EventStream {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let (fresh, partial, generic, fail) =
            (self.fresh, self.partial, self.generic, self.fail_fresh);
        Box::pin(async_stream::stream! {
            tokio::task::yield_now().await;
            if partial {
                yield Ok(StreamEvent::Text("visible partial".into()));
            }
            if !fresh || fail {
                yield Err(ModelError::Authentication(if generic { "Provider returned HTTP 401 (invalid_api_key)" } else { TOKEN_EXPIRED }.into()));
            } else {
                yield Ok(StreamEvent::Text("fresh answer".into()));
                yield Ok(StreamEvent::Finished(FinishReason::Stop));
            }
        })
    }
}

struct Fixture {
    provider: RenewableProvider,
    calls: Arc<AtomicUsize>,
    renewals: Arc<AtomicUsize>,
}
fn fixture(partial: bool, generic: bool, fail_fresh: bool, expired: bool) -> Fixture {
    let calls = Arc::new(AtomicUsize::new(0));
    let renewals = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let initial = credential("old", if expired { 0 } else { hour() });
    Fixture {
        provider: RenewableProvider {
            config: ProviderConfig::default(),
            state: Arc::new(Mutex::new(State {
                generation: 0,
                credential: initial.clone(),
                provider: None,
            })),
            resolver: Arc::new(Resolver {
                calls: renewals.clone(),
                current: Mutex::new(ProviderCredentialUpdate {
                    generation: 0,
                    credential: initial,
                }),
            }),
            factory: Arc::new(move |_, credential| {
                let value = record(&credential).unwrap();
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
    }
}

#[tokio::test]
async fn explicit_expired_rejection_renews_once_and_retries_only_that_request() {
    let f = fixture(false, false, false, false);
    let reply = f
        .provider
        .complete(request(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(reply.message.content, "fresh answer");
    assert_eq!(f.calls.load(Ordering::SeqCst), 2);
    assert_eq!(f.renewals.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn valid_credential_is_reused_without_asking_the_host() {
    let f = fixture(false, false, false, false);
    f.provider.state.lock().await.credential = credential("new", hour());
    for _ in 0..3 {
        f.provider
            .complete(request(), CancellationToken::new())
            .await
            .unwrap();
    }
    assert_eq!(f.calls.load(Ordering::SeqCst), 3);
    assert_eq!(f.renewals.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn expired_before_next_model_call_is_renewed_before_dispatch() {
    let f = fixture(false, false, false, true);
    f.provider
        .complete(request(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.renewals.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn partial_stream_and_generic_auth_failure_are_never_replayed() {
    for (partial, generic) in [(true, false), (false, true)] {
        let f = fixture(partial, generic, false, false);
        let result: Vec<_> = f
            .provider
            .stream(request(), CancellationToken::new())
            .collect()
            .await;
        assert!(result.last().unwrap().is_err());
        assert_eq!(f.calls.load(Ordering::SeqCst), 1);
        assert_eq!(f.renewals.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn renewed_request_rejection_is_terminal_without_a_refresh_loop() {
    let f = fixture(false, false, true, false);
    assert!(
        f.provider
            .complete(request(), CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(f.calls.load(Ordering::SeqCst), 2);
    assert_eq!(f.renewals.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancelled_request_does_not_dispatch_or_refresh() {
    let f = fixture(false, false, false, true);
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        f.provider.complete(request(), cancel).await,
        Err(ModelError::Interrupted)
    );
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.renewals.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn shared_parent_and_child_requests_wait_on_one_renewal() {
    let f = fixture(false, false, false, false);
    let (parent, child) = (f.provider.clone(), f.provider.clone());
    let (a, b) = tokio::join!(
        parent.complete(request(), CancellationToken::new()),
        child.complete(request(), CancellationToken::new())
    );
    assert_eq!(a.unwrap().message.content, "fresh answer");
    assert_eq!(b.unwrap().message.content, "fresh answer");
    assert_eq!(f.renewals.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn proactive_renewal_spends_the_request_budget_without_a_second_force_refresh() {
    let f = fixture(false, false, true, true);
    assert!(
        f.provider
            .complete(request(), CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.renewals.load(Ordering::SeqCst), 1);
}

struct WaitingResolver {
    entered: Arc<tokio::sync::Notify>,
}
#[async_trait::async_trait]
impl ProviderCredentialResolver for WaitingResolver {
    async fn resolve(
        &self,
        _: u64,
        _: bool,
        _: CancellationToken,
    ) -> Result<ProviderCredentialUpdate, ModelError> {
        self.entered.notify_one();
        std::future::pending().await
    }
}

#[tokio::test]
async fn cancellation_during_renewal_never_replays_the_model_request() {
    let mut f = fixture(false, false, false, false);
    let entered = Arc::new(tokio::sync::Notify::new());
    f.provider.resolver = Arc::new(WaitingResolver {
        entered: entered.clone(),
    });
    let cancel = CancellationToken::new();
    let provider = f.provider.clone();
    let task_cancel = cancel.clone();
    let pending = tokio::spawn(async move { provider.complete(request(), task_cancel).await });
    entered.notified().await;
    cancel.cancel();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(1), pending)
            .await
            .unwrap()
            .unwrap(),
        Err(ModelError::Interrupted)
    );
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
}

struct WrongAccount;
#[async_trait::async_trait]
impl ProviderCredentialResolver for WrongAccount {
    async fn resolve(
        &self,
        generation: u64,
        _: bool,
        _: CancellationToken,
    ) -> Result<ProviderCredentialUpdate, ModelError> {
        let mut value = record(&credential("new", hour())).unwrap();
        value["subject"] = "different-account".into();
        Ok(ProviderCredentialUpdate {
            generation: generation + 1,
            credential: WorkerCredential(Some(value.to_string())),
        })
    }
}

#[tokio::test]
async fn renewed_account_identity_mismatch_fails_closed_without_dispatch() {
    let mut f = fixture(false, false, false, true);
    f.provider.resolver = Arc::new(WrongAccount);
    let error = f
        .provider
        .complete(request(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(error, ModelError::Authentication(_)));
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
}
