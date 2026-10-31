#[test]
fn native_asset_locations_match_packaged_and_development_layouts() {
    use std::path::Path;
    let (root, asset) =
        super::packaged_asset_location(Path::new("/build/xylo-helper"), "whisper-runtime").unwrap();
    assert_eq!(root, Path::new("/build"));
    assert_eq!(asset, Path::new("/build/native-assets/whisper-runtime"));
    #[cfg(target_os = "macos")]
    {
        let (root, asset) = super::packaged_asset_location(
            Path::new("/Applications/Xylo.app/Contents/MacOS/xylo-helper"),
            "whisper-runtime",
        )
        .unwrap();
        assert_eq!(root, Path::new("/Applications/Xylo.app/Contents"));
        assert_eq!(
            asset,
            Path::new("/Applications/Xylo.app/Contents/Resources/native-assets/whisper-runtime")
        );
    }
}
use super::*;
#[test]
fn startup_keeps_only_running_install_stages() {
    let root = tempfile::tempdir().unwrap();
    let manager = AssetManager::new(root.path()).unwrap();
    let staging = manager.root.join("staging");
    let recovery = staging.join(format!("recovery-llama-runtime-{}", Uuid::new_v4()));
    let abandoned = staging.join(format!("llama-runtime-{}", Uuid::new_v4()));
    let live = staging.join(format!("whisper-runtime-{}", Uuid::new_v4()));
    let unrelated = staging.join("notes");
    for directory in [&recovery, &abandoned, &live, &unrelated] {
        std::fs::create_dir_all(directory.join("payload")).unwrap();
    }
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(manager.root.join("locks/whisper-runtime.lock"))
        .unwrap();
    fs2::FileExt::lock_exclusive(&held).unwrap();
    AssetManager::new(root.path()).unwrap();
    assert!(!recovery.exists() && !abandoned.exists());
    assert!(live.exists(), "A stage whose install is running must stay");
    assert!(!unrelated.exists());
    assert_eq!(stage_asset("a-b"), None);
}
#[test]
fn reviewed_catalog_is_pinned_and_models_require_an_explicit_selection() {
    let root = tempfile::tempdir().unwrap();
    let manager = AssetManager::new(root.path()).unwrap();
    let catalog = manager.catalog().unwrap();
    assert_eq!(manager.active_jobs(), 0);
    assert!(manager.status(None).unwrap().is_empty());
    assert!(
        catalog["assets"]
            .as_array()
            .unwrap()
            .iter()
            .all(|asset| asset["installed"] == false && asset["active"] == false)
    );
    assert!(
        manager
            .plan("qwen3-0.6b-q8")
            .unwrap()
            .contains(&"llama-runtime".into())
    );
    assert!(manager.install("https://arbitrary.example/binary").is_err());
}

#[tokio::test]
async fn installation_is_durable_verified_and_rejects_changed_bytes_on_activation() {
    let root = tempfile::tempdir().unwrap();
    let mut manager = AssetManager::new(root.path()).unwrap();
    let body = b"deterministic native fixture".to_vec();
    let (url, _, server) = download::tests::fixture(body.clone(), "normal").await;
    let mut spec = download::tests::specification(url, &body);
    spec.destination = "native-fixture".into();
    manager.catalog = Arc::new(vec![Asset {
        id: "fixture".into(),
        name: "Fixture".into(),
        kind: "model".into(),
        description: "fixture".into(),
        revision: "1".into(),
        project_url: "https://example.com".into(),
        license: catalog::License {
            identifier: "MIT".into(),
            url: "https://example.com/license".into(),
            notice: "fixture".into(),
        },
        requires: vec![],
        hidden: false,
        variants: vec![Variant {
            platform: "any".into(),
            files: vec![spec],
            exports: BTreeMap::from([(
                "model".into(),
                catalog::Export {
                    basename: "native-fixture".into(),
                    executable: false,
                },
            )]),
            source_build: false,
        }],
    }]);
    manager.downloader = download::tests::local_downloader(manager.root.join("downloads"));
    let job = manager.install("fixture").unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if manager.snapshot(job.job_id).unwrap().active() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            } else {
                break;
            }
        }
    })
    .await
    .unwrap();
    let status = manager.status(Some(job.job_id)).unwrap();
    assert_eq!(status[0].phase, "installed", "{}", status[0].message);
    assert_eq!(status[0].progress, 1.);
    let (directory, _) = manager
        .verified_exports("fixture", &CancellationToken::new())
        .await
        .unwrap();
    assert!(directory.starts_with(root.path().canonicalize().unwrap()));
    assert!(directory.join("receipt.json").is_file());
    std::fs::write(directory.join("native-fixture"), vec![0; body.len()]).unwrap();
    assert!(
        manager
            .verified_exports("fixture", &CancellationToken::new())
            .await
            .unwrap_err()
            .to_string()
            .contains("SHA256")
    );
    assert!(
        manager
            .activation("fixture", &CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(manager.catalog().unwrap()["assets"][0]["installed"], false);
    let repair = manager.install("fixture").unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while manager.snapshot(repair.job_id).unwrap().active() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(manager.snapshot(repair.job_id).unwrap().phase, "installed");
    manager
        .verified_exports("fixture", &CancellationToken::new())
        .await
        .unwrap();
    // The damaged copy was moved aside and removed, never left half-deleted.
    assert_eq!(
        std::fs::read_dir(manager.root.join("staging"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(
        std::fs::read_dir(manager.root.join("installed/fixture"))
            .unwrap()
            .count(),
        1
    );
    manager.shutdown().await;
    server.abort();
}
