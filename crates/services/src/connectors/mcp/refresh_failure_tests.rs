//! Failed catalog refreshes must revoke stale executable authority.
use super::completeness_tests::{broker, result};
use super::*;
use serde_json::json;
use tokio::io::AsyncWriteExt;
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_list_changed_fences_the_old_read_only_catalog() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let changed = Arc::new(AtomicU64::new(0));
    let revision = changed.clone();
    let server = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let changed = revision.clone();
            tokio::spawn(async move {
                let Some((head, body)) = super::tests::read_http_request(&mut socket).await else {
                    return;
                };
                if !head.starts_with(b"POST") {
                    let _ = socket
                        .write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n")
                        .await;
                    return;
                }
                let request: Value = serde_json::from_slice(&body).unwrap();
                if request.get("id").is_none() {
                    let _ = socket
                        .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n")
                        .await;
                    return;
                }
                let generation = changed.load(Ordering::Acquire);
                let mut response = result(&request, generation > 0);
                if generation == 1 && request["method"] == "tools/list" {
                    response = json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32603,"message":"refresh failed"}});
                }
                let notify = request["method"] == "tools/call";
                let (kind, body) = if notify {
                    changed.fetch_add(1, Ordering::AcqRel);
                    (
                        "text/event-stream",
                        format!(
                            "data: {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}}\n\ndata: {response}\n\n"
                        ),
                    )
                } else {
                    ("application/json", response.to_string())
                };
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(reply.as_bytes()).await;
            });
        }
    });
    let (broker, config, temp) = broker(format!("http://{address}/mcp"), "http");
    let (events, mut receiver) = tokio::sync::mpsc::channel(16);
    let broker = broker.with_chat(events, Default::default());
    assert_eq!(
        broker.tools(&config, &Default::default()).await.unwrap()[0].name,
        "lookup"
    );
    broker
        .call(&config, "lookup", json!({}), &Default::default())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(xylo_core::EngineEvent::Status { text }) = receiver.recv().await {
                assert!(text.contains("refresh failed"));
                assert!(text.len() < 256);
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        broker.cached_tools(&config).unwrap_or_default().is_empty(),
        "failed refresh must remove old authority"
    );
    assert!(
        broker
            .call(&config, "lookup", json!({}), &Default::default())
            .await
            .is_err()
    );
    let reopened = ConnectorBroker::new(
        Store::open(temp.path().join("test.db")).unwrap(),
        broker.pool.vault.clone(),
    );
    assert!(
        reopened.cached_tools(&config).is_err(),
        "restart must not resurrect revoked authority"
    );
    changed.store(2, Ordering::Release);
    assert_eq!(
        broker.tools(&config, &Default::default()).await.unwrap()[0].name,
        "changed"
    );
    assert_eq!(reopened.cached_tools(&config).unwrap()[0].name, "changed");
    broker.shutdown().await;
    server.abort();
}
