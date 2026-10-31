use super::*;
#[test]
fn navigation_rejects_executable_and_local_file_urls() {
    assert!(validate_url("https://example.com/page").is_ok());
    assert!(validate_url("file:///etc/passwd").is_err());
    assert!(validate_url("javascript:alert(1)").is_err());
    assert!(validate_url("https://user:secret@example.com").is_err());
}

#[test]
fn download_budget_defaults_and_clamps_to_ceiling() {
    assert_eq!(download_budget(&json!({})), DEFAULT_DOWNLOAD_MAX_BYTES);
    assert_eq!(download_budget(&json!({"max_bytes":0})), 1);
    assert_eq!(download_budget(&json!({"max_bytes":10})), 10);
    assert_eq!(
        download_budget(&json!({"max_bytes":u64::MAX})),
        DOWNLOAD_MAX_BYTES_CEILING
    );
}

#[test]
fn progress_past_budget_is_rejected_and_nan_fails_closed() {
    // total_bytes is 0 when the server did not send a length.
    assert!(!download_exceeds_budget(10.0, 0.0, 10));
    assert!(!download_exceeds_budget(5.0, 10.0, 10));
    assert!(download_exceeds_budget(11.0, 0.0, 10));
    assert!(download_exceeds_budget(0.0, 11.0, 10));
    assert!(download_exceeds_budget(f64::NAN, 0.0, 10));
    assert!(download_exceeds_budget(0.0, f64::NAN, 10));
}

#[tokio::test]
async fn staged_file_over_budget_is_deleted() {
    let temp = tempfile::tempdir().unwrap();
    let small = temp.path().join("small");
    let large = temp.path().join("large");
    std::fs::write(&small, b"1234").unwrap();
    std::fs::write(&large, b"12345").unwrap();
    assert_eq!(enforce_file_budget(&small, 4).await.unwrap(), 4);
    assert!(small.is_file());
    assert!(enforce_file_budget(&large, 4).await.is_err());
    assert!(!large.exists());
    assert!(
        enforce_file_budget(&temp.path().join("missing"), 4)
            .await
            .is_err()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn staged_symlink_is_rejected_without_following_it() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("target");
    let link = temp.path().join("link");
    std::fs::write(&target, b"x").unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(enforce_file_budget(&link, 4).await.is_err());
    assert!(!link.exists());
    assert!(target.is_file());
}

#[tokio::test]
async fn restore_runs_after_success_and_after_failure() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let restores = AtomicUsize::new(0);
    let restore = || async {
        restores.fetch_add(1, Ordering::SeqCst);
        Ok(())
    };
    assert_eq!(
        run_then_restore(async { Ok(7) }, restore()).await.unwrap(),
        7
    );
    let failed = run_then_restore(
        async { Err::<(), _>(anyhow::anyhow!("body failed")) },
        restore(),
    )
    .await
    .unwrap_err();
    assert_eq!(failed.to_string(), "body failed");
    assert_eq!(restores.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn restore_failure_is_reported_and_never_hides_body_error() {
    let after_success = run_then_restore(async { Ok(()) }, async {
        Err(anyhow::anyhow!("deny failed"))
    })
    .await
    .unwrap_err();
    assert!(format!("{after_success:#}").contains("deny failed"));
    let both = run_then_restore(
        async { Err::<(), _>(anyhow::anyhow!("body failed")) },
        async { Err(anyhow::anyhow!("deny failed")) },
    )
    .await
    .unwrap_err();
    let message = format!("{both:#}");
    assert!(message.contains("body failed") && message.contains("deny failed"));
}

/// Separate platform acceptance from the dependency-free offline suite.
#[tokio::test]
#[ignore = "Requires an explicit local Chromium executable in XYLO_CHROMIUM"]
async fn managed_chromium_performs_real_cdp_workflow_on_loopback() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let executable = std::env::var_os("XYLO_CHROMIUM").expect("Set XYLO_CHROMIUM");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let http = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut request = [0; 4096];
                let count = stream.read(&mut request).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..count]);
                let (content_type, extra, body) = if request.starts_with("GET /download ") {
                    (
                        "text/plain",
                        "Content-Disposition: attachment; filename=proof.txt\r\n",
                        "native-download",
                    )
                } else {
                    (
                        "text/html",
                        "",
                        r#"<html><head><title>Native capability test</title></head><body><label>Name<input id="name" aria-label="Name"></label><button id="submit" onclick="document.getElementById('result').textContent=document.getElementById('name').value">Submit</button><div id="result" role="status"></div><a id="file" href="/download">file</a></body></html>"#,
                    )
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let artifacts = root.clone();
    let service = BrowserService::new(CapabilityConfig {
        workspace: root.clone(),
        artifacts: root,
        chromium: Some(PathBuf::from(executable)),
        operation_timeout_seconds: 20,
        ..Default::default()
    });
    let cancel = CancellationToken::new();
    service
        .execute(
            "browser_navigate",
            &json!({"url":format!("http://{address}/")}),
            cancel.clone(),
        )
        .await
        .unwrap();
    service
        .execute("browser_wait", &json!({"selector":"#name"}), cancel.clone())
        .await
        .unwrap();
    service
        .execute(
            "browser_type",
            &json!({"selector":"#name","text":"Native proof"}),
            cancel.clone(),
        )
        .await
        .unwrap();
    service
        .execute(
            "browser_click",
            &json!({"selector":"#submit"}),
            cancel.clone(),
        )
        .await
        .unwrap();
    let snapshot = service
        .execute("browser_snapshot", &json!({}), cancel.clone())
        .await
        .unwrap();
    assert!(snapshot.summary.contains("Native proof"));
    let screenshot = service
        .execute("browser_screenshot", &json!({}), cancel.clone())
        .await
        .unwrap();
    assert!(screenshot.artifacts[0].is_file());
    let download = service
        .execute(
            "browser_download",
            &json!({"url":format!("http://{address}/download")}),
            cancel.clone(),
        )
        .await
        .unwrap();
    assert_eq!(
        tokio::fs::read_to_string(&download.artifacts[0])
            .await
            .unwrap(),
        "native-download"
    );
    // Regression: the download's navigation never finishes loading; it must
    // not leave later navigations queued behind it until the timeout.
    let started = Instant::now();
    let page = service
        .execute(
            "browser_navigate",
            &json!({"url":format!("http://{address}/")}),
            cancel.clone(),
        )
        .await
        .unwrap();
    assert!(page.summary.contains("Native capability test"));
    assert!(started.elapsed() < Duration::from_secs(10));
    // Regression: downloads are denied again once browser_download returns,
    // so clicking an attachment link on a live session writes nothing.
    let staged = |root: PathBuf| {
        let profiles: Vec<_> = std::fs::read_dir(root)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".browser-"))
            .collect();
        assert_eq!(profiles.len(), 1, "session must still be running");
        std::fs::read_dir(profiles[0].path().join("downloads"))
            .unwrap()
            .count()
    };
    // An attachment never commits, so the page still shows the form with the
    // #file link.
    service
        .execute(
            "browser_click",
            &json!({"selector":"#file"}),
            cancel.clone(),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(staged(artifacts.clone()), 0);
    let oversized = service
        .execute(
            "browser_download",
            &json!({"url":format!("http://{address}/download"),"max_bytes":4}),
            cancel,
        )
        .await;
    assert!(oversized.is_err());
    service.shutdown().await.unwrap();
    assert!(!service.status().await.to_string().contains("running"));
    http.abort();
}
