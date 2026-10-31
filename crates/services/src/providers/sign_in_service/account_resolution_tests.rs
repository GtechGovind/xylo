use super::*;
use crate::VaultKey;
use serde_json::json;

// These fixtures represent an already validated registration in an isolated
// vault. Signature/nonce/audience verification is tested in chatgpt.rs.
fn fixture(
    scopes: &str,
    expires: i64,
) -> (
    tempfile::TempDir,
    ChatGptSignInService,
    CredentialVault,
    ProviderConfig,
) {
    let directory = tempfile::tempdir().unwrap();
    let service = ChatGptSignInService::new(directory.path()).unwrap();
    let vault =
        CredentialVault::unlocked(directory.path(), VaultKey::from_bytes([0x42; 32])).unwrap();
    let record = json!({"type":"chatgpt","client_id":"oaiapp_fixture","subject":"fixture-account","issuer":"https://auth.openai.com","email":"fixture@example.invalid","ext_agent_host_id":"urn:uuid:00000000-0000-4000-8000-000000000000","id_token":"fixture-validated-id","access_token":"fixture-access","refresh_token":null,"expires_at_ms":expires,"scopes":scopes.split_whitespace().collect::<Vec<_>>()});
    vault.set("selected-account", &record.to_string()).unwrap();
    let config = ProviderConfig {
        family: ApiFamily::OpenAiResponses,
        endpoint: "https://api.openai.com/v1".into(),
        credential_ref: Some("selected-account".into()),
        ..Default::default()
    };
    (directory, service, vault, config)
}

fn rejected_digest() -> String {
    hex::encode(Sha256::digest(b"fixture-access"))
}
fn allow_renewal(vault: &CredentialVault) {
    let mut credential: ChatGptCredential =
        serde_json::from_str(&vault.get("selected-account").unwrap().unwrap()).unwrap();
    credential.refresh_token = Some("fixture-refresh".into());
    vault
        .set(
            "selected-account",
            &serde_json::to_string(&credential).unwrap(),
        )
        .unwrap();
}
fn renewed(mut credential: ChatGptCredential) -> ChatGptCredential {
    credential.access_token = "fixture-renewed-access".into();
    credential.refresh_token = Some("fixture-rotated-refresh".into());
    credential.expires_at_ms = now_ms() + 3_600_000;
    credential
}
#[tokio::test]
async fn rejected_future_dated_token_forces_one_refresh_and_persists_rotation() {
    let (_directory, service, vault, config) =
        fixture("chatgpt.tokens.use.direct", now_ms() + 3_600_000);
    allow_renewal(&vault);
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let record = service
        .refresh_rejected_with(
            &config,
            vault.clone(),
            &rejected_digest(),
            &CancellationToken::new(),
            |credential, _| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move { Ok(renewed(credential)) }
            },
        )
        .await
        .unwrap();
    let credential: ChatGptCredential = serde_json::from_str(&record).unwrap();
    assert_eq!(
        credential.access_token, "fixture-renewed-access",
        "A rejected token must not be reused just because local expiry is in the future"
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(vault.get("selected-account").unwrap().unwrap(), record);
    assert_eq!(
        credential.refresh_token.as_deref(),
        Some("fixture-rotated-refresh")
    );
}

#[tokio::test]
async fn selected_authorized_registration_resolves_the_same_bearer_for_catalog_and_inference() {
    let (_directory, service, vault, config) = fixture(
        "openid profile email chatgpt.tokens.use.direct",
        now_ms() + 3_600_000,
    );
    let original = vault.get("selected-account").unwrap().unwrap();
    service
        .prepare_provider_credential(&config, vault.clone(), &CancellationToken::new())
        .await
        .unwrap();
    let resolved = service
        .resolve_provider_credential(&config, vault.clone(), &CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resolved, original);
    let parsed = ProviderCredential::parse(Some(resolved)).unwrap();
    assert!(matches!(parsed, ProviderCredential::ChatGpt { .. }));
    assert_eq!(
        parsed.headers(&config.family, &config.headers).unwrap()["authorization"],
        "Bearer fixture-access"
    );
}

#[tokio::test]
async fn identity_only_account_remains_visible_but_cannot_authorize_inference() {
    let (_directory, service, vault, config) =
        fixture("openid profile email", now_ms() + 3_600_000);
    let account = service
        .account(vault.clone(), "selected-account")
        .await
        .unwrap()
        .unwrap();
    assert!(account.connected);
    assert!(!account.plan_usage_authorized);
    let error = service
        .resolve_provider_credential(&config, vault.clone(), &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("plan usage is not authorized"));
    assert!(vault.get("selected-account").unwrap().is_some());
}

