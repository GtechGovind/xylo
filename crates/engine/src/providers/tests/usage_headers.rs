use super::*;
use crate::model::StreamEvent;

#[tokio::test]
async fn request_metadata_preserves_model_and_only_numeric_rate_limit_headers() {
    let mut payload = sse(&[
        json!({"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":2}}),
    ]);
    payload.extend_from_slice(b"data: [DONE]\r\n\r\n");
    let (endpoint, server) = fixture(payload, 200, "text/event-stream\r\nx-ratelimit-remaining-requests: 5\r\nx-ratelimit-remaining-tokens: not-a-number\r\nanthropic-ratelimit-input-tokens-remaining: 90\r\nx-private-account: secret-value").await;
    let provider = provider(ApiFamily::OpenAiChat, endpoint);
    let mut stream = provider.stream(
        ModelRequest {
            messages: vec![Message::user("hello")],
            tools: Vec::new(),
            max_tokens: 100,
            reasoning: crate::model::Reasoning::Configured,
        },
        CancellationToken::new(),
    );
    let first = stream.next().await.unwrap().unwrap();
    assert!(!format!("{first:?}").contains("secret-value"));
    let StreamEvent::Metadata {
        model,
        rate_limits: Some(limits),
    } = first
    else {
        panic!("Missing provider request metadata");
    };
    assert_eq!(model, "fixture-model");
    assert_eq!(limits.requests, Some(5));
    assert_eq!(limits.tokens, None);
    assert_eq!(limits.input_tokens, Some(90));
    let mut response = Assembler::default();
    while let Some(event) = stream.next().await {
        response.push(event.unwrap());
    }
    assert_eq!(response.finish().unwrap().message.content, "done");
    server.await.unwrap();
}
