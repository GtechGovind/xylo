//! Capture the binding policy on real HTTP requests; no live signatures or accounts.
use super::*;
use crate::model::Reasoning;

const BINDING_BETA: &str = "thinking-binding-controls-2026-08-01";

fn signed_history() -> Vec<Message> {
    let mut assistant = Message::assistant_with_tool_calls(
        "Reading the file.",
        vec![ToolCall::new("read-1", "lookup", r#"{"q":"file"}"#)],
    );
    assistant.native = Some(NativeContent::Anthropic(vec![
        json!({"type":"thinking","thinking":"Fixture reasoning","signature":"signed-fixture"}),
        json!({"type":"redacted_thinking","data":"opaque-fixture"}),
        json!({"type":"text","text":"Reading the file."}),
        json!({"type":"tool_use","id":"read-1","name":"lookup","input":{"q":"file"}}),
    ]));
    let mut answer = Message::assistant("Read completed.");
    answer.native = Some(NativeContent::Anthropic(vec![
        json!({"type":"thinking","thinking":"After observing the file","signature":"later-signed-fixture"}),
        json!({"type":"text","text":"Read completed."}),
    ]));
    vec![
        Message::system("Original policy"),
        Message::user("Earlier task"),
        Message::assistant("Earlier answer"),
        Message::user("Read the file"),
        assistant,
        Message::tool_result("read-1", "File observation"),
        answer,
        Message::user("Continue working"),
    ]
}

async fn capture(
    model: &str,
    messages: Vec<Message>,
    tools: Vec<ToolSchema>,
    options: Option<Vec<String>>,
    beta: Option<&str>,
) -> Recorded {
    let events = [
        json!({"type":"message_start","message":{"usage":{"input_tokens":3}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Continued"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
        json!({"type":"message_stop"}),
    ];
    let (endpoint, recorded) = fixture(sse(&events), 200, "text/event-stream").await;
    let mut config = config(ApiFamily::Anthropic, endpoint);
    config.model = model.into();
    config.reasoning_options = options;
    if let Some(beta) = beta {
        config.headers.insert("anthropic-beta".into(), beta.into());
    }
    let provider =
        NativeProvider::new(config, ProviderCredential::ApiKey("fixture-key".into())).unwrap();
    provider
        .complete(
            ModelRequest {
                messages,
                tools,
                max_tokens: 8192,
                reasoning: Reasoning::Configured,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    recorded.await.unwrap()
}

#[tokio::test]
async fn signed_replay_binding_survives_changed_prefixes_and_restarts() {
    let history = signed_history();
    for model in ["claude-sonnet-5-5", "claude-opus-4-5"] {
        let mut first = None;
        for scenario in [
            "unchanged",
            "system",
            "compaction",
            "masking",
            "tools",
            "retry",
        ] {
            let mut messages = history.clone();
            let mut tools = vec![tool_schema()];
            match scenario {
                "system" => messages[0] = Message::system("Updated permission policy"),
                "compaction" => {
                    let mut summary = Message::system("Summary of the earlier task and answer");
                    summary.summary = true;
                    messages.splice(1..3, [summary]);
                }
                "masking" => messages[5].content = "Earlier observation omitted".into(),
                "tools" => tools[0].description = "Updated available tool".into(),
                _ => {}
            }
            // A later turn or retry reloads the retained blocks into a new provider.
            let stored = serde_json::to_value(&messages).unwrap();
            let restored = serde_json::from_value(stored.clone()).unwrap();
            let recorded = capture(model, restored, tools, None, Some("custom-beta")).await;
            assert_eq!(
                recorded
                    .body
                    .pointer("/thinking/block_binding/prefix_mismatch_behavior"),
                Some(&json!("drop_block")),
                "{model}: {scenario}"
            );
            assert!(
                recorded.headers.contains(BINDING_BETA),
                "{model}: {scenario}"
            );
            assert!(recorded.headers.contains("custom-beta"));
            if model == "claude-opus-4-5" {
                assert!(recorded.headers.contains("interleaved-thinking-2025-05-14"));
            }
            let replay = recorded.body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|message| message["content"][0]["type"] == "thinking")
                .map(|message| message["content"].clone())
                .collect::<Vec<_>>();
            let expected = history
                .iter()
                .filter_map(|message| match &message.native {
                    Some(NativeContent::Anthropic(blocks)) => Some(json!(blocks)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(replay, expected);
            assert_eq!(serde_json::to_value(&messages).unwrap(), stored);
            if scenario == "unchanged" {
                first = Some(recorded.body.clone());
            } else if scenario == "retry" {
                assert_eq!(Some(recorded.body), first);
            }
        }
    }
}

#[tokio::test]
async fn signed_replay_binding_uses_adaptive_defaults_without_adding_effort() {
    for model in [
        "claude-sonnet-5-5",
        "claude-opus-5-5",
        "claude-haiku-5-5",
        "claude-fable-5-1",
        "claude-mythos-5-1",
    ] {
        let recorded = capture(
            model,
            signed_history(),
            vec![tool_schema()],
            Some(vec![]),
            Some("custom-beta,thinking-binding-controls-2026-08-01"),
        )
        .await;
        assert_eq!(
            recorded.body["thinking"],
            json!({"type":"adaptive","block_binding":{"prefix_mismatch_behavior":"drop_block"}}),
            "{model}"
        );
        assert!(recorded.body.get("output_config").is_none());
        assert_eq!(recorded.headers.matches(BINDING_BETA).count(), 1);
    }
}

#[tokio::test]
async fn haiku_signed_history_replays_on_sonnet_after_a_policy_change() {
    let events = [
        json!({"type":"message_start","message":{"usage":{"input_tokens":3}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"haiku-fixture-signature"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":"Completed on Haiku"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
        json!({"type":"message_stop"}),
    ];
    let (endpoint, initial) = fixture(sse(&events), 200, "text/event-stream").await;
    let mut config = config(ApiFamily::Anthropic, endpoint);
    config.model = "claude-haiku-5-5".into();
    let haiku =
        NativeProvider::new(config, ProviderCredential::ApiKey("fixture-key".into())).unwrap();
    let initial_history = [
        Message::system("Original policy"),
        Message::user("First task"),
    ];
    let reply = collect(&haiku, &initial_history).await.unwrap();
    let initial = initial.await.unwrap();
    assert_eq!(initial.body["model"], "claude-haiku-5-5");
    let history = vec![
        Message::system("Updated policy"),
        Message::user("First task"),
        reply.message.clone(),
        Message::user("Continue on Sonnet"),
    ];
    let recorded = capture(
        "claude-sonnet-5-5",
        history,
        vec![tool_schema()],
        None,
        None,
    )
    .await;
    assert_eq!(recorded.body["model"], "claude-sonnet-5-5");
    assert_eq!(
        recorded
            .body
            .pointer("/thinking/block_binding/prefix_mismatch_behavior"),
        Some(&json!("drop_block"))
    );
    let Some(NativeContent::Anthropic(blocks)) = reply.message.native else {
        panic!("the source reply must retain its signed content");
    };
    assert_eq!(recorded.body["messages"][1]["content"], json!(blocks));
}

#[tokio::test]
async fn no_signed_replay_and_legacy_implicit_modes_keep_binding_controls_off() {
    for (model, messages, options) in [
        ("claude-sonnet-5-5", vec![Message::user("First task")], None),
        ("claude-opus-4-5", signed_history(), Some(vec![])),
        ("claude-sonnet-4-6", signed_history(), Some(vec![])),
        ("claude-opus-4-8", signed_history(), Some(vec![])),
        ("unknown-proxy-model", signed_history(), None),
    ] {
        let recorded = capture(model, messages, vec![tool_schema()], options, None).await;
        assert!(recorded.body.pointer("/thinking/block_binding").is_none());
        assert!(!recorded.headers.contains(BINDING_BETA));
    }
    for family in [
        ApiFamily::OpenAiChat,
        ApiFamily::OpenAiResponses,
        ApiFamily::Gemini,
    ] {
        let provider = provider(family, "http://127.0.0.1:1".into());
        let body = body_for(&provider, &signed_history(), &[tool_schema()]);
        assert!(body.pointer("/thinking/block_binding").is_none());
    }
}
