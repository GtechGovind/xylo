use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

const PNG: &[u8] = include_bytes!("../../../../packaging/icons/app.png");
struct Recorded {
    line: String,
    headers: String,
    body: Value,
}
async fn request(socket: tokio::net::TcpStream) -> (BufReader<tokio::net::TcpStream>, Recorded) {
    tokio::time::timeout(Duration::from_secs(5), request_inner(socket))
        .await
        .expect("HTTP fixture read deadline")
}
async fn request_inner(
    socket: tokio::net::TcpStream,
) -> (BufReader<tokio::net::TcpStream>, Recorded) {
    let mut socket = BufReader::new(socket);
    let mut line = String::new();
    socket.read_line(&mut line).await.unwrap();
    let first = line.clone();
    let mut headers = String::new();
    let mut length = 0;
    loop {
        line.clear();
        socket.read_line(&mut line).await.unwrap();
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse::<usize>().unwrap();
        }
        headers.push_str(&line);
        if line == "\r\n" {
            break;
        }
    }
    assert!(length <= 64 * 1024);
    let mut body = vec![0; length];
    socket.read_exact(&mut body).await.unwrap();
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    (
        socket,
        Recorded {
            line: first,
            headers: headers.to_ascii_lowercase(),
            body,
        },
    )
}
async fn respond(
    socket: &mut BufReader<tokio::net::TcpStream>,
    status: u16,
    payload: &[u8],
    extra: &str,
) {
    let header = format!(
        "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
        payload.len()
    );
    socket.get_mut().write_all(header.as_bytes()).await.unwrap();
    for bytes in payload.chunks(11) {
        socket.get_mut().write_all(bytes).await.unwrap();
        tokio::task::yield_now().await;
    }
}
async fn fixture(payload: Value, status: u16) -> (String, tokio::task::JoinHandle<Recorded>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let (mut socket, record) = request(socket).await;
        respond(
            &mut socket,
            status,
            payload.to_string().as_bytes(),
            "x-request-id: fixture-request\r\n",
        )
        .await;
        record
    });
    (endpoint, task)
}
fn generation(endpoint: String, protocol: ImageGenerationProtocol) -> ImageGenerationRequest {
    ImageGenerationRequest {
        id: Uuid::new_v4(),
        session_id: Uuid::new_v4(),
        provider: ImageGenerationProvider {
            name: "Chosen image connection".into(),
            protocol,
            endpoint,
            model: "explicit-image-model".into(),
            credential_ref: Some("saved-key-reference".into()),
            headers: Default::default(),
            timeout_seconds: 10,
        },
        prompt: "Fixture prompt that must not be stored in receipts".into(),
        size: "1024x1024".into(),
    }
}
fn service() -> (tempfile::TempDir, ImageGenerationService) {
    let temp = tempfile::tempdir().unwrap();
    let service =
        ImageGenerationService::new(temp.path().join("data"), temp.path().join("data/artifacts"))
            .unwrap();
    (temp, service)
}

