//! Notification refresh failure must revoke broker schema and read-only authority.
use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_refresh_revokes_broker_read_only_authority() {
    let (endpoint, server) = failed_server().await;
    let (_directory, broker, mut events) = fixture(PermissionMode::ApproveForMe);
    let config = ConnectorConfig {
        id: Uuid::new_v4(),
        name: "Trusted fixture".into(),
        transport: "http".into(),
        endpoint,
        command: None,
        args: Vec::new(),
        credential_ref: None,
        trusted: true,
    };
    broker.store.save_connector(&config).unwrap();
    bounded_run(
        &broker,
        "discover",
        "connector_tools",
        json!({"connector_id":config.id}),
    )
    .await
    .unwrap();
    let args = json!({"connector_id":config.id,"tool":"fixture_tool","arguments":{}});
    assert!(broker.connector_call_reads(&args));
    assert!(
        !broker
            .effective_definition("connector_call", &args)
            .unwrap()
            .writes
    );
    bounded_run(&broker, "first-read", "connector_call", args.clone())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let EngineEvent::Status { text } = events.recv().await.unwrap()
                && text.contains("refresh failed")
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        !broker.connector_call_reads(&args),
        "stale snapshot must not grant parallel read authority"
    );
    assert!(
        broker
            .effective_definition("connector_call", &args)
            .is_err()
    );
    let error = bounded_run(&broker, "stale-read", "connector_call", args)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unavailable"), "{error:#}");
    broker.shutdown().await.unwrap();
    server.abort();
}

async fn bounded_run(
    broker: &Arc<ToolBroker>,
    key: &str,
    name: &str,
    args: Value,
) -> Result<String> {
    match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        super::run(broker, key, name, args),
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            broker.cancel.cancel();
            Err(error.into())
        }
    }
}

/// A loopback server proves project approval reaches a real metadata response.
async fn failed_server() -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
    let changed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let task = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let changed = changed.clone();
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                let mut chunk = [0; 4096];
                let request = loop {
                    let read = socket.read(&mut chunk).await.unwrap();
                    if read == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&chunk[..read]);
                    assert!(bytes.len() <= 65536);
                    if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&bytes[..end]);
                        if !head.starts_with("POST ") {
                            socket
                                .write_all(
                                    b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n",
                                )
                                .await
                                .unwrap();
                            return;
                        }
                        let length = head
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|n| n.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            break serde_json::from_slice::<Value>(
                                &bytes[end + 4..end + 4 + length],
                            )
                            .unwrap();
                        }
                    }
                };
                if request.get("id").is_none() {
                    socket
                        .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n")
                        .await
                        .unwrap();
                    return;
                }
                let result = match request["method"].as_str().unwrap() {
                    "initialize" => {
                        json!({"protocolVersion":request["params"]["protocolVersion"],"serverInfo":{"name":"project-fixture","version":"1"},"capabilities":{"tools":{"listChanged":true}}})
                    }
                    "tools/list" => {
                        json!({"tools":[{"name":"fixture_tool","inputSchema":{"type":"object"},"annotations":{"readOnlyHint":true}}]})
                    }
                    "tools/call" => {
                        json!({"content":[{"type":"text","text":"previous read result"}]})
                    }
                    method => panic!("Unexpected method {method}"),
                };
                let response = if request["method"] == "tools/list"
                    && changed.load(std::sync::atomic::Ordering::Acquire)
                {
                    json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32603,"message":"changed catalog unavailable"}})
                } else {
                    json!({"jsonrpc":"2.0","id":request["id"],"result":result})
                };
                let (kind, body) = if request["method"] == "tools/call" {
                    changed.store(true, std::sync::atomic::Ordering::Release);
                    (
                        "text/event-stream",
                        format!(
                            "data: {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}}\n\ndata: {response}\n\n"
                        ),
                    )
                } else {
                    ("application/json", response.to_string())
                };
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            });
        }
    });
    (endpoint, task)
}
