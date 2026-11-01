//! Search JSON fixtures exercise the actual HTTP adapters without accounts.
use super::*;
use crate::providers::tests::{fixture, provider};

#[tokio::test]
async fn native_search_requests_and_citations() {
    let cases = [
        (
            ApiFamily::Anthropic,
            json!({"content":[{"type":"web_search_tool_result","content":[{"type":"web_search_result","title":"Docs","url":"https://example.com/docs","page_age":"today"}]},{"type":"text","text":"Answer","citations":[{"type":"web_search_result_location","url":"https://example.com/docs","title":"Docs","cited_text":"Snippet"}]}]}),
        ),
        (
            ApiFamily::OpenAiResponses,
            json!({"output":[{"type":"web_search_call","action":{"sources":[{"type":"url","url":"https://example.com/docs"}]}},{"type":"message","content":[{"type":"output_text","text":"Answer","annotations":[{"type":"url_citation","url":"https://example.com/docs","title":"Docs"}]}]}]}),
        ),
        (
            ApiFamily::Gemini,
            json!({"candidates":[{"content":{"parts":[{"text":"Answer"}]},"groundingMetadata":{"groundingChunks":[{"web":{"uri":"https://example.com/docs","title":"Docs"}}],"groundingSupports":[{"segment":{"text":"Answer"},"groundingChunkIndices":[0]}]}}]}),
        ),
    ];
    for (family, payload) in cases {
        let (endpoint, server) = fixture(
            serde_json::to_vec(&payload).unwrap(),
            200,
            "application/json",
        )
        .await;
        let address = url::Url::parse(&endpoint)
            .unwrap()
            .socket_addrs(|| None)
            .unwrap();
        let adapter = provider(family.clone(), endpoint);
        let result = adapter
            .search(json!({"query":"docs"}), address, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(result["results"][0]["url"], "https://example.com/docs");
        assert!(!result["citations"].as_array().unwrap().is_empty());
        let sent = server.await.unwrap();
        match family {
            ApiFamily::Anthropic => {
                assert_eq!(sent.body["tools"][0]["type"], "web_search_20250305");
                assert_eq!(sent.body["stream"], false);
            }
            ApiFamily::OpenAiResponses => {
                assert_eq!(sent.body["tools"][0]["type"], "web_search");
                assert_eq!(sent.body["stream"], false);
            }
            ApiFamily::Gemini => {
                assert!(sent.body["tools"][0].get("google_search").is_some());
                assert!(sent.line.contains(":generateContent"));
                assert!(!sent.line.contains("streamGenerateContent"));
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn filtered_search_drops_disallowed_sources_and_keeps_ordinary_requests_unchanged() {
    let provider = provider(ApiFamily::OpenAiResponses, "http://127.0.0.1:1".into());
    let args=SearchArguments::parse(&json!({"query":"docs","allowed_domains":["example.com"],"blocked_domains":["bad.example.com"]})).unwrap();
    let search = search_request(&provider, &args).unwrap();
    assert_eq!(
        search["tools"][0]["filters"]["allowed_domains"],
        json!(["example.com"])
    );
    let ordinary = request::body(
        &provider,
        &[Message::user("hello")],
        &[],
        128,
        Reasoning::Lowest,
    )
    .unwrap();
    assert!(ordinary["tools"].as_array().is_none_or(Vec::is_empty));
    let parsed=parse(ProviderProtocol::Responses,&json!({"output":[{"content":[{"text":"potential mixed answer","annotations":[{"url":"https://example.com/a","title":"good"},{"url":"https://bad.example.com/a","title":"bad"},{"url":"https://evil.test/a","title":"evil"}]}]}]}),&args).unwrap();
    assert_eq!(parsed["results"].as_array().unwrap().len(), 1);
    assert_eq!(parsed["citations"].as_array().unwrap().len(), 1);
    assert_eq!(parsed["answer"], "");
}

#[test]
fn anthropic_search_tool_errors_are_failures() {
    let args = SearchArguments::parse(&json!({"query":"x"})).unwrap();
    assert!(parse(ProviderProtocol::AnthropicMessages,&json!({"content":[{"type":"web_search_tool_result","content":{"type":"web_search_tool_result_error","error_code":"unavailable"}}]}),&args).is_err());
}