#[tokio::test]
async fn expired_nonrenewable_registration_fails_before_any_inference_and_explains_reconnect() {
    let (_directory, service, vault, config) = fixture("chatgpt.tokens.use.direct", 0);
    let error = service
        .resolve_provider_credential(&config, vault, &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Reconnect ChatGPT"));
    assert!(!error.to_string().contains("fixture-access"));
}

#[tokio::test]
async fn account_registration_never_resolves_for_a_custom_inference_origin_or_chat_protocol() {
    let (_directory, service, vault, mut config) =
        fixture("chatgpt.tokens.use.direct", now_ms() + 3_600_000);
    config.endpoint = "https://custom.example/v1".into();
    assert!(
        service
            .resolve_provider_credential(&config, vault.clone(), &CancellationToken::new())
            .await
            .unwrap_err()
            .to_string()
            .contains("official public Responses API")
    );
    config.endpoint = "https://api.openai.com/v1".into();
    config.family = ApiFamily::OpenAiChat;
    assert!(
        service
            .resolve_provider_credential(&config, vault, &CancellationToken::new())
            .await
            .is_err()
    );
}
#[tokio::test]
async fn parallel_rejections_of_old_token_share_one_rotating_refresh() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (directory, service, vault, config) =
        fixture("chatgpt.tokens.use.direct", now_ms() + 3_600_000);
    allow_renewal(&vault);
    let second_service = ChatGptSignInService::new(directory.path()).unwrap();
    let calls = AtomicUsize::new(0);
    let digest = rejected_digest();
    let cancel = CancellationToken::new();
    let refresh = |credential, _| {
        calls.fetch_add(1, Ordering::SeqCst);
        async move {
            tokio::time::sleep(Duration::from_millis(25)).await;
            Ok(renewed(credential))
        }
    };
    let (first, second) = tokio::join!(
        service.refresh_rejected_with(&config, vault.clone(), &digest, &cancel, refresh),
        second_service.refresh_rejected_with(&config, vault.clone(), &digest, &cancel, refresh)
    );
    assert_eq!(first.unwrap(), second.unwrap());
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "A waiting sibling must reread the rotated record under the lease"
    );
}
#[tokio::test]
async fn cancelled_rejection_refresh_does_not_write_or_return_credentials() {
    let (_directory, service, vault, config) =
        fixture("chatgpt.tokens.use.direct", now_ms() + 3_600_000);
    allow_renewal(&vault);
    let before = vault.get("selected-account").unwrap();
    let cancel = CancellationToken::new();
    let digest = rejected_digest();
    let entered = std::sync::Arc::new(tokio::sync::Notify::new());
    let signal = entered.clone();
    let refresh = service.refresh_rejected_with(
        &config,
        vault.clone(),
        &digest,
        &cancel,
        move |_, _| async move {
            signal.notify_one();
            std::future::pending::<Result<ChatGptCredential>>().await
        },
    );
    let cancelling = async {
        entered.notified().await;
        cancel.cancel();
    };
    let (result, ()) = tokio::join!(refresh, cancelling);
    assert!(result.unwrap_err().to_string().contains("cancelled"));
    assert_eq!(vault.get("selected-account").unwrap(), before);
}
#[tokio::test]
async fn nonrenewable_and_denied_registrations_never_refresh() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    for scopes in ["chatgpt.tokens.use.direct", "openid profile"] {
        let (_directory, service, vault, config) = fixture(scopes, now_ms() + 3_600_000);
        let calls = AtomicUsize::new(0);
        let result = service
            .refresh_rejected_with(
                &config,
                vault.clone(),
                &rejected_digest(),
                &CancellationToken::new(),
                |credential, _| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    async move { Ok(renewed(credential)) }
                },
            )
            .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(
            !vault
                .get("selected-account")
                .unwrap()
                .unwrap()
                .contains("fixture-renewed-access")
        );
    }
}
#[tokio::test]
async fn renewed_denied_grant_is_persisted_but_never_returned_as_authorized() {
    let (_directory, service, vault, config) =
        fixture("chatgpt.tokens.use.direct", now_ms() + 3_600_000);
    allow_renewal(&vault);
    let result = service
        .refresh_rejected_with(
            &config,
            vault.clone(),
            &rejected_digest(),
            &CancellationToken::new(),
            |credential, _| async move {
                let mut credential = renewed(credential);
                credential.scopes.clear();
                Ok(credential)
            },
        )
        .await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("plan usage is not authorized")
    );
    let saved: ChatGptCredential =
        serde_json::from_str(&vault.get("selected-account").unwrap().unwrap()).unwrap();
    assert!(!saved.plan_usage_authorized());
    assert_eq!(saved.access_token, "fixture-renewed-access");
    assert_eq!(
        saved.refresh_token.as_deref(),
        Some("fixture-rotated-refresh")
    );
}
#[tokio::test]
async fn changed_account_identity_or_custom_endpoint_cannot_replace_registration() {
    let (_directory, service, vault, mut config) =
        fixture("chatgpt.tokens.use.direct", now_ms() + 3_600_000);
    allow_renewal(&vault);
    let before = vault.get("selected-account").unwrap();
    let result = service
        .refresh_rejected_with(
            &config,
            vault.clone(),
            &rejected_digest(),
            &CancellationToken::new(),
            |credential, _| async move {
                let mut credential = renewed(credential);
                credential.subject = "different-account".into();
                Ok(credential)
            },
        )
        .await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("different account")
    );
    assert_eq!(vault.get("selected-account").unwrap(), before);
    config.endpoint = "https://custom.example/v1".into();
    assert!(
        service
            .refresh_rejected_provider_credential(
                &config,
                vault.clone(),
                &rejected_digest(),
                &CancellationToken::new()
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("official public Responses API")
    );
    assert_eq!(vault.get("selected-account").unwrap(), before);
}
#[tokio::test]
async fn waiting_rejection_does_not_resurrect_signed_out_registration() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (_directory, service, vault, config) =
        fixture("chatgpt.tokens.use.direct", now_ms() + 3_600_000);
    allow_renewal(&vault);
    let held = service
        .lease("selected-account", &CancellationToken::new())
        .await
        .unwrap();
    let calls = AtomicUsize::new(0);
    let digest = rejected_digest();
    let cancel = CancellationToken::new();
    let refresh =
        service.refresh_rejected_with(&config, vault.clone(), &digest, &cancel, |credential, _| {
            calls.fetch_add(1, Ordering::SeqCst);
            async move { Ok(renewed(credential)) }
        });
    tokio::pin!(refresh);
    tokio::select! {result=&mut refresh=>panic!("Refresh must wait for sign-out lease: {:?}",result.is_ok()),_=tokio::time::sleep(Duration::from_millis(20))=>{}}
    let mut registration: ChatGptCredential =
        serde_json::from_str(&vault.get("selected-account").unwrap().unwrap()).unwrap();
    registration.access_token.clear();
    registration.id_token.clear();
    registration.refresh_token = None;
    registration.scopes.clear();
    registration.expires_at_ms = 0;
    let signed_out = serde_json::to_string(&registration).unwrap();
    vault.set("selected-account", &signed_out).unwrap();
    drop(held);
    assert!(
        refresh
            .await
            .unwrap_err()
            .to_string()
            .contains("signed out")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(vault.get("selected-account").unwrap().unwrap(), signed_out);
}