#[tokio::test]
async fn openai_generates_verified_managed_image_without_persisting_prompt_or_secret_and_never_replays()
 {
    let (endpoint, server) =
        fixture(json!({"data":[{"b64_json":STANDARD.encode(PNG)}]}), 200).await;
    let (_temp, service) = service();
    let mut request = generation(endpoint, ImageGenerationProtocol::OpenAiImages);
    let receipt = service
        .generate(
            request.clone(),
            Some("fixture-private-key".into()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(receipt.state, ImageGenerationState::Succeeded);
    assert_eq!(receipt.images.len(), 1);
    assert_eq!(
        receipt.images[0].attachment.source,
        AttachmentSource::Generated
    );
    assert_eq!(fs::read(&receipt.images[0].artifact_path).unwrap(), PNG);
    assert!(receipt.images[0].preview_path.is_file());
    assert!(receipt.dispatch_started);
    assert_eq!(
        receipt.provider_request_id.as_deref(),
        Some("fixture-request")
    );
    let sent = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert!(sent.line.starts_with("POST /v1/images/generations "));
    assert!(
        sent.headers
            .contains("authorization: bearer fixture-private-key")
    );
    assert_eq!(sent.body["model"], "explicit-image-model");
    assert_eq!(sent.body["n"], 1);
    assert_eq!(sent.body["size"], "1024x1024");
    let stored = fs::read_to_string(service.job_path(request.id)).unwrap();
    assert!(!stored.contains(&request.prompt));
    assert!(!stored.contains("fixture-private-key"));
    assert!(!stored.contains("base64"));
    let again = service
        .generate(
            request.clone(),
            Some("fixture-private-key".into()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(again.images, receipt.images);
    request.prompt = "Changed request with reused ID".into();
    assert!(
        service
            .generate(
                request,
                Some("fixture-private-key".into()),
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    fs::write(&receipt.images[0].artifact_path, b"tampered output").unwrap();
    assert!(service.receipt(receipt.id).is_err());
}
#[tokio::test]
async fn gemini_sends_native_image_modality_and_selected_model_and_saves_actual_output() {
    let(endpoint,server)=fixture(json!({"candidates":[{"finishReason":"STOP","content":{"parts":[{"text":"Generated"},{"inlineData":{"mimeType":"image/png","data":STANDARD.encode(PNG)}}]}}]}),200).await;
    let (_temp, service) = service();
    let request = generation(endpoint, ImageGenerationProtocol::GeminiGenerateContent);
    let receipt = service
        .generate(
            request,
            Some("gemini-fixture-key".into()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(receipt.state, ImageGenerationState::Succeeded);
    let sent = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert!(
        sent.line
            .contains("/v1beta/models/explicit-image-model:generateContent")
    );
    assert!(sent.headers.contains("x-goog-api-key: gemini-fixture-key"));
    assert!(!sent.headers.contains("authorization:"));
    assert_eq!(
        sent.body["generationConfig"]["responseModalities"],
        json!(["TEXT", "IMAGE"])
    );
    assert_eq!(
        sent.body["generationConfig"]["imageConfig"]["imageSize"],
        "1K"
    );
    assert_eq!(fs::read(&receipt.images[0].artifact_path).unwrap(), PNG);
}
#[tokio::test]
async fn compatible_url_output_download_has_no_provider_credentials_and_is_bounded() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let image_url = format!("{endpoint}/result.png");
    let server = tokio::spawn(async move {
        let (socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let (mut socket, first) = request(socket).await;
        respond(
            &mut socket,
            200,
            json!({"data":[{"url":image_url}]}).to_string().as_bytes(),
            "",
        )
        .await;
        drop(socket);
        let (socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let (mut socket, second) = request(socket).await;
        respond(&mut socket, 200, PNG, "").await;
        (first, second)
    });
    let (_temp, service) = service();
    let receipt = service
        .generate(
            generation(endpoint, ImageGenerationProtocol::OpenAiImages),
            Some("must-not-reach-download".into()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(receipt.state, ImageGenerationState::Succeeded);
    let (first, second) = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert!(first.headers.contains("must-not-reach-download"));
    assert!(!second.headers.contains("authorization:"));
    assert!(!second.headers.contains("cookie:"));
    assert!(second.line.starts_with("GET /result.png "));
}
#[tokio::test]
async fn after_dispatch_cancellation_is_durable_uncertainty_and_is_not_replayed() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (arrived, arrival) = tokio::sync::oneshot::channel();
    let (finish, finished) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let (_socket, record) = request(socket).await;
        let _ = arrived.send(());
        let _ = finished.await;
        record
    });
    let (_temp, service) = service();
    let request = generation(endpoint, ImageGenerationProtocol::OpenAiImages);
    let cancel = CancellationToken::new();
    let task = {
        let service = service.clone();
        let request = request.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            service
                .generate(request, Some("fixture-key".into()), cancel)
                .await
                .unwrap()
        })
    };
    tokio::time::timeout(Duration::from_secs(5), arrival)
        .await
        .unwrap()
        .unwrap();
    let active = service.receipt(request.id).unwrap().unwrap();
    assert_eq!(active.state, ImageGenerationState::Submitted);
    cancel.cancel();
    let receipt = task.await.unwrap();
    assert_eq!(receipt.state, ImageGenerationState::OutcomeUnknown);
    assert!(receipt.dispatch_started);
    let _ = finish.send(());
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    let again = service
        .generate(
            request,
            Some("fixture-key".into()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(again.state, ImageGenerationState::OutcomeUnknown);
}
#[tokio::test]
async fn predispatch_cancel_never_contacts_provider_and_restart_does_not_replay_submitted_job() {
    let (_temp, service) = service();
    let request = generation(
        "http://127.0.0.1:1".into(),
        ImageGenerationProtocol::OpenAiImages,
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    let receipt = service
        .generate(request.clone(), Some("fixture-key".into()), cancel)
        .await
        .unwrap();
    assert_eq!(receipt.state, ImageGenerationState::Cancelled);
    assert!(!receipt.dispatch_started);
    let mut job: Job =
        serde_json::from_slice(&fs::read(service.job_path(request.id)).unwrap()).unwrap();
    job.receipt.state = ImageGenerationState::Submitted;
    job.receipt.dispatch_started = true;
    atomic_json(&service.job_path(request.id), &job).unwrap();
    let recovered = service.receipt(request.id).unwrap().unwrap();
    assert_eq!(recovered.state, ImageGenerationState::OutcomeUnknown);
    assert!(
        service
            .generate(
                request,
                Some("fixture-key".into()),
                CancellationToken::new()
            )
            .await
            .unwrap()
            .images
            .is_empty()
    );
}
#[tokio::test]
async fn private_cross_origin_output_and_invalid_images_fail_without_automatic_retry() {
    for payload in [
        json!({"data":[{"url":"http://127.0.0.1:1/private.png"}]}),
        json!({"data":[{"b64_json":STANDARD.encode(b"not an image")}]}),
        json!({"data":[{"b64_json":"invalid-base64"}]}),
    ] {
        let (endpoint, server) = fixture(payload, 200).await;
        let (_temp, service) = service();
        let receipt = service
            .generate(
                generation(endpoint, ImageGenerationProtocol::OpenAiImages),
                Some("fixture-key".into()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(receipt.state, ImageGenerationState::Failed);
        assert!(receipt.images.is_empty());
        assert_eq!(
            fs::read_dir(service.artifacts.join("generated"))
                .unwrap()
                .count(),
            0
        );
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
    }
}
#[test]
fn provider_configuration_cannot_store_auth_headers_or_query_keys() {
    let (_temp, service) = service();
    let mut provider = generation(
        "https://images.example/v1".into(),
        ImageGenerationProtocol::OpenAiImages,
    )
    .provider;
    for header in ["Authorization", "Cookie", "x-api-key", "x-access-token"] {
        provider.headers.clear();
        provider
            .headers
            .insert(header.into(), "do-not-store".into());
        assert!(service.save_provider(&provider).is_err());
        assert!(!service.root.join("provider.json").exists());
    }
    provider.headers.clear();
    provider.endpoint = "https://images.example/v1?api_key=do-not-store".into();
    assert!(service.save_provider(&provider).is_err());
    provider.endpoint = "https://images.example/v1".into();
    service.save_provider(&provider).unwrap();
    assert_eq!(service.configured_provider().unwrap(), Some(provider));
}

#[tokio::test]
async fn durable_bounded_recent_history_recovers_uncertainty_after_restart_without_prompt_data() {
    let (temp, service) = service();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let (_socket, _) = request(socket).await;
    });
    let input = generation(endpoint, ImageGenerationProtocol::OpenAiImages);
    let id = input.id;
    let receipt = service
        .generate(input, Some("fixture-key".into()), CancellationToken::new())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.state, ImageGenerationState::OutcomeUnknown);
    drop(service);
    let reopened =
        ImageGenerationService::new(temp.path().join("data"), temp.path().join("data/artifacts"))
            .unwrap();
    let history = reopened.recent_receipts(16).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].id, id);
    assert_eq!(history[0].state, ImageGenerationState::OutcomeUnknown);
    assert!(reopened.recent_receipts(17).is_err());
    let index = fs::read_to_string(temp.path().join("data/image-generation/recent.json")).unwrap();
    assert!(!index.contains("prompt"));
    assert!(!index.contains("fixture-key"));
    assert!(index.len() < 4096);
}

#[tokio::test]
async fn oversized_native_http_response_and_output_download_are_rejected_before_buffering() {
    for output_download in [false, true] {
        let (_temp, service) = service();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let origin = endpoint.clone();
        let server = tokio::spawn(async move {
            let (socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let (mut socket, _) = request(socket).await;
            let mut oversized = 12 * 1024 * 1024 + 1;
            if output_download {
                respond(
                    &mut socket,
                    200,
                    json!({"data":[{"url":format!("{origin}/output.png")}]})
                        .to_string()
                        .as_bytes(),
                    "",
                )
                .await;
                drop(socket);
                let (next, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                    .await
                    .unwrap()
                    .unwrap();
                socket = request(next).await.0;
                oversized = workbench_core::MAX_IMAGE_BYTES as usize + 1;
            }
            socket.get_mut().write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {oversized}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        });
        let receipt = service
            .generate(
                generation(endpoint, ImageGenerationProtocol::OpenAiImages),
                Some("fixture-key".into()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(receipt.state, ImageGenerationState::Failed);
        assert!(receipt.images.is_empty());
        assert!(receipt.message.contains("size limit"));
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn explicit_anonymous_compatible_connection_is_allowed_but_missing_selected_credential_never_dispatches()
 {
    let (_temp, service) = service();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let mut input = generation(endpoint, ImageGenerationProtocol::OpenAiImages);
    let id = input.id;
    assert!(
        service
            .generate(input.clone(), None, CancellationToken::new())
            .await
            .unwrap_err()
            .to_string()
            .contains("credential")
    );
    assert!(service.receipt(id).unwrap().is_none());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
    input.provider.credential_ref = None;
    let mut anonymous_https = input.provider.clone();
    anonymous_https.endpoint = "https://explicit-compatible.example/v1".into();
    assert!(validate_provider(&anonymous_https).is_ok());
    anonymous_https.endpoint = "http://explicit-compatible.example/v1".into();
    assert!(validate_provider(&anonymous_https).is_err());
    let server = tokio::spawn(async move {
        let (socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let (mut socket, record) = request(socket).await;
        assert!(!record.headers.contains("authorization:"));
        respond(
            &mut socket,
            200,
            json!({"data":[{"b64_json":STANDARD.encode(PNG)}]})
                .to_string()
                .as_bytes(),
            "",
        )
        .await;
    });
    assert_eq!(
        service
            .generate(input, None, CancellationToken::new())
            .await
            .unwrap()
            .state,
        ImageGenerationState::Succeeded
    );
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn contended_durable_commit_does_not_block_async_runtime_and_cancel_prevents_dispatch() {
    let (_temp, service) = service();
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(service.root.join("recent.lock"))
        .unwrap();
    fs2::FileExt::lock_exclusive(&lock).unwrap();
    let cancel = CancellationToken::new();
    let task = {
        let service = service.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            service
                .generate(
                    generation(
                        "http://127.0.0.1:1".into(),
                        ImageGenerationProtocol::OpenAiImages,
                    ),
                    Some("fixture-key".into()),
                    cancel,
                )
                .await
        })
    };
    let started = std::time::Instant::now();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "Durable commit blocked the single async runtime thread"
    );
    cancel.cancel();
    drop(lock);
    let receipt = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(receipt.state, ImageGenerationState::Cancelled);
    assert!(!receipt.dispatch_started);
}

#[tokio::test]
async fn aborted_async_caller_retains_job_lease_until_started_durable_commit_finishes() {
    let (_temp, service) = service();
    let input = generation(
        "http://127.0.0.1:1".into(),
        ImageGenerationProtocol::OpenAiImages,
    );
    let id = input.id;
    let fingerprint = format!("{:x}", Sha256::digest(serde_json::to_vec(&input).unwrap()));
    let Preparation::Ready(lease, mut receipt) = service.prepare(&input, &fingerprint).unwrap()
    else {
        panic!("new request")
    };
    let lease = std::sync::Arc::new(lease);
    let index_lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(service.root.join("recent.lock"))
        .unwrap();
    fs2::FileExt::lock_exclusive(&index_lock).unwrap();
    receipt.state = ImageGenerationState::Submitted;
    receipt.dispatch_started = true;
    let task = {
        let service = service.clone();
        let lease = lease.clone();
        tokio::spawn(async move {
            service
                .persist_async(&fingerprint, &mut receipt, &lease)
                .await
        })
    };
    // The JSON marker is written before the deliberately contended index phase.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let job: Job =
                serde_json::from_slice(&fs::read(service.job_path(id)).unwrap()).unwrap();
            if job.receipt.state == ImageGenerationState::Submitted {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    drop(lease);
    assert_eq!(
        service.receipt(id).unwrap().unwrap().state,
        ImageGenerationState::Submitted,
        "Status must not recover a writer whose blocking commit is still running"
    );
    drop(index_lock);
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if service.try_lease(id).unwrap().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        service.receipt(id).unwrap().unwrap().state,
        ImageGenerationState::OutcomeUnknown
    );
}
