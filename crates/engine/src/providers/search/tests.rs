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
    for (family, mut payload) in cases {
        let usage = match family {
            ApiFamily::Anthropic => {
                json!({"input_tokens":12,"output_tokens":7,"cache_read_input_tokens":3,"cache_creation_input_tokens":2,"server_tool_use":{"web_search_requests":1}})
            }
            ApiFamily::OpenAiResponses => {
                json!({"input_tokens":12,"output_tokens":7,"input_tokens_details":{"cached_tokens":3},"output_tokens_details":{"reasoning_tokens":2},"total_tokens":19})
            }
            ApiFamily::Gemini => {
                json!({"promptTokenCount":12,"candidatesTokenCount":7,"thoughtsTokenCount":2,"cachedContentTokenCount":3,"toolUsePromptTokenCount":1,"totalTokenCount":21})
            }
            _ => unreachable!(),
        };
        if family == ApiFamily::Gemini {
            payload["usageMetadata"] = usage.clone();
            payload["modelVersion"] = json!("reported-model");
        } else {
            payload["usage"] = usage.clone();
            payload["model"] = json!("reported-model");
        }
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
        assert_eq!(result["model"], "reported-model");
        assert_eq!(result["requested_model"], "fixture-model");
        assert_eq!(result["usage"], usage);
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

#[test]
fn gpt5_search_uses_lowest_search_compatible_effort_without_changing_extraction() {
    let args = SearchArguments::parse(&json!({"query":"docs"})).unwrap();
    for model in ["gpt-5", "gpt-5-2025-08-07", "gpt-5-mini", "gpt-5-nano"] {
        let mut adapter = provider(ApiFamily::OpenAiResponses, "http://127.0.0.1:1".into());
        adapter.config.model = model.into();
        adapter.config.reasoning_options = Some(vec![
            "high".into(),
            "minimal".into(),
            "medium".into(),
            "low".into(),
        ]);
        assert_eq!(
            search_request(&adapter, &args).unwrap()["reasoning"]["effort"],
            "low"
        );
        let extraction = request::body(
            &adapter,
            &[Message::user("extract")],
            &[],
            128,
            Reasoning::Lowest,
        )
        .unwrap();
        assert_eq!(extraction["reasoning"]["effort"], "minimal");
    }
}

#[test]
fn openai_snippets_only_include_the_cited_range() {
    let args =
        SearchArguments::parse(&json!({"query":"docs","allowed_domains":["good.test"]})).unwrap();
    let payload = json!({"output":[{"content":[{"text":"Allowed source. Blocked source.","annotations":[{"url":"https://good.test/a","title":"Good","start_index":0,"end_index":15},{"url":"https://bad.test/a","title":"Bad","start_index":16,"end_index":31}]}]}]});
    let parsed = parse(ProviderProtocol::Responses, &payload, &args).unwrap();
    assert_eq!(parsed["results"].as_array().unwrap().len(), 1);
    assert_eq!(parsed["results"][0]["snippet"], "Allowed source.");
    assert!(!parsed.to_string().contains("Blocked source."));
    let missing_range = json!({"output":[{"content":[{"text":"Mixed answer","annotations":[{"url":"https://good.test/a","title":"Good"}]}]}]});
    assert_eq!(
        parse(ProviderProtocol::Responses, &missing_range, &args).unwrap()["results"][0]["snippet"],
        ""
    );
}

#[test]
fn gemini_defers_publisher_filters_for_documented_grounding_redirects() {
    let redirect = "https://vertexaisearch.cloud.google.com/grounding-api-redirect/signed-source";
    for filters in [
        json!({"query":"docs","allowed_domains":["publisher.test"]}),
        json!({"query":"docs","blocked_domains":["publisher.test"]}),
    ] {
        let args = SearchArguments::parse(&filters).unwrap();
        let payload = json!({"candidates":[{"content":{"parts":[{"text":"Mixed answer"}]},"groundingMetadata":{"groundingChunks":[{"web":{"uri":redirect,"title":"publisher.test"}}]}}]});
        let parsed = parse(ProviderProtocol::GeminiGenerateContent, &payload, &args).unwrap();
        assert_eq!(parsed["results"][0]["url"], redirect);
        assert_eq!(parsed["source_resolution"], "gemini_grounding");
        assert_eq!(parsed["answer"], "");
    }
}

#[test]
fn search_does_not_invent_an_unadvertised_compatible_effort() {
    let mut adapter = provider(ApiFamily::OpenAiResponses, "http://127.0.0.1:1".into());
    adapter.config.model = "gpt-5".into();
    adapter.config.reasoning_options = Some(vec!["minimal".into()]);
    let args = SearchArguments::parse(&json!({"query":"docs"})).unwrap();
    assert!(
        search_request(&adapter, &args)
            .unwrap_err()
            .to_string()
            .contains("other than minimal")
    );
}

#[test]
fn citation_ranges_handle_unicode_and_invalid_indices_without_broad_fallbacks() {
    let text = "é Good. Bad.";
    let boundaries: Vec<_> = text
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(text.len()))
        .collect();
    assert_eq!(
        cited_range(text, &boundaries, &json!({"start_index":0,"end_index":7})),
        "é Good."
    );
    for annotation in [
        json!({}),
        json!({"start_index":-1,"end_index":3}),
        json!({"start_index":5,"end_index":2}),
        json!({"start_index":0,"end_index":99999}),
    ] {
        assert_eq!(cited_range(text, &boundaries, &annotation), "");
    }
}
