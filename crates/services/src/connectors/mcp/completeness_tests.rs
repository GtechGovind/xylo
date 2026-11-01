//! Loopback protocol fixtures for metadata, notifications, forms, and legacy SSE.
use super::*;
use crate::VaultKey;
use serde_json::json;
use tokio::io::AsyncWriteExt;

pub(super) fn broker(
    endpoint: String,
    transport: &str,
) -> (ConnectorBroker, ConnectorConfig, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("test.db")).unwrap();
    let vault = CredentialVault::unlocked(temp.path(), VaultKey::from_bytes([7; 32])).unwrap();
    let config = ConnectorConfig {
        id: Uuid::new_v4(),
        name: "fixture".into(),
        transport: transport.into(),
        endpoint,
        command: None,
        args: Vec::new(),
        credential_ref: None,
        trusted: false,
    };
    (ConnectorBroker::new(store, vault), config, temp)
}
pub(super) fn result(request: &Value, changed: bool) -> Value {
    let value = match request["method"].as_str().unwrap_or_default() {
        "initialize" => {
            json!({"protocolVersion":request["params"]["protocolVersion"],"serverInfo":{"name":"fixture","version":"1"},"capabilities":{"tools":{"listChanged":true},"resources":{},"prompts":{}}})
        }
        "tools/list" => {
            json!({"tools":[{"name":if changed {"changed"} else {"lookup"},"inputSchema":{"type":"object"},"annotations":{"readOnlyHint":true}}]})
        }
        "resources/list" => json!({"resources":[{"uri":"notes://one","name":"One"}]}),
        "resources/read" => {
            json!({"contents":[{"uri":request["params"]["uri"],"text":"External resource evidence"}]})
        }
        "prompts/list" => {
            json!({"prompts":[{"name":"review","description":"Review evidence","arguments":[{"name":"topic","required":true}]}]})
        }
        "prompts/get" => {
            json!({"messages":[{"role":"user","content":{"type":"text","text":"External prompt evidence"}}]})
        }
        _ => json!({"content":[{"type":"text","text":"done"}]}),
    };
    json!({"jsonrpc":"2.0","id":request["id"],"result":value})
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_metadata_and_list_changed_use_real_protocol_messages() {
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
                if generation >= 2 && request["method"] == "tools/list" {
                    response["result"]["tools"][0]["name"] = json!("latest");
                }
                let notify = request["method"] == "tools/call"
                    || (request["method"] == "tools/list" && generation == 1);
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
    let (broker, config, _temp) = broker(format!("http://{address}/mcp"), "http");
    assert_eq!(
        broker.tools(&config, &Default::default()).await.unwrap()[0].name,
        "lookup"
    );
    let prompts = broker.cached_prompts(&config).unwrap();
    assert_eq!(
        crate::mcp_prompt_commands(&config, &prompts)[0].command,
        "/mcp__fixture__review"
    );
    assert_eq!(
        broker
            .metadata(&config, "resources/list", json!({}), &Default::default())
            .await
            .unwrap()[0]["uri"],
        "notes://one"
    );
    let resource = broker
        .metadata(
            &config,
            "resources/read",
            json!({"uri":"notes://one"}),
            &Default::default(),
        )
        .await
        .unwrap();
    assert!(resource.to_string().contains("External resource evidence"));
    let prompt = broker
        .metadata(
            &config,
            "prompts/get",
            json!({"name":"review","arguments":{"topic":"test"}}),
            &Default::default(),
        )
        .await
        .unwrap();
    assert!(prompt.to_string().contains("External prompt evidence"));
    broker
        .call(&config, "lookup", json!({}), &Default::default())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !broker
            .cached_tools(&config)
            .is_ok_and(|tools| tools.first().is_some_and(|tool| tool.name == "latest"))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(broker.cached_tools(&config).unwrap()[0].name, "latest");
    broker.shutdown().await;
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_sse_initializes_and_reads_a_resource() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::mpsc::channel::<Value>(16);
    let rx = Arc::new(Mutex::new(Some(rx)));
    let server = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let tx = tx.clone();
            let rx = rx.clone();
            tokio::spawn(async move {
                let Some((head, body)) = super::tests::read_http_request(&mut socket).await else {
                    return;
                };
                if head.starts_with(b"GET") {
                    let Some(mut rx) = rx.lock().await.take() else {
                        return;
                    };
                    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\nevent: endpoint\ndata: /messages\n\n").await.unwrap();
                    while let Some(value) = rx.recv().await {
                        if socket
                            .write_all(format!("event: message\ndata: {value}\n\n").as_bytes())
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                } else {
                    let request: Value = serde_json::from_slice(&body).unwrap();
                    let _ = socket.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                    if request.get("id").is_some() {
                        let _ = tx.send(result(&request, false)).await;
                    }
                }
            });
        }
    });
    let (broker, config, _temp) = broker(format!("http://{address}/sse"), "sse");
    assert_eq!(
        broker.tools(&config, &Default::default()).await.unwrap()[0].name,
        "lookup"
    );
    assert!(
        broker
            .metadata(
                &config,
                "resources/read",
                json!({"uri":"notes://one"}),
                &Default::default()
            )
            .await
            .unwrap()
            .to_string()
            .contains("External resource evidence")
    );
    broker.shutdown().await;
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_form_can_be_declined_through_the_chat_route() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (responses, mut receiver) = tokio::sync::mpsc::channel::<Value>(4);
    let waiter = Arc::new(Mutex::new(None::<tokio::sync::oneshot::Sender<Value>>));
    let server = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let responses = responses.clone();
            let waiter = waiter.clone();
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
                if request["id"] == "form-1" {
                    if let Some(waiter) = waiter.lock().await.take() {
                        let _ = waiter.send(request.clone());
                    }
                    let _ = responses.send(request).await;
                    let _ = socket
                        .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n")
                        .await;
                    return;
                }
                if request["method"] == "tools/call" {
                    let (sender, answer) = tokio::sync::oneshot::channel();
                    *waiter.lock().await = Some(sender);
                    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").await.unwrap();
                    let form = json!({"jsonrpc":"2.0","id":"form-1","method":"elicitation/create","params":{"message":"Choose a topic","requestedSchema":{"type":"object","properties":{"topic":{"type":"string"}},"required":["topic"]}}});
                    socket
                        .write_all(format!("data: {form}\n\n").as_bytes())
                        .await
                        .unwrap();
                    let _ = answer.await;
                    let _ = socket
                        .write_all(format!("data: {}\n\n", result(&request, false)).as_bytes())
                        .await;
                } else if request.get("id").is_some() {
                    let body = result(&request, false).to_string();
                    let _ = socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await;
                } else {
                    let _ = socket
                        .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n")
                        .await;
                }
            });
        }
    });
    let (broker, config, _temp) = broker(format!("http://{address}/mcp"), "http");
    let (events, mut event_rx) = tokio::sync::mpsc::channel(8);
    let broker = broker.with_chat(events, Default::default());
    let execution = broker.clone();
    let target = config.clone();
    let call = tokio::spawn(async move {
        execution
            .call(&target, "lookup", json!({}), &Default::default())
            .await
    });
    let event = tokio::time::timeout(Duration::from_secs(3), event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let xylo_core::EngineEvent::McpElicitation { request } = event else {
        panic!("Expected a chat form")
    };
    assert_eq!(request.server_name, "fixture");
    assert!(
        broker
            .answer_form(
                request.id,
                xylo_core::McpFormAction::Accept,
                Some(json!({"topic":42}))
            )
            .is_err()
    );
    broker
        .answer_form(request.id, xylo_core::McpFormAction::Decline, None)
        .unwrap();
    assert_eq!(
        receiver.recv().await.unwrap()["result"]["action"],
        "decline"
    );
    call.await.unwrap().unwrap();
    assert!(
        broker
            .answer_form(request.id, xylo_core::McpFormAction::Decline, None)
            .is_err()
    );
    broker.shutdown().await;
    server.abort();
}
