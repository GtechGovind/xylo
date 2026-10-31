use super::*;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// A loopback server answering by request path; unknown paths get 404.
async fn server(routes: Vec<(&'static str, Value)>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let routes = routes.clone();
            tokio::spawn(async move {
                let mut socket = BufReader::new(socket);
                let mut request = String::new();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    if socket.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse::<usize>().unwrap_or(0);
                    }
                    if line == "\r\n" {
                        break;
                    }
                    request.push_str(&line);
                }
                let mut body = vec![0; length];
                let _ = socket.read_exact(&mut body).await;
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let (status, payload) = routes
                    .iter()
                    .find(|(route, _)| *route == path)
                    .map_or((404, json!({"error":"not found"})), |(_, value)| {
                        (200, value.clone())
                    });
                let payload = payload.to_string();
                let _ = socket
                    .get_mut()
                    .write_all(
                        format!(
                            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                            payload.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            });
        }
    });
    format!("http://{address}/v1")
}

fn config(endpoint: &str, model: &str) -> ProviderConfig {
    ProviderConfig {
        endpoint: endpoint.into(),
        model: model.into(),
        context_tokens: 131_072,
        ..Default::default()
    }
}

#[tokio::test]
async fn llama_cpp_reports_its_per_slot_context() {
    let endpoint = server(vec![(
        "/props",
        json!({"default_generation_settings":{"n_ctx":4096},"total_slots":1}),
    )])
    .await;
    let served = served_context_tokens(&config(&endpoint, "any"), &CancellationToken::new()).await;
    assert_eq!(served, Some(4096));
}

#[tokio::test]
async fn ollama_reports_the_loaded_context_then_the_num_ctx_parameter() {
    let endpoint = server(vec![
        (
            "/api/ps",
            json!({"models":[{"name":"qwen3:8b","model":"qwen3:8b","context_length":4096}]}),
        ),
        (
            "/api/show",
            json!({"parameters":"stop \"<|im_end|>\"\nnum_ctx 16384","model_info":{"qwen3.context_length":40960}}),
        ),
    ])
    .await;
    let cancel = CancellationToken::new();
    assert_eq!(
        served_context_tokens(&config(&endpoint, "qwen3:8b"), &cancel).await,
        Some(4096)
    );
    let limits = served_context_limits(
        &config(&endpoint, "qwen3:8b"),
        &["qwen3:8b".into(), "llama3".into()],
        &cancel,
    )
    .await;
    assert_eq!(limits.get("qwen3:8b"), Some(&4096));
    // Not loaded: the model's own num_ctx parameter, never the trained maximum.
    assert_eq!(limits.get("llama3"), Some(&16384));
}

#[tokio::test]
async fn lm_studio_reports_only_loaded_models() {
    let endpoint = server(vec![(
        "/api/v0/models",
        json!({"data":[
            {"id":"loaded","state":"loaded","max_context_length":32768,"loaded_context_length":8192},
            {"id":"idle","state":"not-loaded","max_context_length":32768}
        ]}),
    )])
    .await;
    let limits = served_context_limits(
        &config(&endpoint, "loaded"),
        &["loaded".into(), "idle".into()],
        &CancellationToken::new(),
    )
    .await;
    assert_eq!(limits, HashMap::from([("loaded".to_owned(), 8192)]));
}

#[tokio::test]
async fn remote_and_silent_servers_report_nothing() {
    let cancel = CancellationToken::new();
    assert_eq!(
        served_context_tokens(&config("https://api.example.com/v1", "m"), &cancel).await,
        None
    );
    let silent = server(Vec::new()).await;
    assert_eq!(
        served_context_tokens(&config(&silent, "m"), &cancel).await,
        None
    );
}

#[test]
fn parsers_ignore_implausible_values() {
    assert_eq!(parse_llama_props(&json!({"n_ctx": 8192})), Some(8192));
    assert_eq!(parse_llama_props(&json!({"n_ctx": 0})), None);
    assert_eq!(
        parse_ollama_num_ctx(&json!({"parameters":"num_ctx abc"})),
        None
    );
    assert_eq!(parse_ollama_num_ctx(&json!({})), None);
    assert!(parse_ollama_ps(&json!({"models":[{"name":"x"}]})).is_empty());
    assert_eq!(
        local_root("http://localhost:1234/v1/chat?x=1")
            .unwrap()
            .as_str(),
        "http://localhost:1234/"
    );
    assert!(local_root("https://example.com/v1").is_none());
}

#[tokio::test]
async fn discovery_lowers_catalog_context_to_the_served_context() {
    let endpoint = server(vec![(
        "/props",
        json!({"default_generation_settings":{"n_ctx":8192}}),
    )])
    .await;
    let mut models = vec![
        super::super::ProviderModel {
            id: "large".into(),
            display_name: "large".into(),
            context_tokens: Some(131_072),
            reasoning_options: None,
            default_reasoning_effort: None,
        },
        super::super::ProviderModel {
            id: "unknown".into(),
            display_name: "unknown".into(),
            context_tokens: None,
            reasoning_options: None,
            default_reasoning_effort: None,
        },
    ];
    super::super::discovery::apply_served_context(
        &config(&endpoint, "large"),
        &mut models,
        &CancellationToken::new(),
    )
    .await;
    assert_eq!(models[0].context_tokens, Some(8192));
    assert_eq!(models[1].context_tokens, Some(8192));
}

#[tokio::test]
async fn a_cache_reuses_answers_and_asks_again_after_silence() {
    let (endpoint, hits) =
        counting_server(json!({"default_generation_settings":{"n_ctx":2048}})).await;
    let cancel = CancellationToken::new();
    let cache = ServedContextCache::default();
    let first = config(&endpoint, "cached");
    assert_eq!(
        cache.served_context_tokens(&first, &cancel).await,
        Some(2048)
    );
    assert_eq!(
        cache.served_context_tokens(&first, &cancel).await,
        Some(2048)
    );
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    // Another model, or another cache (another engine), probes on its own.
    let other = config(&endpoint, "other");
    assert_eq!(
        cache.served_context_tokens(&other, &cancel).await,
        Some(2048)
    );
    let fresh = ServedContextCache::default();
    assert_eq!(
        fresh.served_context_tokens(&first, &cancel).await,
        Some(2048)
    );
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
    // A server that says nothing is asked again rather than remembered.
    let silent = server(Vec::new()).await;
    let quiet = config(&silent, "m");
    assert_eq!(cache.served_context_tokens(&quiet, &cancel).await, None);
    assert_eq!(cache.served_context_tokens(&quiet, &cancel).await, None);
    assert!(
        cache
            .0
            .lock()
            .unwrap()
            .get(&(silent, "m".to_owned()))
            .is_none()
    );
}

/// A server answering `/props` and counting the requests it gets.
async fn counting_server(props: Value) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = hits.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let payload = props.to_string();
            tokio::spawn(async move {
                let mut socket = BufReader::new(socket);
                loop {
                    let mut line = String::new();
                    if socket.read_line(&mut line).await.unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                let _ = socket
                    .get_mut()
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                            payload.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            });
        }
    });
    (format!("http://{address}/v1"), hits)
}
