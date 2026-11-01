//! Project discovery must not borrow authority from ordinary tool grants.
use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_mcp_first_start_requires_approval_even_in_full_access() {
    let (endpoint, server) = project_server().await;
    let (_directory, broker, mut events) = fixture_with(PermissionMode::FullAccess, |request| {
        std::fs::write(
            request.workspace.join(".mcp.json"),
            json!({"mcpServers":{"remote":{"type":"http","url":endpoint}}}).to_string(),
        )
        .unwrap();
    });
    let entry = xylo_services::project_connectors(&broker.discovery_root())
        .unwrap()
        .remove(0);
    let execution = broker.clone();
    let id = entry.config.id;
    let run = tokio::task::spawn_blocking(move || {
        execution.execute(
            "denied-project",
            "connector_tools",
            json!({"connector_id":id}),
        )
    });
    loop {
        if let EngineEvent::ApprovalRequired {
            action_id,
            reason,
            arguments,
            ..
        } = events.recv().await.unwrap()
        {
            assert!(reason.unwrap().contains(".mcp.json"));
            assert_eq!(arguments["server"], "remote");
            broker
                .session
                .approvals()
                .decide_with(action_id, false, false)
                .unwrap();
            break;
        }
    }
    assert!(run.await.unwrap().is_err());
    assert!(!broker.store.project_mcp_approved(&entry).unwrap());
    let execution = broker.clone();
    let run = tokio::task::spawn_blocking(move || {
        execution.execute(
            "approved-project",
            "connector_tools",
            json!({"connector_id":id}),
        )
    });
    loop {
        if let EngineEvent::ApprovalRequired { action_id, .. } = events.recv().await.unwrap() {
            broker
                .session
                .approvals()
                .decide_with(action_id, true, false)
                .unwrap();
            break;
        }
    }
    assert!(
        run.await
            .unwrap()
            .unwrap()
            .to_string()
            .contains("fixture_tool")
    );
    assert!(broker.store.project_mcp_approved(&entry).unwrap());
    let execution = broker.clone();
    let resource = tokio::task::spawn_blocking(move || {
        execution.execute(
            "already-approved",
            "connector_resource_read",
            json!({"connector_id":id,"uri":"fixture://note"}),
        )
    })
    .await
    .unwrap()
    .unwrap();
    assert!(resource.to_string().contains("Project resource evidence"));
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(event, EngineEvent::ApprovalRequired { .. }),
            "Consent must not be asked again for unchanged content"
        );
    }
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lazy_rules_enter_the_next_model_boundary_once() {
    let (_directory, broker, _events) = fixture(PermissionMode::AskForApproval);
    let root = broker.discovery_root();
    std::fs::create_dir_all(root.join(".xylo/rules")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(
        root.join(".xylo/rules/rust.md"),
        "---\npaths:\n  - src/**/*.rs\n---\nScoped safety rule",
    )
    .unwrap();
    let execution = broker.clone();
    tokio::task::spawn_blocking(move || {
        execution.execute("read-rust", "file_read", json!({"path":"src/main.rs"}))
    })
    .await
    .unwrap()
    .unwrap();
    assert!(
        broker
            .take_instruction_notes()
            .join("\n")
            .contains("Scoped safety rule")
    );
    assert!(broker.take_instruction_notes().is_empty());
}

#[tokio::test]
async fn mcp_mentions_map_to_deferred_permission_gated_resource_reads() {
    let (_directory, broker, _) = fixture_with(PermissionMode::AskForApproval, |request| {
        request.prompt = "Summarize @docs:notes://release".into();
        std::fs::write(
            request.workspace.join(".mcp.json"),
            r#"{"mcpServers":{"docs":{"url":"https://example.com/mcp"}}}"#,
        )
        .unwrap();
    });
    let context = broker.mcp_invocations().unwrap();
    assert!(context.contains("connector_resource_read"));
    assert!(context.contains("notes://release"));
    assert!(context.contains("untrusted"));
    assert!(
        !broker
            .initial_definitions()
            .iter()
            .any(|tool| tool.name == "connector_resource_read")
    );
    assert!(
        broker
            .definitions()
            .iter()
            .any(|tool| tool.name == "connector_resource_read")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lazy_rules_follow_files_discovered_by_search() {
    let (_directory, broker, _events) = fixture(PermissionMode::AskForApproval);
    let root = broker.discovery_root();
    std::fs::create_dir_all(root.join(".xylo/rules")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/main.rs"), "fn searchable_marker() {}\n").unwrap();
    std::fs::write(
        root.join(".xylo/rules/rust.md"),
        "---\npaths: [\"src/**/*.rs\"]\n---\nSearch-scoped rule",
    )
    .unwrap();
    let execution = broker.clone();
    tokio::task::spawn_blocking(move || {
        execution.execute(
            "search-rust",
            "search_project",
            json!({"query":"searchable_marker"}),
        )
    })
    .await
    .unwrap()
    .unwrap();
    assert!(
        broker
            .take_instruction_notes()
            .join("\n")
            .contains("Search-scoped rule")
    );
}

/// A loopback server proves project approval reaches a real metadata response.
async fn project_server() -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
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
                        json!({"protocolVersion":request["params"]["protocolVersion"],"serverInfo":{"name":"project-fixture","version":"1"},"capabilities":{"tools":{},"resources":{}}})
                    }
                    "tools/list" => {
                        json!({"tools":[{"name":"fixture_tool","inputSchema":{"type":"object"}}]})
                    }
                    "resources/read" => {
                        json!({"contents":[{"uri":"fixture://note","text":"Project resource evidence"}]})
                    }
                    method => panic!("Unexpected method {method}"),
                };
                let body = json!({"jsonrpc":"2.0","id":request["id"],"result":result}).to_string();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            });
        }
    });
    (endpoint, task)
}
