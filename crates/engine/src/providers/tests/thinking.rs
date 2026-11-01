//! Request snapshots for thinking controls across provider families.
use super::*;
use crate::model::Reasoning;

#[test]
fn thinking_request_snapshots() {
    let cases = [
        (
            ApiFamily::Anthropic,
            "claude-sonnet-4-6",
            "low",
            json!({"type":"adaptive"}),
            json!({"effort":"low"}),
        ),
        (
            ApiFamily::Anthropic,
            "claude-opus-4-5",
            "high",
            json!({"type":"enabled","budget_tokens":16384}),
            json!({"effort":"high"}),
        ),
        (
            ApiFamily::Anthropic,
            "claude-sonnet-4-5",
            "medium",
            json!({"type":"enabled","budget_tokens":8192}),
            Value::Null,
        ),
        (
            ApiFamily::Gemini,
            "gemini-2.5-pro",
            "low",
            json!({"thinkingBudget":1024,"includeThoughts":true}),
            Value::Null,
        ),
        (
            ApiFamily::Gemini,
            "gemini-3-pro-preview",
            "high",
            json!({"thinkingLevel":"high","includeThoughts":true}),
            Value::Null,
        ),
    ];
    for (family, model, effort, expected, output) in cases {
        let mut provider = provider(family.clone(), "http://127.0.0.1:8080".into());
        provider.config.model = model.into();
        provider.config.reasoning_effort = Some(effort.into());
        let body = request::body(
            &provider,
            &[Message::user("Hello")],
            &[],
            32768,
            Reasoning::Configured,
        )
        .unwrap();
        let actual = if family == ApiFamily::Gemini {
            &body["generationConfig"]["thinkingConfig"]
        } else {
            &body["thinking"]
        };
        assert_eq!(actual, &expected, "{model}/{effort}");
        assert_eq!(body["output_config"], output);
    }
}

#[test]
fn every_family_and_effort_matches_the_complete_request_json_snapshot() {
    let snapshots: Value =
        serde_json::from_str(include_str!("fixtures/thinking_requests.json")).unwrap();
    let families = [
        (
            ApiFamily::Anthropic,
            vec![
                "claude-sonnet-4-5",
                "claude-opus-4-5",
                "claude-sonnet-4-6",
                "claude-opus-4-6",
            ],
        ),
        (
            ApiFamily::Gemini,
            vec!["gemini-2.5-pro", "gemini-2.5-flash", "gemini-3-pro-preview"],
        ),
        (ApiFamily::Vertex, vec!["gemini-3-pro-preview"]),
        (ApiFamily::OpenAiChat, vec!["gpt-5.2"]),
        (ApiFamily::OpenAiResponses, vec!["gpt-5.2"]),
        (ApiFamily::Azure, vec!["gpt-5.2"]),
    ];
    let mut checked = 0;
    for (family, models) in families {
        for model in models {
            let endpoint = if family == ApiFamily::Vertex {
                "http://127.0.0.1:8080/v1/projects/fixture/locations/global/publishers/google"
            } else {
                "http://127.0.0.1:8080/v1"
            };
            let mut provider = provider(family.clone(), endpoint.into());
            provider.config.model = model.into();
            let efforts = provider
                .config
                .supported_reasoning_efforts()
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            for effort in efforts {
                provider.config.reasoning_effort = Some(effort.clone());
                let actual = request::body(
                    &provider,
                    &[Message::user("Hello")],
                    &[],
                    65536,
                    Reasoning::Configured,
                )
                .unwrap();
                let key = format!("{family:?}/{model}/{effort}");
                assert_eq!(actual, snapshots[&key], "{key}");
                checked += 1;
            }
        }
    }
    assert_eq!(checked, snapshots.as_object().unwrap().len());
}

