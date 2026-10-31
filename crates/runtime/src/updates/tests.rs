use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};

fn signed_candidate() -> (tempfile::TempDir, Service) {
    let directory = tempfile::tempdir().unwrap();
    let key = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let sign = |bytes: &[u8]| {
        STANDARD.encode(
            minisign::sign(
                Some(&key.pk),
                &key.sk,
                std::io::Cursor::new(bytes),
                Some("test fixture only"),
                None,
            )
            .unwrap()
            .to_string(),
        )
    };
    let policy = serde_json::from_value(json!({
        "enabled": true, "application_id": "com.qurkos.xylo", "channel": "stable",
        "manifest_url": "https://updates.example.test/stable.json",
        "signature_url": "https://updates.example.test/stable.json.sig",
        "public_key": STANDARD.encode(key.pk.to_box().unwrap().to_bytes()),
        "allowed_origins": ["https://updates.example.test"], "max_artifact_bytes": 1048576,
        "macos_team_id": "TEST000000"
    }))
    .unwrap();
    let target = xylo_distribution::native_target().unwrap();
    let format = if target.starts_with("macos-") {
        "app"
    } else if target.starts_with("linux-") {
        "appimage"
    } else {
        "nsis"
    };
    let updater = Updater::new(
        policy,
        "0.1.0".parse().unwrap(),
        target.clone(),
        directory.path().join("updates"),
        std::env::current_exe().unwrap(),
    )
    .unwrap();
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    use sha2::Digest;
    let digest = sha2::Sha256::digest(b"package")
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let metadata = serde_json::to_vec(&json!({
        "schema": 1, "application_id": "com.qurkos.xylo", "channel": "stable", "sequence": 2,
        "version": "0.2.0", "published_at": at-30, "expires_at": at+3600, "notes": "Test update",
        "platforms": {target: {"url": "https://updates.example.test/private-artifact", "signature": sign(b"package"), "format": format, "size_bytes": 7, "sha256": digest}}
    })).unwrap();
    let offer = updater
        .verify_manifest(&metadata, &sign(&metadata), at)
        .unwrap()
        .unwrap();
    let service = Service::from_updater(Some(updater));
    service.state.lock().offer = Some(offer);
    service.state.lock().phase = "available";
    (directory, service)
}

#[tokio::test]
async fn disabled_build_has_no_fake_update_success_or_network() {
    let data = tempfile::tempdir().unwrap();
    let service =
        Service::new(data.path().to_path_buf(), std::env::current_exe().unwrap()).unwrap();
    // Default builds deliberately have no maintainer key/feed. Test release
    // builds with a supplied private policy through their own qualification.
    let service = if service.updater.is_some() {
        Service::from_updater(None)
    } else {
        service
    };
    let before = service.status();
    assert_eq!(before["phase"], "disabled");
    assert_eq!(before["can_install"], false);
    let cancel = CancellationToken::new();
    let checked = service.check(cancel.clone()).await.unwrap();
    let staged = service.stage(cancel.clone()).await.unwrap();
    let installed = service.install(cancel).await.unwrap();
    assert_eq!(checked["phase"], "disabled");
    assert_eq!(staged["phase"], "disabled");
    assert_eq!(installed["phase"], "disabled");
    assert!(before["sequence"].as_u64() < checked["sequence"].as_u64());
    assert!(checked["sequence"].as_u64() < staged["sequence"].as_u64());
    assert!(staged["sequence"].as_u64() < installed["sequence"].as_u64());
    assert!(!data.path().join("updates").exists());
}

#[tokio::test]
async fn actions_are_exclusive_and_restart_remains_an_explicit_boundary() {
    let service = Service::from_updater(None);
    let _held = service.action.lock().await;
    assert!(
        service
            .check(CancellationToken::new())
            .await
            .unwrap_err()
            .to_string()
            .contains("in progress")
    );
    drop(_held);
    service.state.lock().restart_required = true;
    assert!(
        service
            .install(CancellationToken::new())
            .await
            .unwrap_err()
            .to_string()
            .contains("Restart")
    );
}

#[tokio::test]
async fn cancelled_download_retains_real_authenticated_candidate_without_fake_ready() {
    let (_directory, service) = signed_candidate();
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(
        service
            .stage(cancel)
            .await
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
    let status = service.status();
    assert_eq!(status["phase"], "available");
    assert_eq!(status["can_download"], true);
    assert_eq!(status["can_install"], false);
    assert_eq!(status["available"]["version"], "0.2.0");
    assert!(!status.to_string().contains("updates.example.test"));
    assert!(service.state.lock().offer.is_some());
}

#[tokio::test]
async fn aborted_waiter_cancels_io_and_keeps_action_excluded_until_job_finishes() {
    let (_directory, service) = signed_candidate();
    let (began_tx, began_rx) = tokio::sync::oneshot::channel();
    let (cancelled_tx, cancelled_rx) = tokio::sync::oneshot::channel();
    let (finish_tx, finish_rx) = std::sync::mpsc::channel();
    let running = service.clone();
    let task = tokio::spawn(async move {
        running
            .execute(CancellationToken::new(), "checking", move |_, cancel, _| {
                let _ = began_tx.send(());
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                while !cancel.is_cancelled() && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                ensure!(cancel.is_cancelled(), "Abandoned update was not cancelled");
                let _ = cancelled_tx.send(());
                finish_rx.recv_timeout(std::time::Duration::from_secs(2))?;
                anyhow::bail!("Update cancelled")
            })
            .await
    });
    began_rx.await.unwrap();
    task.abort();
    let _ = task.await;
    cancelled_rx.await.unwrap();
    assert!(
        service
            .stage(CancellationToken::new())
            .await
            .unwrap_err()
            .to_string()
            .contains("in progress")
    );
    finish_tx.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while service.status()["phase"] == "checking" {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(service.status()["phase"], "available");
    assert_eq!(service.status()["can_install"], false);
    assert!(service.state.lock().offer.is_some());
}
