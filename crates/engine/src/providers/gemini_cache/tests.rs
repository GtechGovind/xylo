//! Explicit cache wire tests with isolated local HTTP fixtures.
use super::*;
use crate::model::{Message, Reasoning};
use crate::providers::tests::{fixture, provider};

#[tokio::test]
async fn long_prefix_is_cached_and_reused_without_resending_system_or_tools() {
    let (url, recorded) = fixture(
        serde_json::to_vec(&json!({"name":"cachedContents/fixture"})).unwrap(),
        200,
        "application/json",
    )
    .await;
    let provider = provider(ApiFamily::Gemini, url);
    let messages = vec![
        Message::system("x".repeat(33000)),
        Message::user("first"),
        Message::assistant_with_tool_calls("reply", vec![]),
        Message::user("last"),
    ];
    let tools = vec![crate::model::ToolSchema {
        name: "lookup".into(),
        description: "Look up".into(),
        parameters: json!({"type":"object","properties":{}}),
    }];
    let body = request::body(&provider, &messages, &tools, 8192, Reasoning::Configured).unwrap();
    let cached = provider.cached_body(&body).await.unwrap();
    let sent = recorded.await.unwrap();
    assert!(sent.line.starts_with("POST /v1beta/cachedContents "));
    assert_eq!(sent.body["ttl"], "300s");
    assert_eq!(sent.body["tools"], body["tools"]);
    assert!(cached.get("tools").is_none());
    assert_eq!(cached["cachedContent"], "cachedContents/fixture");
    assert_eq!(
        cached["contents"],
        json!([body["contents"].as_array().unwrap().last().unwrap()])
    );
    assert!(cached.get("systemInstruction").is_none());
    assert_eq!(provider.cached_body(&body).await, Some(cached));
}

#[tokio::test]
async fn cache_rejection_and_short_prefix_use_the_original_request() {
    let (url, recorded) = fixture(b"{}".to_vec(), 400, "application/json").await;
    let provider = provider(ApiFamily::Gemini, url);
    let short = request::body(
        &provider,
        &[Message::user("hi")],
        &[],
        8192,
        Reasoning::Configured,
    )
    .unwrap();
    assert!(provider.cached_body(&short).await.is_none());
    let long = request::body(
        &provider,
        &[
            Message::system("x".repeat(33000)),
            Message::user("one"),
            Message::assistant_with_tool_calls("two", vec![]),
            Message::user("three"),
        ],
        &[],
        8192,
        Reasoning::Configured,
    )
    .unwrap();
    assert!(provider.cached_body(&long).await.is_none());
    recorded.await.unwrap();
}

/// A sequence records requests on one origin, including cache invalidation retries.
async fn sequence(replies: Vec<(u16, Vec<u8>)>) -> (String, tokio::task::JoinHandle<Vec<Value>>) {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let recorded = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (status, payload) in replies {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            let mut length = 0;
            loop {
                let mut line = String::new();
                socket.read_line(&mut line).await.unwrap();
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse::<usize>().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
            }
            let mut bytes = vec![0; length];
            socket.read_exact(&mut bytes).await.unwrap();
            requests.push(serde_json::from_slice(&bytes).unwrap());
            socket.get_mut().write_all(format!("HTTP/1.1 {status} Fixture\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",payload.len()).as_bytes()).await.unwrap();
            socket.get_mut().write_all(&payload).await.unwrap();
        }
        requests
    });
    (url, recorded)
}

fn long_messages() -> Vec<Message> {
    vec![
        Message::system("x".repeat(33000)),
        Message::user("first"),
        Message::assistant("reply"),
        Message::user("last"),
    ]
}

#[tokio::test]
async fn rejected_cache_reference_retries_the_original_request_once_and_invalidates() {
    let answer = format!(
        "data: {}\n\n",
        json!({"candidates":[{"content":{"parts":[{"text":"Answer"}]},"finishReason":"STOP"}]})
    )
    .into_bytes();
    let (url, recorded) = sequence(vec![
        (
            200,
            serde_json::to_vec(&json!({"name":"cachedContents/expired"})).unwrap(),
        ),
        (
            400,
            br#"{"error":{"status":"INVALID_ARGUMENT","message":"Cache expired"}}"#.to_vec(),
        ),
        (200, answer),
    ])
    .await;
    let provider = provider(ApiFamily::Gemini, url);
    let response = provider
        .complete(
            ModelRequest {
                messages: long_messages(),
                tools: vec![],
                max_tokens: 8192,
                reasoning: Reasoning::Configured,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.message.content, "Answer");
    let requests = recorded.await.unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1]["cachedContent"], "cachedContents/expired");
    assert!(requests[2].get("cachedContent").is_none());
    assert!(requests[2].get("systemInstruction").is_some());
    assert!(provider.prefix_cache.0.lock().await.is_empty());
}

#[tokio::test]
async fn locally_expired_prefix_is_created_again_and_initial_system_prefix_can_be_cached() {
    let (url, recorded) = sequence(vec![
        (
            200,
            serde_json::to_vec(&json!({"name":"cachedContents/first"})).unwrap(),
        ),
        (
            200,
            serde_json::to_vec(&json!({"name":"cachedContents/second"})).unwrap(),
        ),
    ])
    .await;
    let provider = provider(ApiFamily::Gemini, url);
    let body = request::body(
        &provider,
        &[Message::system("x".repeat(33000)), Message::user("Hello")],
        &[],
        8192,
        Reasoning::Configured,
    )
    .unwrap();
    assert_eq!(
        provider.cached_body(&body).await.unwrap()["cachedContent"],
        "cachedContents/first"
    );
    for (_, expiry) in provider.prefix_cache.0.lock().await.values_mut() {
        *expiry = Instant::now() - Duration::from_secs(1);
    }
    assert_eq!(
        provider.cached_body(&body).await.unwrap()["cachedContent"],
        "cachedContents/second"
    );
    assert_eq!(recorded.await.unwrap().len(), 2);
}
