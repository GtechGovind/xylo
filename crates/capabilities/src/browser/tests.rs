use super::*;
#[test]
fn navigation_rejects_executable_and_local_file_urls() {
    assert!(validate_url("https://example.com/page").is_ok());
    assert!(validate_url("file:///etc/passwd").is_err());
    assert!(validate_url("javascript:alert(1)").is_err());
    assert!(validate_url("https://user:secret@example.com").is_err());
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
                        r#"<html><head><title>Native capability test</title></head><body><label>Name<input id="name" aria-label="Name"></label><button id="submit" onclick="document.getElementById('result').textContent=document.getElementById('name').value">Submit</button><div id="result" role="status"></div></body></html>"#,
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
            cancel,
        )
        .await
        .unwrap();
    assert_eq!(
        tokio::fs::read_to_string(&download.artifacts[0])
            .await
            .unwrap(),
        "native-download"
    );
    service.shutdown().await.unwrap();
    assert!(!service.status().await.to_string().contains("running"));
    http.abort();
}
