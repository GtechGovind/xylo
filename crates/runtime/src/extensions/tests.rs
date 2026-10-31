use super::*;
fn fixture(
    command: &str,
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    xylo_services::Store,
    ExtensionPackage,
    std::path::PathBuf,
) {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(source.join("hooks")).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(
        source.join("hooks/hooks.json"),
        json!({"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":command}]}]}})
            .to_string(),
    )
    .unwrap();
    let root = directory.path().join("profile/extensions");
    let service = ExtensionService::new(&root);
    let package = service.install_local(&source).unwrap();
    service.set_enabled(&package.id, true).unwrap();
    service
        .component_setup(
            &package.id,
            "hooks",
            ComponentSetup {
                workspace: workspace.clone(),
                ..Default::default()
            },
        )
        .unwrap();
    let store = xylo_services::Store::open(directory.path().join("profile/store.db")).unwrap();
    (directory, root, store, package, workspace)
}
fn vault(root: &std::path::Path) -> Result<xylo_services::CredentialVault> {
    xylo_services::CredentialVault::open(root, xylo_core::SecretStorage::LocalFile)
}
/// A profile whose single source has a fresh cached directory of
/// `names`, so no action touches the network.
fn directory_fixture(names: &[&str]) -> (tempfile::TempDir, std::path::PathBuf) {
    use sha2::{Digest, Sha256};
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("extensions");
    let source = ExtensionSource {
        name: "Fixture".into(),
        repository: "fixture/plugins".into(),
        revision: "main".into(),
        subdirectory: String::new(),
    };
    ExtensionService::new(&root)
        .save_sources(std::slice::from_ref(&source))
        .unwrap();
    let entries: Vec<_> = names
        .iter()
        .map(|name| DirectoryEntry {
            id: format!("fixture/plugins#{name}"),
            name: (*name).into(),
            display_name: name.to_uppercase(),
            description: format!("{name} helper"),
            version: Some("1.0.0".into()),
            author: None,
            homepage: None,
            categories: vec![
                if name.starts_with('r') {
                    "Review"
                } else {
                    "Tools"
                }
                .into(),
            ],
            keywords: Vec::new(),
            components: Vec::new(),
            source: "Fixture".into(),
            install: None,
            unavailable: Some("fixture".into()),
            installed: false,
            update_available: false,
        })
        .collect();
    let key = hex::encode(Sha256::digest(b"fixture/plugins\0main\0"));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    std::fs::create_dir_all(root.join("directory")).unwrap();
    std::fs::write(
        root.join("directory").join(format!("{key}.json")),
        json!({"source_key":key,"etag":null,"revision":"abc","fetched_at":now,
            "freshness_note":null,"entries":entries,"manifests":{}})
        .to_string(),
    )
    .unwrap();
    (directory, root)
}

async fn run(
    root: &std::path::Path,
    store: &xylo_services::Store,
    name: &str,
    args: Value,
) -> Value {
    execute(
        root.to_owned(),
        store.clone(),
        vault(root),
        uuid::Uuid::new_v4(),
        name,
        &args,
        CancellationToken::new(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn directory_search_reads_the_cache_with_filters_and_paging() {
    let (directory, root) = directory_fixture(&["reviewer", "formatter", "runner"]);
    let store = xylo_services::Store::open(directory.path().join("store.db")).unwrap();
    let all = run(&root, &store, "extension_directory", json!({})).await;
    let page: DirectoryPage = serde_json::from_value(all["directory"].clone()).unwrap();
    assert_eq!(page.total, 3);
    assert_eq!(page.sources.len(), 1);
    let filtered = run(
        &root,
        &store,
        "extension_directory",
        json!({"category":"review","limit":1}),
    )
    .await;
    let page: DirectoryPage = serde_json::from_value(filtered["directory"].clone()).unwrap();
    assert_eq!(page.total, 2);
    assert_eq!(page.entries.len(), 1);
    let text = run(
        &root,
        &store,
        "extension_directory",
        json!({"text":"FORMAT"}),
    )
    .await;
    let page: DirectoryPage = serde_json::from_value(text["directory"].clone()).unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].name, "formatter");
}

#[tokio::test]
async fn directory_refresh_reuses_a_fresh_cache_and_returns_statuses_and_results() {
    let (directory, root) = directory_fixture(&["reviewer", "formatter"]);
    let store = xylo_services::Store::open(directory.path().join("store.db")).unwrap();
    let refreshed = run(
        &root,
        &store,
        "extension_directory_refresh",
        json!({"force":false,"query":{"text":"review"}}),
    )
    .await;
    let sources: Vec<DirectorySourceStatus> =
        serde_json::from_value(refreshed["sources"].clone()).unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].entries, 2);
    let page: DirectoryPage = serde_json::from_value(refreshed["directory"].clone()).unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].name, "reviewer");
}

#[tokio::test]
async fn native_component_receipt_reuses_result_without_reexecuting_hook() {
    let (_directory, root, store, package, workspace) =
        fixture("printf once >> receipt-marker; printf '{}'");
    let attempt = uuid::Uuid::new_v4();
    let args = json!({"id":package.id,"component":"hooks"});
    let first = execute(
        root.clone(),
        store.clone(),
        vault(&root),
        attempt,
        "extension_component_verify",
        &args,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let second = execute(
        root.clone(),
        store.clone(),
        vault(&root),
        attempt,
        "extension_component_verify",
        &args,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(first, second);
    assert_eq!(
        std::fs::read_to_string(workspace.join("receipt-marker")).unwrap(),
        "once"
    );
    assert_eq!(first["component_status"]["readiness"], "execution_verified");
    assert_eq!(
        store.extension_execution_receipts(5).unwrap()[0]["state"],
        "succeeded"
    );
    assert!(store.conversations(10).unwrap().is_empty());
}
#[tokio::test]
async fn cancelled_native_component_has_unknown_receipt_and_same_attempt_never_replays() {
    let (_directory, root, store, package, workspace) =
        fixture("printf once >> receipt-marker; sleep 30; printf '{}'");
    let attempt = uuid::Uuid::new_v4();
    let args = json!({"id":package.id,"component":"hooks"});
    let cancel = CancellationToken::new();
    let worker_cancel = cancel.clone();
    let worker_root = root.clone();
    let worker_store = store.clone();
    let worker_args = args.clone();
    let running = tokio::spawn(async move {
        execute(
            worker_root.clone(),
            worker_store,
            vault(&worker_root),
            attempt,
            "extension_component_verify",
            &worker_args,
            worker_cancel,
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        // Opening shell redirection creates the file before printf writes.
        // Cancel only after the observable effect this test asserts exists.
        while std::fs::read_to_string(workspace.join("receipt-marker"))
            .ok()
            .as_deref()
            != Some("once")
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    cancel.cancel();
    assert!(running.await.unwrap().is_err());
    assert_eq!(
        store.extension_execution_receipts(5).unwrap()[0]["state"],
        "outcome_unknown"
    );
    let retry = execute(
        root.clone(),
        store,
        vault(&root),
        attempt,
        "extension_component_verify",
        &args,
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(retry.to_string().contains("outcome is unknown"));
    assert_eq!(
        std::fs::read_to_string(workspace.join("receipt-marker")).unwrap(),
        "once"
    );
}
