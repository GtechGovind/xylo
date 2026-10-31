//! Actual permission/journal boundary with isolated credentials and loopback HTTP.
use super::super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use xylo_services::{ConnectorNotDispatched, CredentialVault, VaultKey};

const SECRET: &str = "offline-native-journal-token";

fn install(
    broker: &mut Arc<ToolBroker>,
    vault: CredentialVault,
    endpoint: String,
    reference: Option<&str>,
) -> Value {
    Arc::get_mut(broker).unwrap().native = NativeConnectors::new(vault).unwrap();
    let id = Uuid::new_v4();
    broker
        .store
        .save_connector(&ConnectorConfig {
            id,
            name: "Offline native journal fixture".into(),
            transport: "gmail".into(),
            endpoint,
            command: None,
            args: vec![],
            credential_ref: reference.map(str::to_owned),
        })
        .unwrap();
    json!({"connector_id":id,"tool":"gmail_send_draft","arguments":{"draft_id":"existing-draft"}})
}

async fn execute(
    broker: &Arc<ToolBroker>,
    incoming: &mut tokio::sync::mpsc::Receiver<EngineEvent>,
    key: &str,
    arguments: Value,
) -> (anyhow::Error, usize) {
    let (worker, key) = (broker.clone(), key.to_owned());
    let mut operation =
        tokio::task::spawn_blocking(move || worker.execute(&key, "connector_call", arguments));
    let mut approvals = 0;
    let deadline = tokio::time::sleep(Duration::from_secs(6));
    tokio::pin!(deadline);
    let result = loop {
        tokio::select! {
            result = &mut operation => break result.unwrap(),
            event = incoming.recv() => {
                let event = event.unwrap();
                if let EngineEvent::ApprovalRequired {action_id,..} = event {
                    approvals += 1;
                    broker.session.approvals().decide_with(action_id, true, false).unwrap();
                } else if let EngineEvent::ToolFinished {result,success,..} = event {
                    assert!(!success);
                    assert!(!result.contains(SECRET));
                }
            },
            _ = &mut deadline => {
                broker.cancel.cancel();
                panic!("Offline native connector fixture exceeded its bounded deadline")
            }
        }
    };
    // Result delivery can race the buffered terminal event. Inspect that too.
    while let Ok(event) = incoming.try_recv() {
        if let EngineEvent::ToolFinished {
            result, success, ..
        } = event
        {
            assert!(!success);
            assert!(!result.contains(SECRET));
        }
    }
    let error = result.unwrap_err();
    assert!(!format!("{error:#}").contains(SECRET));
    (error, approvals)
}

fn vault(directory: &std::path::Path) -> CredentialVault {
    CredentialVault::unlocked(directory, VaultKey::from_bytes([7; 32])).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_native_write_rejections_fail_the_journal_and_cached_retries_never_dispatch() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    for (index, secret, reference) in [
        (0, None, None),
        (1, None, Some("fixture")),
        (2, Some(format!("{SECRET}\r\ninvalid")), Some("fixture")),
        (
            3,
            Some(json!({"access_token":SECRET,"expires_at_ms":1}).to_string()),
            Some("fixture"),
        ),
        (4, Some(format!("{{{SECRET}")), Some("fixture")),
    ] {
        let (dir, mut broker, mut incoming) = super::fixture(PermissionMode::AskForApproval);
        let vault = vault(&dir.path().join("profile"));
        if let Some(secret) = secret {
            vault.set("fixture", &secret).unwrap();
        }
        let arguments = install(&mut broker, vault.clone(), endpoint.clone(), reference);
        let key = format!("rejected-{index}");
        let (error, approvals) = execute(&broker, &mut incoming, &key, arguments.clone()).await;
        assert!(error.downcast_ref::<ConnectorNotDispatched>().is_some());
        assert_eq!(
            approvals, 1,
            "The write still uses the actual permission boundary"
        );
        let action = broker.journaled(&key).unwrap().unwrap();
        assert_eq!(action.state, "failed");
        assert!(!action.error.as_ref().unwrap().contains(SECRET));
        assert!(
            broker
                .store
                .unresolved_actions(broker.request.session_id)
                .unwrap()
                .is_empty()
        );
        vault.set("fixture", SECRET).unwrap();
        let (_, approvals) = execute(&broker, &mut incoming, &key, arguments).await;
        assert_eq!(
            approvals, 0,
            "A failed identity cannot authorize a fresh attempt"
        );
        let cached = broker.journaled(&key).unwrap().unwrap();
        assert_eq!(cached.intent.id, action.intent.id);
        assert_eq!(cached.updated_at, action.updated_at);
        assert_eq!(cached.state, "failed");
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
}

#[derive(Clone, Copy)]
enum ResponseKind {
    ServerError,
    InvalidJson,
    CancelAfterSend,
    DropAfterSend,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sent_native_write_failures_are_unknown_and_cached_retries_never_send_again() {
    for (index, kind) in [
        ResponseKind::ServerError,
        ResponseKind::InvalidJson,
        ResponseKind::CancelAfterSend,
        ResponseKind::DropAfterSend,
    ]
    .into_iter()
    .enumerate()
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (dir, mut broker, mut incoming) = super::fixture(PermissionMode::AskForApproval);
        let vault = vault(&dir.path().join("profile"));
        vault.set("fixture", SECRET).unwrap();
        let arguments = install(&mut broker, vault, endpoint, Some("fixture"));
        let token = broker.cancel.clone();
        let sends = Arc::new(AtomicUsize::new(0));
        let seen = sends.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0; 4096];
                let header = loop {
                    let n = stream.read(&mut buffer).await.unwrap();
                    assert!(n > 0 && bytes.len() < 65536);
                    bytes.extend_from_slice(&buffer[..n]);
                    if let Some(index) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                        break index + 4;
                    }
                };
                let length = String::from_utf8_lossy(&bytes[..header])
                    .lines()
                    .find_map(|line| {
                        line.split_once(':')
                            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                            .map(|(_, v)| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap();
                assert!(length <= 65536);
                while bytes.len() < header + length {
                    let n = stream.read(&mut buffer).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buffer[..n]);
                }
                assert!(bytes.starts_with(b"POST /gmail/v1/users/me/drafts/send "));
                seen.fetch_add(1, Ordering::SeqCst);
                let (status, body) = match kind {
                    ResponseKind::ServerError => (500, format!("{{\"error\":\"{SECRET}\"}}")),
                    ResponseKind::InvalidJson => (200, "{invalid-json".into()),
                    ResponseKind::CancelAfterSend => {
                        token.cancel();
                        continue;
                    }
                    ResponseKind::DropAfterSend => continue,
                };
                let response = format!(
                    "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        let key = format!("sent-{index}");
        let (error, approvals) = execute(&broker, &mut incoming, &key, arguments.clone()).await;
        assert!(error.downcast_ref::<ConnectorNotDispatched>().is_none());
        assert_eq!(approvals, 1);
        let action = broker.journaled(&key).unwrap().unwrap();
        assert_eq!(action.state, "outcome_unknown");
        assert_eq!(sends.load(Ordering::SeqCst), 1);
        assert!(!action.error.unwrap().contains(SECRET));
        let (_, approvals) = execute(&broker, &mut incoming, &key, arguments).await;
        assert_eq!(approvals, 0);
        assert_eq!(
            broker.journaled(&key).unwrap().unwrap().intent.id,
            action.intent.id
        );
        assert_eq!(sends.load(Ordering::SeqCst), 1);
        server.abort();
    }
}
