use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use xylo_core::{Conversation, ImageGenerationProtocol, ProviderConfig, now_ms};

fn fixture(
    mode: PermissionMode,
    endpoint: String,
) -> (
    tempfile::TempDir,
    Arc<ToolBroker>,
    tokio::sync::mpsc::Receiver<EngineEvent>,
) {
    let directory = tempfile::tempdir().unwrap();
    let artifacts = directory.path().join("artifacts");
    let service = ImageGenerationService::new(directory.path().into(), artifacts.clone()).unwrap();
    service
        .save_provider(&ImageGenerationProvider {
            name: "Explicit image connection".into(),
            protocol: ImageGenerationProtocol::OpenAiImages,
            endpoint,
            model: "explicit-image-model".into(),
            credential_ref: None,
            headers: Default::default(),
            timeout_seconds: 5,
        })
        .unwrap();
    let store = Store::open(directory.path().join("history.sqlite")).unwrap();
    let session = Uuid::new_v4();
    store
        .upsert_conversation(&Conversation {
            id: session,
            title: "Images".into(),
            workspace: directory.path().into(),
            updated_at: now_ms(),
        })
        .unwrap();
    let request = TurnRequest {
        session_id: session,
        turn_id: Uuid::new_v4(),
        prompt: "Generate an image".into(),
        workspace: directory.path().into(),
        provider: ProviderConfig::default(),
        permissions: mode,
        attachments: Vec::new(),
    };
    let (events, receiver) = tokio::sync::mpsc::channel(256);
    let broker = ToolBroker::new(
        request,
        store,
        CapabilityConfig {
            workspace: directory.path().into(),
            artifacts,
            ..Default::default()
        },
        ApprovalGate::default(),
        events,
        CancellationToken::new(),
    )
    .unwrap();
    (directory, Arc::new(broker), receiver)
}
async fn approve(
    broker: &ToolBroker,
    events: &mut tokio::sync::mpsc::Receiver<EngineEvent>,
    allow: bool,
) -> Uuid {
    loop {
        if let EngineEvent::ApprovalRequired {
            action_id,
            name,
            arguments,
            ..
        } = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .unwrap()
            .unwrap()
        {
            assert_eq!(name, "image_generate");
            assert_eq!(
                arguments["selected_image_provider"]["name"],
                "Explicit image connection"
            );
            assert_eq!(
                arguments["selected_image_provider"]["model"],
                "explicit-image-model"
            );
            assert!(arguments.get("credential_ref").is_none());
            broker.approvals.decide(action_id, allow).unwrap();
            return action_id;
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approved_image_tool_uses_frozen_saved_provider_actual_output_and_durable_no_replay() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (directory, broker, mut events) = fixture(PermissionMode::AskForWrites, endpoint);
    let service =
        ImageGenerationService::new(directory.path().into(), broker.artifacts.clone()).unwrap();
    // Changing settings while approval is pending cannot redirect this turn's action.
    let mut changed = service.configured_provider().unwrap().unwrap();
    changed.endpoint = "http://127.0.0.1:1".into();
    changed.model = "different-model".into();
    service.save_provider(&changed).unwrap();
    let server = tokio::spawn(async move {
        // Approval and native admission may queue before HTTP dispatch. Keep
        // listening during that phase; the execution below has an outer bound
        // and the configured provider response timeout remains five seconds.
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = BufReader::new(socket);
        let mut line = String::new();
        socket.read_line(&mut line).await.unwrap();
        assert!(line.starts_with("POST /v1/images/generations "));
        let mut length = 0;
        loop {
            line.clear();
            socket.read_line(&mut line).await.unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse::<usize>().unwrap();
            }
        }
        assert!(length < 64 * 1024);
        let mut bytes = vec![0; length];
        socket.read_exact(&mut bytes).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["model"], "explicit-image-model");
        assert_eq!(body["size"], "1024x1024");
        let body=json!({"data":[{"b64_json":STANDARD.encode(b"\x89\x50\x4e\x47\x0d\x0a\x1a\x0a\x00\x00\x00\x0d\x49\x48\x44\x52\x00\x00\x00\x02\x00\x00\x00\x02\x08\x06\x00\x00\x00\x72\xb6\x0d\x24\x00\x00\x00\x11\x49\x44\x41\x54\x78\x9c\x63\x50\x70\x48\xf8\x0f\xc2\x0c\x30\x06\x00\x39\x74\x06\xfd\x20\x19\xfd\xfb\x00\x00\x00\x00\x49\x45\x4e\x44\xae\x42\x60\x82")}]}).to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket
            .get_mut()
            .write_all(response.as_bytes())
            .await
            .unwrap();
    });
    let task = broker.clone();
    let running = tokio::task::spawn_blocking(move || {
        task.execute(
            "image-call",
            "image_generate",
            json!({"prompt":"A fixture image","size":"1024x1024"}),
        )
    });
    let action = approve(&broker, &mut events, true).await;
    let output = tokio::time::timeout(Duration::from_secs(30), running)
        .await
        .expect("Approved image execution did not finish within its fixture budget")
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    let result: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(result["status"], "success");
    let path = PathBuf::from(result["artifacts"][0].as_str().unwrap());
    assert!(path.is_file());
    assert_eq!(
        service.receipt(action).unwrap().unwrap().state,
        ImageGenerationState::Succeeded
    );
    assert_eq!(
        broker.journaled("image-call").unwrap().unwrap().state,
        "succeeded"
    );
    let task = broker.clone();
    let repeated = tokio::task::spawn_blocking(move || {
        task.execute(
            "image-call",
            "image_generate",
            json!({"prompt":"A fixture image","size":"1024x1024"}),
        )
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(serde_json::from_str::<Value>(&repeated).unwrap(), result);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn image_tool_read_only_denial_and_arbitrary_provider_override_never_dispatch() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (_directory, broker, _events) = fixture(PermissionMode::ReadOnly, endpoint.clone());
    let task = broker.clone();
    assert!(
        tokio::task::spawn_blocking(move || task.execute(
            "readonly-image",
            "image_generate",
            json!({"prompt":"image"})
        ))
        .await
        .unwrap()
        .unwrap_err()
        .to_string()
        .contains("read-only")
    );
    assert!(broker.journaled("readonly-image").unwrap().is_none());
    let (_directory, broker, mut events) = fixture(PermissionMode::AskForWrites, endpoint);
    let task = broker.clone();
    assert!(
        tokio::task::spawn_blocking(move || task.execute(
            "override-image",
            "image_generate",
            json!({"prompt":"image","endpoint":"https://other.example"})
        ))
        .await
        .unwrap()
        .is_err()
    );
    assert!(broker.journaled("override-image").unwrap().is_none());
    let task = broker.clone();
    let running = tokio::task::spawn_blocking(move || {
        task.execute("denied-image", "image_generate", json!({"prompt":"image"}))
    });
    approve(&broker, &mut events, false).await;
    assert!(running.await.unwrap().is_err());
    assert_eq!(
        broker.journaled("denied-image").unwrap().unwrap().state,
        "denied"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn image_tool_transport_loss_retains_uncertainty_and_blocks_action_replay() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (_directory, broker, mut events) = fixture(PermissionMode::AskForWrites, endpoint);
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        drop(socket);
    });
    let task = broker.clone();
    let running = tokio::task::spawn_blocking(move || {
        task.execute("lost-image", "image_generate", json!({"prompt":"image"}))
    });
    approve(&broker, &mut events, true).await;
    assert!(
        tokio::time::timeout(Duration::from_secs(30), running)
            .await
            .expect("Approved transport-loss execution did not finish within its fixture budget")
            .unwrap()
            .is_err()
    );
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        broker.journaled("lost-image").unwrap().unwrap().state,
        "outcome_unknown"
    );
    let task = broker.clone();
    assert!(
        tokio::task::spawn_blocking(move || task.execute(
            "lost-image",
            "image_generate",
            json!({"prompt":"image"})
        ))
        .await
        .unwrap()
        .unwrap_err()
        .to_string()
        .contains("Reconcile")
    );
}
