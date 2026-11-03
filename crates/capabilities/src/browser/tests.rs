use super::*;

#[test]
fn navigation_accepts_only_uncredentialed_http() {
    assert!(validate_url("https://example.com/page").is_ok());
    for url in [
        "file:///etc/passwd",
        "javascript:alert(1)",
        "https://user:secret@example.com",
        "data:text/html,hello",
    ] {
        assert!(validate_url(url).is_err());
    }
}

#[test]
fn runtime_candidate_is_only_a_sibling() {
    let directory = tempfile::tempdir().unwrap();
    for binary in ["xylo", "xylo-capability-host", "xylo.exe"] {
        let candidate = sibling_runtime(&directory.path().join(binary)).unwrap();
        assert_eq!(candidate.parent(), Some(directory.path()));
        assert_eq!(
            candidate.file_name().unwrap(),
            if cfg!(windows) {
                "xylo-obscura.exe"
            } else {
                "xylo-obscura"
            }
        );
    }
    // Public resolution must use exactly current_exe's sibling, regardless of
    // whether a bundled runtime is present in this test installation.
    let candidate = sibling_runtime(&std::env::current_exe().unwrap()).unwrap();
    match (runtime_executable(), validate_executable(&candidate)) {
        (Ok(actual), Ok(expected)) => assert_eq!(actual, expected),
        (Err(_), Err(_)) => {}
        _ => panic!("Runtime resolution did not match the sole bundled candidate"),
    }
}

#[test]
fn launch_is_exact_loopback_ephemeral_supervised_serve_contract() {
    let temp = tempfile::tempdir().unwrap();
    let executable = temp.path().join("xylo-obscura");
    let spec = launch_spec(
        executable.clone(),
        temp.path(),
        "a".repeat(64),
        Duration::from_secs(10),
        false,
    );
    assert_eq!(spec.executable, executable);
    assert_eq!(spec.cwd, temp.path());
    assert_eq!(
        spec.args,
        vec![
            "serve".to_owned(),
            "--host".into(),
            "127.0.0.1".into(),
            "--port".into(),
            "0".into(),
            "--workers".into(),
            "1".into(),
            "--max-connections".into(),
            "1".into(),
            "--ready-file".into(),
            temp.path()
                .join("ready.json")
                .to_string_lossy()
                .into_owned(),
            "--storage-dir".into(),
            temp.path().join("storage").to_string_lossy().into_owned(),
        ]
    );
    assert_eq!(
        spec.env,
        BTreeMap::from([("OBSCURA_CDP_TOKEN".into(), "a".repeat(64))])
    );
    assert!(!spec.args.iter().any(|arg| arg.contains(&"a".repeat(64))));
    assert!(
        !spec
            .args
            .iter()
            .any(|arg| arg.contains("allow-file-access"))
    );
    let private = launch_spec(
        executable,
        temp.path(),
        "b".repeat(64),
        Duration::from_secs(10),
        true,
    );
    assert_eq!(
        &private.args[..private.args.len() - 1],
        spec.args.as_slice()
    );
    assert_eq!(private.args.last().unwrap(), "--allow-private-network");
    assert!(!CapabilityConfig::default().browser_allow_private_network);
}

#[test]
fn ready_record_matches_tagged_source_and_rejects_redirects() {
    let mut ready: ReadyRecord = serde_json::from_value(json!({
        "pid":123, "host":"127.0.0.1", "port":32123,
        "websocket_path":"/devtools/browser"
    }))
    .unwrap();
    assert_eq!(
        ready.endpoint(Some(123)).unwrap(),
        "ws://127.0.0.1:32123/devtools/browser"
    );
    assert!(ready.endpoint(Some(124)).is_err());
    assert!(ready.endpoint(None).is_err());
    ready.host = "example.com".into();
    assert!(ready.endpoint(Some(123)).is_err());
    ready.host = "127.0.0.1".into();
    ready.websocket_path = "/devtools/browser?token=secret".into();
    assert!(ready.endpoint(Some(123)).is_err());
    ready.websocket_path = "/devtools/browser".into();
    ready.port = 0;
    assert!(ready.endpoint(Some(123)).is_err());
}