#[test]
fn thinking_budget_fits_output_limit_and_unknown_models_get_no_controls() {
    let mut provider = provider(ApiFamily::Anthropic, "http://127.0.0.1:8080".into());
    provider.config.model = "claude-sonnet-4-5".into();
    provider.config.reasoning_effort = Some("high".into());
    let body = request::body(
        &provider,
        &[Message::user("hi")],
        &[],
        2048,
        Reasoning::Configured,
    )
    .unwrap();
    assert_eq!(
        body["thinking"],
        json!({"type":"enabled","budget_tokens":2047})
    );
    let small = request::body(
        &provider,
        &[Message::user("hi")],
        &[],
        100,
        Reasoning::Lowest,
    )
    .unwrap();
    assert!(small.get("thinking").is_none());
    provider.config.model = "unknown".into();
    let unknown = request::body(
        &provider,
        &[Message::user("hi")],
        &[],
        8192,
        Reasoning::Configured,
    )
    .unwrap();
    assert!(unknown.get("thinking").is_none());
}

#[test]
fn redacted_and_signed_thinking_replay_unchanged_after_tool_results() {
    let provider = provider(ApiFamily::Anthropic, "http://127.0.0.1:8080".into());
    let blocks = vec![
        json!({"type":"thinking","thinking":"Consider","signature":"signed"}),
        json!({"type":"redacted_thinking","data":"opaque"}),
        json!({"type":"tool_use","id":"lookup-1","name":"lookup","input":{}}),
    ];
    let mut assistant =
        Message::assistant_with_tool_calls("", vec![ToolCall::new("lookup-1", "lookup", "{}")]);
    assistant.native = Some(NativeContent::Anthropic(blocks.clone()));
    let body = request::body(
        &provider,
        &[
            Message::user("hi"),
            assistant,
            Message::tool_result("lookup-1", "result"),
        ],
        &[],
        8192,
        Reasoning::Configured,
    )
    .unwrap();
    assert_eq!(body["messages"][1]["content"], json!(blocks));
}

#[tokio::test]
async fn manual_thinking_requests_merge_the_interleaved_beta_with_custom_betas() {
    let events = [
        json!({"type":"message_start","message":{"id":"m","type":"message","role":"assistant","content":[],"usage":{"input_tokens":1}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Answer"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
        json!({"type":"message_stop"}),
    ];
    let (url, recorded) = fixture(sse(&events), 200, "text/event-stream").await;
    let mut config = config(ApiFamily::Anthropic, url);
    config.model = "claude-sonnet-4-5".into();
    config.reasoning_effort = Some("medium".into());
    config
        .headers
        .insert("anthropic-beta".into(), "custom-beta".into());
    let provider =
        NativeProvider::new(config, ProviderCredential::ApiKey("fixture-key".into())).unwrap();
    provider
        .complete(
            ModelRequest {
                messages: vec![Message::user("Hello")],
                tools: vec![],
                max_tokens: 32768,
                reasoning: Reasoning::Configured,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let recorded = recorded.await.unwrap();
    assert!(
        recorded
            .headers
            .contains("anthropic-beta: custom-beta,interleaved-thinking-2025-05-14")
    );
    assert_eq!(
        recorded.body["thinking"],
        json!({"type":"enabled","budget_tokens":8192})
    );
}

#[test]
fn utility_requests_choose_lowest_native_effort_even_with_reordered_catalog_options() {
    let mut provider = provider(ApiFamily::Anthropic, "http://127.0.0.1:8080".into());
    provider.config.model = "claude-sonnet-4-6".into();
    provider.config.reasoning_options = Some(vec!["high".into(), "low".into()]);
    provider.config.reasoning_effort = Some("high".into());
    let body = request::body(
        &provider,
        &[Message::user("Title")],
        &[],
        8192,
        Reasoning::Lowest,
    )
    .unwrap();
    assert_eq!(body["output_config"], json!({"effort":"low"}));
}

#[test]
fn gemini_pro_utility_requests_respect_the_minimum_thinking_budget() {
    let mut provider = provider(ApiFamily::Gemini, "http://127.0.0.1:8080".into());
    provider.config.model = "gemini-2.5-pro".into();
    let body = request::body(
        &provider,
        &[Message::user("Title")],
        &[],
        100,
        Reasoning::Lowest,
    )
    .unwrap();
    assert_eq!(
        body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        128
    );
}
