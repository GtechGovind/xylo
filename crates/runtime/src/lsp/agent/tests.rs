#![cfg(unix)]

use super::*;

mod protocol_fixture {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/test_support.rs"));
}

#[tokio::test]
async fn agent_requests_start_on_demand_and_report_current_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.py");
    std::fs::write(&path, "BAD_TYPE").unwrap();
    std::fs::write(
        dir.path().join("language-servers.json"),
        json!({"servers":[{"languages":["python"],"executable":protocol_fixture::executable("lsp"),"args":[]}]}).to_string(),
    ).unwrap();
    let service = AgentService::new(dir.path().to_owned());
    assert!(service.manager.servers.lock().await.is_empty());
    let root = dir.path().canonicalize().unwrap();
    assert!(!service.may_have_processes(&root));
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
    assert!(service.may_have_processes(&root));
    assert!(!service.may_have_processes(&root.join("another-checkout")));
    let busy = service.state.lock().await;
    assert!(service.may_have_processes(&root));
    drop(busy);
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

fn version_fixture(flag: &str) -> (tempfile::TempDir, PathBuf, AgentService) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.py");
    std::fs::write(&path, "valid").unwrap();
    std::fs::write(
        dir.path().join("language-servers.json"),
        json!({"servers":[{"languages":["python"],"executable":protocol_fixture::executable("lsp"),"args":[flag]}]}).to_string(),
    ).unwrap();
    let service = AgentService::new(dir.path().to_owned());
    (dir, path, service)
}

#[tokio::test]
async fn review_h3_delayed_versionless_clean_publication_cannot_describe_the_new_edit() {
    let (dir, path, service) = version_fixture("--delayed-versionless");
    service
        .request(dir.path(), &path, "hover", 0, 0, CancellationToken::new())
        .await
        .unwrap();
    std::fs::write(&path, "BAD_TYPE").unwrap();
    let result = service
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
        result["diagnostics"][0]["message"], "incompatible type",
        "{result}"
    );
    service.shutdown().await;
}

#[tokio::test]
async fn review_h3_versionless_only_servers_report_unknown_freshness_not_clean_results() {
    let (dir, path, service) = version_fixture("--versionless-only");
    let result = service
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
    assert_eq!(result["freshness"], "unavailable", "{result}");
    assert!(
        result["diagnostics"].is_null(),
        "Unknown freshness must not claim a clean diagnostic list"
    );
    service.shutdown().await;
}