#[tokio::test]
async fn cancellation_after_known_rotation_commits_tokens_without_returning_them() {
    let (_directory, service, vault, config) =
        fixture("chatgpt.tokens.use.direct", now_ms() + 3_600_000);
    allow_renewal(&vault);
    let cancel = CancellationToken::new();
    let cancelled = cancel.clone();
    let result = service
        .refresh_rejected_with(
            &config,
            vault.clone(),
            &rejected_digest(),
            &cancel,
            move |credential, _| async move {
                cancelled.cancel();
                Ok(renewed(credential))
            },
        )
        .await;
    assert!(result.unwrap_err().to_string().contains("cancelled"));
    let saved: ChatGptCredential =
        serde_json::from_str(&vault.get("selected-account").unwrap().unwrap()).unwrap();
    assert_eq!(
        saved.refresh_token.as_deref(),
        Some("fixture-rotated-refresh"),
        "A known OAuth rotation must not be discarded when cancellation arrives during its durable commit"
    );
    assert_eq!(saved.access_token, "fixture-renewed-access");
}
#[tokio::test]
async fn dropped_refresh_waiter_keeps_lease_until_blocking_commit_finishes() {
    let (directory, service, vault, _config) =
        fixture("chatgpt.tokens.use.direct", now_ms() + 3_600_000);
    let lease = service
        .lease("selected-account", &CancellationToken::new())
        .await
        .unwrap();
    let waiting_service = ChatGptSignInService::new(directory.path()).unwrap();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (finish_tx, finish_rx) = std::sync::mpsc::channel();
    let writing = vault.clone();
    let commit = ChatGptSignInService::commit_under_lease(lease, move || {
        let _ = entered_tx.send(());
        finish_rx.recv().unwrap();
        writing.set("selected-account", "committed-rotation")
    });
    entered_rx.await.unwrap();
    drop(commit); // Models outer host timeout dropping its join future.
    let cancel = CancellationToken::new();
    let waiting = waiting_service.lease("selected-account", &cancel);
    tokio::pin!(waiting);
    tokio::select! { result=&mut waiting=>panic!("A dropped caller released the file lease before the credential write finished: {:?}",result.is_ok()),_=tokio::time::sleep(Duration::from_millis(50))=>{} }
    finish_tx.send(()).unwrap();
    let _following = tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        vault.get("selected-account").unwrap().as_deref(),
        Some("committed-rotation")
    );
}
