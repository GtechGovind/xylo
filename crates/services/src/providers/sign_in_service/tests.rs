use super::*;
use crate::VaultKey;
#[test]
fn callback_routes_accept_browser_and_proxy_forms_but_not_other_origins() {
    let redirect = "http://127.0.0.1:60649/auth/callback";
    let query = "code=fixture-code&scope=openid+email&state=fixture-state&client_id=oaiapp_fixture";
    for target in [
        format!("/auth/callback?{query}"),
        format!("{redirect}?{query}"),
    ] {
        assert_eq!(
            callback_request_url(&format!("GET {target} HTTP/1.1\r\n"), redirect),
            Some(format!("{redirect}?{query}"))
        );
    }
    for target in [
        "http://evil.example/auth/callback?code=x",
        "http://127.0.0.1:60650/auth/callback?code=x",
        "//evil.example/auth/callback?code=x",
        "/favicon.ico",
        "/auth/callback?code=x#fragment",
        "/callback?code=x",
    ] {
        assert!(callback_request_url(&format!("GET {target} HTTP/1.1"), redirect).is_none());
    }
    assert!(callback_request_url("POST /auth/callback?code=x HTTP/1.1", redirect).is_none());
}

#[tokio::test]
async fn browser_callback_reaches_exchange_after_unrelated_requests() {
    for absolute in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let redirect = format!("http://{address}/auth/callback");
        let pending = PendingChatGptSignIn::begin(
            &format!("urn:uuid:{}", uuid::Uuid::new_v4()),
            &redirect,
            None,
        )
        .unwrap();
        let authorization = url::Url::parse(&pending.authorization_url).unwrap();
        let state = authorization
            .query_pairs()
            .find(|(key, _)| key == "state")
            .unwrap()
            .1
            .into_owned();
        let query =
            format!("code=fixture-code&scope=openid+email&state={state}&client_id=oaiapp_fixture");
        let expected = format!("{redirect}?{query}");
        let task = tokio::spawn(async move {
            receive_callback(&listener, &pending, &redirect, &CancellationToken::new()).await
        });
        let mut noise = tokio::net::TcpStream::connect(address).await.unwrap();
        noise
            .write_all(b"GET /favicon.ico HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .unwrap();
        let mut response = [0; 512];
        assert!(noise.read(&mut response).await.unwrap() > 0);
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        let target = if absolute {
            expected.clone()
        } else {
            format!("/auth/callback?{query}")
        };
        socket
            .write_all(format!("GET {target} HTTP/1.1\r\nHost: {address}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let (_, received) = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(received, expected);
    }
}

#[tokio::test]
async fn idle_or_closed_preconnects_do_not_block_or_end_sign_in() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let redirect = format!("http://{address}/auth/callback");
    let pending = PendingChatGptSignIn::begin(
        &format!("urn:uuid:{}", uuid::Uuid::new_v4()),
        &redirect,
        None,
    )
    .unwrap();
    let state = url::Url::parse(&pending.authorization_url)
        .unwrap()
        .query_pairs()
        .find(|(key, _)| key == "state")
        .unwrap()
        .1
        .into_owned();
    let task = tokio::spawn(async move {
        receive_callback(&listener, &pending, &redirect, &CancellationToken::new()).await
    });
    // Browser speculative connections: one stays open silently, one closes.
    let _idle = tokio::net::TcpStream::connect(address).await.unwrap();
    drop(tokio::net::TcpStream::connect(address).await.unwrap());
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
    socket
        .write_all(
            format!("GET /auth/callback?code=fixture-code&scope=openid+email&state={state}&client_id=oaiapp_fixture HTTP/1.1\r\nHost: {address}\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    // Well before the idle connection's 10 s read deadline.
    let (_, received) = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("an idle connection must not delay the real callback")
        .unwrap()
        .unwrap();
    assert!(received.contains("code=fixture-code"));
}

#[tokio::test]
async fn stale_or_duplicate_state_denials_do_not_abort_the_pending_sign_in() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let redirect = format!(
        "http://127.0.0.1:{}/auth/callback",
        listener.local_addr().unwrap().port()
    );
    let pending = PendingChatGptSignIn::begin(
        &format!("urn:uuid:{}", uuid::Uuid::new_v4()),
        &redirect,
        None,
    )
    .unwrap();
    let authorization = url::Url::parse(&pending.authorization_url).unwrap();
    let state = authorization
        .query_pairs()
        .find(|(key, _)| key == "state")
        .unwrap()
        .1
        .into_owned();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        receive_callback(&listener, &pending, &redirect, &CancellationToken::new()).await
    });
    for query in [
        "state=wrong&error=access_denied".into(),
        format!("state=wrong&state={state}&error=access_denied"),
    ] {
        let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        socket
            .write_all(
                format!("GET /auth/callback?{query} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 400"));
        assert!(!task.is_finished());
    }
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    socket.write_all(format!("GET /auth/callback?state={state}&error=access_denied HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes()).await.unwrap();
    let error = task.await.unwrap().err().unwrap();
    assert!(error.to_string().contains("declined"));
}
#[tokio::test]
async fn native_sign_in_binds_callback_before_authorizing_and_cancellation_closes_it() {
    let directory = tempfile::tempdir().unwrap();
    let service = ChatGptSignInService::new(directory.path()).unwrap();
    let vault =
        CredentialVault::unlocked(directory.path(), VaultKey::from_bytes([0x42; 32])).unwrap();
    let cancel = CancellationToken::new();
    let run_cancel = cancel.clone();
    let (send, receive) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        service
            .sign_in(
                &format!("urn:uuid:{}", uuid::Uuid::new_v4()),
                vault,
                "fixture-registration",
                run_cancel,
                |url| async move {
                    send.send(url).unwrap();
                    Ok(())
                },
            )
            .await
    });
    let authorization = url::Url::parse(&receive.await.unwrap()).unwrap();
    let redirect = authorization
        .query_pairs()
        .find(|(name, _)| name == "redirect_uri")
        .unwrap()
        .1
        .into_owned();
    let callback = url::Url::parse(&redirect).unwrap();
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", callback.port().unwrap()))
        .await
        .unwrap();
    socket
        .write_all(b"GET /auth/callback?state=wrong&code=fake HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .await
        .unwrap();
    let mut reply = String::new();
    tokio::time::timeout(Duration::from_secs(2), socket.read_to_string(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert!(reply.starts_with("HTTP/1.1 400"));
    cancel.cancel();
    assert!(task.await.unwrap().is_err());
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", callback.port().unwrap()))
            .await
            .is_err()
    );
}
#[tokio::test]
async fn rotating_token_reference_lease_is_exclusive_and_cancellable() {
    let directory = tempfile::tempdir().unwrap();
    let first = ChatGptSignInService::new(directory.path()).unwrap();
    let second = ChatGptSignInService::new(directory.path()).unwrap();
    let lease = first
        .lease("one-account", &CancellationToken::new())
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    let waiting_cancel = cancel.clone();
    let waiting = tokio::spawn(async move { second.lease("one-account", &waiting_cancel).await });
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());
    cancel.cancel();
    assert!(waiting.await.unwrap().is_err());
    drop(lease);
    assert!(
        first
            .lease("one-account", &CancellationToken::new())
            .await
            .is_ok()
    );
}