#[test]
fn screenshot_rejects_bad_encoding_dimensions_and_oversize_payloads() {
    assert!(screenshot_bytes(&json!({})).is_err());
    assert!(screenshot_bytes(&json!({"data":"not-base64"})).is_err());
    assert!(screenshot_bytes(&json!({"data":"a".repeat(MAX_SCREENSHOT_BYTES + 1)})).is_err());
    let mut header = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    header.extend_from_slice(&1280u32.to_be_bytes());
    header.extend_from_slice(&900u32.to_be_bytes());
    let encode = |bytes: &[u8]| {
        json!({
            "data":base64::engine::general_purpose::STANDARD.encode(bytes)
        })
    };
    assert!(screenshot_bytes(&encode(&header)).is_err());
    let mut png = std::io::Cursor::new(Vec::new());
    image::RgbaImage::new(1280, 900)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    assert!(screenshot_bytes(&encode(png.get_ref())).is_ok());
    header[20..24].copy_from_slice(&901u32.to_be_bytes());
    assert!(screenshot_bytes(&encode(&header)).is_err());
    assert!(screenshot_bytes(&encode(b"\x89PNG\r\n\x1a\n")).is_err());
}

#[test]
fn screenshot_publication_never_clobbers() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("existing.png");
    save_screenshot(&path, b"original").unwrap();
    assert!(save_screenshot(&path, b"replacement").is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"original");
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn download_is_rejected_before_launch_or_argument_validation() {
    let service = BrowserService::new(CapabilityConfig::default());
    let error = service
        .execute("browser_download", &json!({}), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), DOWNLOAD_UNSUPPORTED);
    assert!(service.state.lock().await.is_none());
    let definitions = definitions();
    assert_eq!(definitions.len(), 7);
    assert!(
        !definitions
            .iter()
            .any(|definition| definition.name == "browser_download")
    );
    assert_eq!(service.status().await["download"], "unavailable");
}

#[tokio::test]
async fn precancelled_operation_never_launches() {
    let service = BrowserService::new(CapabilityConfig::default());
    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = service
        .execute("browser_snapshot", &json!({}), cancel)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"));
    assert!(service.state.lock().await.is_none());
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn queued_cancellation_does_not_wait_for_another_operations_lock() {
    let service = BrowserService::new(CapabilityConfig::default());
    let guard = service.state.lock().await;
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::task::yield_now().await;
        trigger.cancel();
    });
    let error = tokio::time::timeout(
        Duration::from_secs(1),
        service.execute("browser_snapshot", &json!({}), cancel),
    )
    .await
    .expect("Queued cancellation must not try to acquire the session lock again")
    .unwrap_err();
    assert!(error.to_string().contains("cancelled"));
    assert!(guard.is_none());
}

#[tokio::test]
async fn queued_timeout_does_not_wait_for_another_operations_lock() {
    let service = BrowserService::new(CapabilityConfig {
        operation_timeout_seconds: 1,
        ..Default::default()
    });
    let guard = service.state.lock().await;
    let error = tokio::time::timeout(
        Duration::from_secs(2),
        service.execute("browser_snapshot", &json!({}), CancellationToken::new()),
    )
    .await
    .expect("Queued timeout must not try to acquire the session lock again")
    .unwrap_err();
    assert!(error.to_string().contains("timed out"));
    assert!(guard.is_none());
}

