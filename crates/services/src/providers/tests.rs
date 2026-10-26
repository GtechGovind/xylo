use super::*;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn config(family: ApiFamily, endpoint: &str) -> ProviderConfig {
    ProviderConfig {
        family,
        endpoint: endpoint.into(),
        model: "chosen-model".into(),
        ..Default::default()
    }
}

#[test]
fn credential_query_aliases_cannot_enter_saved_provider_endpoints() {
    for name in [
        "apiKey",
        "API-KEY",
        "api_key",
        "api%4Bey",
        "token",
        "accessToken",
        "refresh_token",
        "client-secret",
        "password",
        "Authorization",
        "signature",
        "sig",
        "X-Amz-Signature",
        "X-Goog-Credential",
    ] {
        let endpoint = format!("https://provider.example/v1?{name}=private-value");
        let error = normalize_provider(&config(ApiFamily::OpenAiChat, &endpoint)).unwrap_err();
        assert!(!error.to_string().contains("private-value"));
    }
    let endpoint = normalize_provider(&config(
        ApiFamily::Azure,
        "https://provider.example/openai/v1?api-version=2024-10-21&region=west",
    ))
    .unwrap();
    assert!(
        endpoint
            .inference_url
            .query()
            .unwrap()
            .contains("api-version=2024-10-21")
    );
}

#[test]
fn native_endpoint_normalization_never_sends_a_different_protocol() {
    for (family, base, path) in [
        (
            ApiFamily::OpenAiChat,
            "https://api.openai.com",
            "/v1/chat/completions",
        ),
        (
            ApiFamily::OpenAiResponses,
            "https://api.openai.com/v1/responses",
            "/v1/responses",
        ),
        (
            ApiFamily::Anthropic,
            "https://api.anthropic.com/v1/",
            "/v1/messages",
        ),
        (
            ApiFamily::Azure,
            "https://example.openai.azure.com/",
            "/openai/v1/chat/completions",
        ),
        (
            ApiFamily::Gemini,
            "https://generativelanguage.googleapis.com",
            "/v1beta/models/chosen-model:generateContent",
        ),
        (
            ApiFamily::Vertex,
            "https://us-central1-aiplatform.googleapis.com/v1/projects/p/locations/us-central1/publishers/google",
            "/v1/projects/p/locations/us-central1/publishers/google/models/chosen-model:generateContent",
        ),
    ] {
        assert_eq!(
            normalize_provider(&config(family, base))
                .unwrap()
                .inference_url
                .path(),
            path
        );
    }
    let azure = normalize_provider(&config(
        ApiFamily::Azure,
        "https://example.openai.azure.com/openai/deployments/deployment?api-version=2024-10-21",
    ))
    .unwrap();
    assert_eq!(azure.inference_url.query(), Some("api-version=2024-10-21"));
    assert!(azure.models_url.is_none());
    assert!(
        normalize_provider(&config(
            ApiFamily::Azure,
            "https://example.openai.azure.com/openai/deployments/deployment"
        ))
        .is_err()
    );
    assert!(
        normalize_provider(&config(
            ApiFamily::Vertex,
            "https://aiplatform.googleapis.com"
        ))
        .is_err()
    );
    assert!(
        normalize_provider(&config(
            ApiFamily::OpenAiResponses,
            "https://chatgpt.com/backend-api/codex"
        ))
        .is_err()
    );
    assert!(
        normalize_provider(&config(
            ApiFamily::OpenAiChat,
            "https://user:secret@example.com/v1"
        ))
        .is_err()
    );
    assert!(
        normalize_provider(&config(ApiFamily::Gemini, "https://example.com?key=secret")).is_err()
    );
    let bedrock = normalize_provider(&config(
        ApiFamily::Bedrock,
        "https://bedrock-runtime.eu-west-1.amazonaws.com",
    ))
    .unwrap();
    assert_eq!(bedrock.aws_region.as_deref(), Some("eu-west-1"));
    assert_eq!(
        bedrock.models_url.unwrap().host_str(),
        Some("bedrock.eu-west-1.amazonaws.com")
    );
}

#[test]
fn credential_auth_modes_are_explicit_expiring_and_not_plaintext_headers() {
    let key = ProviderCredential::parse(Some("api-test".into())).unwrap();
    let headers = key
        .headers(&ApiFamily::Gemini, &Default::default())
        .unwrap();
    assert_eq!(headers["x-goog-api-key"], "api-test");
    assert!(!headers.contains_key("authorization"));
    let headers = key.headers(&ApiFamily::Azure, &Default::default()).unwrap();
    assert_eq!(headers["api-key"], "api-test");
    assert!(!headers.contains_key("authorization"));
    let bearer = ProviderCredential::parse(Some(
        json!({"access_token":"oauth-test","expires_at_ms":now_ms()+60000}).to_string(),
    ))
    .unwrap();
    assert_eq!(
        bearer
            .headers(&ApiFamily::Vertex, &Default::default())
            .unwrap()["authorization"],
        "Bearer oauth-test"
    );
    assert!(
        ProviderCredential::parse(Some(
            json!({"access_token":"expired","expires_at_ms":1}).to_string()
        ))
        .is_err()
    );
    assert!(
        key.headers(
            &ApiFamily::Anthropic,
            &BTreeMap::from([("x-api-key".into(), "plaintext".into())])
        )
        .is_err()
    );
}

