use super::*;

#[test]
fn server_configuration_is_app_owned_and_bounded() {
    let dir = tempfile::tempdir().unwrap();
    // A project-local executable setting is never read by the resolver.
    let project = dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(
        project.join("language-servers.json"),
        br#"{"servers":[{"languages":["python"],"executable":"/bin/sh"}]}"#,
    )
    .unwrap();
    assert!(resolve_server(dir.path(), "python").is_err());
    std::fs::write(
        dir.path().join("language-servers.json"),
        br#"{"servers":[{"languages":["python"],"executable":"relative-command"}]}"#,
    )
    .unwrap();
    assert!(
        resolve_server(dir.path(), "python")
            .unwrap_err()
            .to_string()
            .contains("absolute")
    );
    std::fs::write(
        dir.path().join("language-servers.json"),
        vec![b' '; 1024 * 1024 + 1],
    )
    .unwrap();
    assert!(
        resolve_server(dir.path(), "python")
            .unwrap_err()
            .to_string()
            .contains("large")
    );
}

#[tokio::test]
async fn cancelled_editor_work_never_resolves_or_starts_a_server() {
    let dir = tempfile::tempdir().unwrap();
    let (events, _) = async_channel::bounded(8);
    let manager = Manager::new(dir.path().into(), events);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = manager
        .change(dir.path(), &dir.path().join("missing.rs"), 1, "", &cancel)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"));
    assert!(manager.servers.lock().await.is_empty());
}

#[tokio::test]
#[ignore = "requires an installed native clangd; run explicitly during OS qualification"]
async fn native_clangd_completion_diagnostics_versions_and_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = tokio::fs::canonicalize(dir.path()).await.unwrap();
    let path = workspace.join("sample.cpp");
    let text = "struct Widget { int count; };\nint main() { Widget w; w. }\n";
    tokio::fs::write(&path, text).await.unwrap();
    let (events, incoming) = async_channel::bounded(64);
    let manager = Manager::new(workspace.clone(), events);
    let cancel = CancellationToken::new();
    manager
        .change(&workspace, &path, 1, text, &cancel)
        .await
        .unwrap();
    let position = text.lines().nth(1).unwrap().find("w.").unwrap() + 2;
    let completed = manager
        .request(
            Uuid::new_v4(),
            &workspace,
            &path,
            1,
            text,
            "textDocument/completion",
            json!({"position":{"line":1,"character":position},"context":{"triggerKind":1}}),
            cancel.clone(),
        )
        .await
        .unwrap();
    assert!(completed.to_string().contains("count"), "{completed}");
    let diagnostics = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let LspEvent::Diagnostics {
                path: actual,
                version: Some(1),
                diagnostics,
            } = incoming.recv().await.unwrap()
                && actual == path
                && diagnostics
                    .as_array()
                    .is_some_and(|items| !items.is_empty())
            {
                break diagnostics;
            }
        }
    })
    .await
    .unwrap();
    assert!(!diagnostics.as_array().unwrap().is_empty());
    let fixed =
        "struct Widget { int count; };\nint main() { Widget w; w.count=1; return w.count; }\n";
    manager
        .change(&workspace, &path, 2, fixed, &cancel)
        .await
        .unwrap();
    assert!(
        manager
            .change(&workspace, &path, 1, text, &cancel)
            .await
            .unwrap_err()
            .to_string()
            .contains("superseded")
    );
    assert!(
        manager
            .change(&workspace, &path, 2, "different", &cancel)
            .await
            .unwrap_err()
            .to_string()
            .contains("reused")
    );
    let column = fixed.lines().nth(1).unwrap().find("count").unwrap() + 1;
    let hover = manager
        .request(
            Uuid::new_v4(),
            &workspace,
            &path,
            2,
            fixed,
            "textDocument/hover",
            json!({"position":{"line":1,"character":column}}),
            cancel,
        )
        .await
        .unwrap();
    assert!(hover.to_string().contains("int"), "{hover}");
    let client = manager
        .documents
        .lock()
        .await
        .get(&path)
        .unwrap()
        .client
        .clone();
    manager.close(&path).await.unwrap();
    assert!(manager.documents.lock().await.is_empty());
    manager.shutdown().await;
    assert!(manager.servers.lock().await.is_empty());
    tokio::time::timeout(Duration::from_secs(3), async {
        while !client.closed() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