#[tokio::test]
async fn cdp_auth_session_viewport_events_and_script_errors() {
    use async_tungstenite::tungstenite::{
        Message,
        handshake::server::{Request, Response},
    };
    use futures_util::StreamExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}/devtools/browser", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = async_tungstenite::tokio::accept_hdr_async(
            stream,
            |request: &Request, response: Response| {
                assert_eq!(request.uri().path(), "/devtools/browser");
                assert!(request.uri().query().is_none());
                assert_eq!(request.headers()["authorization"], "Bearer test-secret");
                assert!(!request.headers().contains_key("origin"));
                Ok(response)
            },
        )
        .await
        .unwrap();
        for (index, method) in [
            "Target.createTarget",
            "Target.attachToTarget",
            "Emulation.setDeviceMetricsOverride",
            "Runtime.evaluate",
            "Runtime.evaluate",
        ]
        .into_iter()
        .enumerate()
        {
            let message = socket.next().await.unwrap().unwrap();
            let request: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
            assert_eq!(request["method"], method);
            assert_eq!(request["id"], (index + 1) as u64);
            if index >= 2 {
                assert_eq!(request["sessionId"], "session-test");
            }
            let result = match index {
                0 => json!({"targetId":"target-test"}),
                1 => {
                    assert_eq!(request["params"]["targetId"], "target-test");
                    assert_eq!(request["params"]["flatten"], true);
                    json!({"sessionId":"session-test"})
                }
                2 => {
                    assert_eq!(
                        request["params"],
                        json!({
                            "width":1280,"height":900,"deviceScaleFactor":1,"mobile":false
                        })
                    );
                    json!({})
                }
                3 => json!({"result":{"type":"boolean","value":true}}),
                _ => json!({"exceptionDetails":{"text":"fixture error"}}),
            };
            socket
                .send(Message::Text(
                    json!({
                        "method":"Page.loadEventFired", "params":{}
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            socket
                .send(Message::Text(
                    json!({
                        "id":request["id"], "result":result
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
        }
    });
    let mut client = Cdp::connect(&endpoint, "test-secret").await.unwrap();
    client.initialize().await.unwrap();
    assert_eq!(client.evaluate("true".into()).await.unwrap(), json!(true));
    assert!(client.evaluate("throw new Error()".into()).await.is_err());
    server.await.unwrap();
}

/// Run in a packaged layout: place the render-enabled xylo-obscura binary
/// beside this test executable (usually target/debug/deps), never via PATH/env.
#[tokio::test]
#[ignore = "Requires the bundled render-enabled xylo-obscura sibling binary"]
async fn bundled_obscura_real_seven_operations_and_cleanup() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    runtime_executable().expect("Install bundled xylo-obscura beside the test executable");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let http = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut request = [0; 4096];
                let _ = stream.read(&mut request).await;
                let body = r#"<!doctype html><html><head><title>Obscura acceptance</title></head><body><label>Name<input id="name" aria-label="Name"></label><button id="submit" onclick="document.getElementById('result').textContent=document.getElementById('name').value">Submit</button><div id="result" role="status"></div></body></html>"#;
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                );
                let _ = stream.write_all(reply.as_bytes()).await;
            });
        }
    });
    let temp = tempfile::tempdir().unwrap();
    let service = BrowserService::new(CapabilityConfig {
        artifacts: temp.path().to_owned(),
        operation_timeout_seconds: 30,
        browser_allow_private_network: true,
        ..Default::default()
    });
    let cancel = CancellationToken::new();
    let nav = service
        .execute(
            "browser_navigate",
            &json!({"url":format!("http://{address}/")}),
            cancel.clone(),
        )
        .await
        .unwrap();
    assert!(nav.summary.contains("Obscura acceptance"));
    let profile = service
        .state
        .lock()
        .await
        .as_ref()
        .unwrap()
        .process
        .profile()
        .to_owned();
    service
        .execute("browser_wait", &json!({"selector":"#name"}), cancel.clone())
        .await
        .unwrap();
    service
        .execute(
            "browser_type",
            &json!({
                "selector":"#name", "text":"Obscura proof"
            }),
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
    assert!(snapshot.summary.contains("Obscura proof"));
    let screenshot = service
        .execute("browser_screenshot", &json!({}), cancel.clone())
        .await
        .unwrap();
    let png = tokio::fs::read(&screenshot.artifacts[0]).await.unwrap();
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 1280);
    assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 900);
    assert!(
        service
            .execute("browser_download", &json!({}), cancel.clone())
            .await
            .is_err()
    );
    assert!(service.state.lock().await.is_some());
    service
        .execute("browser_close", &json!({}), cancel)
        .await
        .unwrap();
    assert!(service.state.lock().await.is_none());
    assert!(
        !profile.exists(),
        "private storage must be removed after reaping"
    );
    http.abort();
}

#[tokio::test]
#[ignore = "Requires the bundled render-enabled xylo-obscura sibling binary"]
async fn bundled_cancellation_stops_process_and_removes_profile() {
    runtime_executable().expect("Install bundled xylo-obscura beside the test executable");
    let service = BrowserService::new(CapabilityConfig::default());
    service
        .execute("browser_snapshot", &json!({}), CancellationToken::new())
        .await
        .unwrap();
    let profile = service
        .state
        .lock()
        .await
        .as_ref()
        .unwrap()
        .process
        .profile()
        .to_owned();
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        trigger.cancel();
    });
    let error = service
        .execute(
            "browser_wait",
            &json!({"selector":"#never-present"}),
            cancel,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"));
    assert!(service.state.lock().await.is_none());
    // Cancellation kills synchronously; reaping/profile deletion is scheduled.
    tokio::time::timeout(Duration::from_secs(10), async {
        while profile.exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
}