#[tokio::test]
async fn gemini_discovery_uses_native_auth_filters_and_pagination() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for page in 0..2 {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                socket.read_line(&mut line).await.unwrap();
                headers.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            assert!(headers.starts_with("GET /v1beta/models"));
            assert!(
                headers
                    .to_lowercase()
                    .contains("x-goog-api-key: fixture-key")
            );
            if page == 1 {
                assert!(headers.contains("pageToken=page-two"));
            }
            let body=if page==0{json!({"models":[{"name":"models/embed-only","supportedGenerationMethods":["embedContent"]},{"name":"models/real-chat","displayName":"Real chat","supportedGenerationMethods":["generateContent"]}],"nextPageToken":"page-two"})}else{json!({"models":[{"name":"models/second-chat","supportedGenerationMethods":["generateContent"]}]})}.to_string();
            socket.get_mut().write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).as_bytes()).await.unwrap();
        }
    });
    let config = config(ApiFamily::Gemini, &format!("http://{address}"));
    let models = super::discovery::discover_with_credential(
        &config,
        &ProviderCredential::ApiKey("fixture-key".into()),
        &Default::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        ["real-chat", "second-chat"]
    );
    server.await.unwrap();
}

#[tokio::test]
async fn bedrock_catalog_uses_control_plane_signed_or_bearer_auth_and_lists_inference_profiles() {
    for signed in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for page in 0..3 {
                let (socket, _) = listener.accept().await.unwrap();
                let mut socket = BufReader::new(socket);
                let mut request = String::new();
                loop {
                    let mut line = String::new();
                    socket.read_line(&mut line).await.unwrap();
                    request.push_str(&line);
                    if line == "\r\n" {
                        break;
                    }
                }
                let request = request.to_lowercase();
                if signed {
                    assert!(
                        request.contains("authorization: aws4-hmac-sha256 credential=fixture-id/")
                    );
                    assert!(request.contains("x-amz-security-token: fixture-session"));
                    assert!(!request.contains("fixture-secret"));
                } else {
                    assert!(request.contains("authorization: bearer fixture-key"));
                }
                if page == 0 {
                    assert!(request.starts_with("get /foundation-models "));
                } else {
                    assert!(request.starts_with("get /inference-profiles"));
                }
                if page == 2 {
                    assert!(request.contains("nexttoken=page%2btwo"));
                }
                let body=match page {
                    0=>json!({"modelSummaries":[{"modelId":"text-model","modelName":"Text model","outputModalities":["TEXT"],"responseStreamingSupported":true},{"modelId":"image-only","outputModalities":["IMAGE"]}]}),
                    1=>json!({"inferenceProfileSummaries":[{"inferenceProfileId":"us.text-model","inferenceProfileName":"US model","status":"ACTIVE"}],"nextToken":"page+two"}),
                    _=>json!({"inferenceProfileSummaries":[{"inferenceProfileId":"old-profile","status":"INACTIVE"},{"inferenceProfileId":"app-profile","status":"ACTIVE"}]})
                }.to_string();
                socket.get_mut().write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            }
        });
        let credential = if signed {
            ProviderCredential::Aws {
                access_key_id: "fixture-id".into(),
                secret_access_key: "fixture-secret".into(),
                session_token: Some("fixture-session".into()),
            }
        } else {
            ProviderCredential::ApiKey("fixture-key".into())
        };
        let models = super::discovery::discover_with_credential(
            &config(ApiFamily::Bedrock, &format!("http://{address}")),
            &credential,
            &Default::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["text-model", "us.text-model", "app-profile"]
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn custom_gateway_discovery_preserves_base_path_and_filters_explicit_api_support() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = BufReader::new(socket);
        let mut headers = String::new();
        loop {
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            headers.push_str(&line);
            if line == "\r\n" {
                break;
            }
        }
        assert!(headers.starts_with("GET /api/v1/llm/models HTTP/1.1"));
        assert!(
            headers
                .to_lowercase()
                .contains("authorization: bearer fixture-token")
        );
        let body = json!({"object":"list","data":[
            {"id":"chat-model","supported_apis":["chatCompletions","responses"],"metadata":{"context_length":8192}},
            {"id":"response-only","supported_apis":["responses"]},
            {"id":"plain-compatible-model"}
        ]}).to_string();
        socket.get_mut().write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
    });
    let config = config(
        ApiFamily::OpenAiChat,
        &format!("http://{address}/api/v1/llm"),
    );
    assert_eq!(
        normalize_provider(&config).unwrap().inference_url.path(),
        "/api/v1/llm/chat/completions"
    );
    let models = super::discovery::discover_with_credential(
        &config,
        &ProviderCredential::ApiKey("fixture-token".into()),
        &Default::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        ["chat-model", "plain-compatible-model"]
    );
    assert_eq!(models[0].context_tokens, Some(8192));
    server.await.unwrap();
}

#[test]
fn provider_setup_allows_discovery_before_selecting_a_model() {
    let provider = ProviderConfig::default();
    assert!(provider.validate_connection().is_ok());
    assert!(provider.validate().is_err());
}

#[test]
fn saved_models_survive_switches_and_roundtrip_without_duplicates() {
    let mut settings = workbench_core::AppSettings::default();
    let first = config(ApiFamily::OpenAiChat, "https://gateway.example/v1");
    let mut second = first.clone();
    second.model = "second-model".into();
    settings.remember_model(first.clone());
    settings.remember_model(second.clone());
    settings.remember_model(first.clone());
    settings.provider = second;
    let saved = serde_json::to_vec(&settings).unwrap();
    let restored: workbench_core::AppSettings = serde_json::from_slice(&saved).unwrap();
    assert_eq!(restored.providers.len(), 2);
    assert_eq!(restored.provider.model, "second-model");
    assert_eq!(restored.providers[0].model, first.model);
}
