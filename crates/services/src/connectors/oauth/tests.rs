use super::*;
use crate::VaultKey;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Token endpoint that counts exchanges and answers slowly, so concurrent
/// refreshes overlap and a missing single-flight gate would show up as a
/// second exchange.
async fn token_endpoint(exchanges: Arc<AtomicUsize>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let exchanges = exchanges.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    let read = socket.read(&mut buffer).await.unwrap();
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&request);
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length = text[..end]
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|value| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                let count = exchanges.fetch_add(1, Ordering::SeqCst) + 1;
                tokio::time::sleep(Duration::from_millis(200)).await;
                let body = format!(
                    r#"{{"access_token":"access-{count}","refresh_token":"refresh-{count}","token_type":"Bearer","expires_in":3600}}"#
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            });
        }
    });
    (format!("http://{address}/token"), server)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_refreshes_of_one_credential_exchange_once() {
    let exchanges = Arc::new(AtomicUsize::new(0));
    let (endpoint, server) = token_endpoint(exchanges.clone()).await;
    let directory = tempfile::tempdir().unwrap();
    let vault = CredentialVault::unlocked(directory.path(), VaultKey::from_bytes([3; 32])).unwrap();
    let reference = "oauth-single-flight";
    let initial = OAuthTokens {
        access_token: "access-0".into(),
        refresh_token: Some("refresh-0".into()),
        expires_at_ms: Some(0),
        scope: None,
    };
    vault
        .set(reference, &serde_json::to_string(&initial).unwrap())
        .unwrap();
    let config = OAuthConfig {
        authorization_endpoint: endpoint.clone(),
        token_endpoint: endpoint,
        client_id: "fixture".into(),
        scopes: Vec::new(),
    };
    let refreshes = (0..4).map(|_| {
        let config = config.clone();
        let vault = vault.clone();
        tokio::spawn(async move {
            refresh_oauth(&config, &vault, reference, &CancellationToken::new()).await
        })
    });
    for refresh in futures_util::future::join_all(refreshes).await {
        refresh.unwrap().unwrap();
    }
    assert_eq!(exchanges.load(Ordering::SeqCst), 1);
    let stored: OAuthTokens =
        serde_json::from_str(&vault.get(reference).unwrap().unwrap()).unwrap();
    assert_eq!(stored.access_token, "access-1");
    assert_eq!(stored.refresh_token.as_deref(), Some("refresh-1"));

    // A later, non-overlapping refresh is a real request and uses the rotated token.
    refresh_oauth(&config, &vault, reference, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(exchanges.load(Ordering::SeqCst), 2);
    server.abort();
}

#[test]
fn state_comparison_requires_an_exact_match() {
    assert!(constant_time_eq(b"state-value", b"state-value"));
    assert!(!constant_time_eq(b"state-value", b"state-valuf"));
    assert!(!constant_time_eq(b"state-value", b"state-valu"));
    assert!(!constant_time_eq(b"", b"x"));
    assert!(constant_time_eq(b"", b""));
}

#[tokio::test]
async fn completing_with_a_wrong_state_is_rejected_before_any_exchange() {
    let config = OAuthConfig {
        authorization_endpoint: "https://auth.example.com/authorize".into(),
        token_endpoint: "https://auth.example.com/token".into(),
        client_id: "fixture".into(),
        scopes: vec!["read".into()],
    };
    let flow = PendingOAuth::begin(config, "http://127.0.0.1:45678/callback").unwrap();
    let mut wrong = flow.state.clone();
    let last = wrong.pop().unwrap();
    wrong.push(if last == 'A' { 'B' } else { 'A' });
    let error = flow
        .complete("code", &wrong, &CancellationToken::new())
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("state mismatch"));
}
