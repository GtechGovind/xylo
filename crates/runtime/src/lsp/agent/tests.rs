#![cfg(unix)]

use super::*;
#[tokio::test]
async fn agent_requests_start_on_demand_and_report_current_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.py");
    std::fs::write(&path, "BAD_TYPE").unwrap();
    std::fs::write(dir.path().join("language-servers.json"),json!({"servers":[{"languages":["python"],"executable":"/usr/bin/python3","args":[concat!(env!("CARGO_MANIFEST_DIR"),"/src/lsp/agent/test_server.py")]}]}).to_string()).unwrap();
    let service = AgentService::new(dir.path().to_owned());
    assert!(service.manager.servers.lock().await.is_empty());
    let diagnostics = service
        .request(
            dir.path(),
            &path,
            "diagnostics",
            0,
            0,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        diagnostics["diagnostics"][0]["message"],
        "incompatible type"
    );
    for (operation, method) in [
        ("definition", "textDocument/definition"),
        ("references", "textDocument/references"),
        ("hover", "textDocument/hover"),
        ("symbols", "textDocument/documentSymbol"),
        ("workspace_symbols", "workspace/symbol"),
    ] {
        let response = service
            .request(dir.path(), &path, operation, 0, 1, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(response["method"], method);
    }
    std::fs::write(&path, "valid").unwrap();
    let clean = service
        .request(
            dir.path(),
            &path,
            "diagnostics",
            0,
            0,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(clean["diagnostics"], json!([]));
    let previous = service
        .manager
        .documents
        .lock()
        .await
        .get(&path.canonicalize().unwrap())
        .unwrap()
        .version;
    let other = dir.path().join("other.py");
    std::fs::write(&other, "valid").unwrap();
    service
        .request(
            dir.path(),
            &other,
            "diagnostics",
            0,
            0,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    service
        .request(
            dir.path(),
            &path,
            "diagnostics",
            0,
            0,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let documents = service.manager.documents.lock().await;
    assert_eq!(documents.len(), 1);
    assert!(
        documents
            .get(&path.canonicalize().unwrap())
            .unwrap()
            .version
            > previous
    );
    drop(documents);
    service.shutdown().await;
}
