use super::*;
use serde_json::json;
use xylo_engine::{ModelRequest, ToolSchema};

fn request(tools: bool, reasoning: Reasoning) -> ModelRequest {
    ModelRequest {
        messages: vec![Message::system("system"), Message::user("hi")],
        tools: if tools {
            vec![ToolSchema {
                name: "file_read".into(),
                description: "Read".into(),
                parameters: json!({"type": "object"}),
            }]
        } else {
            Vec::new()
        },
        max_tokens: 64,
        reasoning,
    }
}

fn sample() -> Trace {
    Trace {
        version: TRACE_VERSION,
        task: "tool-loop".into(),
        trial: 1,
        model: ModelLabel {
            label: "scripted".into(),
            model: "scripted".into(),
        },
        provider: xylo_core::ProviderConfig::default(),
        served_window: None,
        prompt: "Fix {workspace}/src/lib.rs".into(),
        exchanges: vec![Exchange {
            kind: RequestKind::Agent,
            request: RecordedRequest::of(&request(true, Reasoning::Configured)),
            outcome: Outcome::Reply(ModelReply::calls(1, [("file_read", json!({"path": "a"}))])),
        }],
        events: vec![EngineEvent::Started],
        actions: Vec::new(),
        checkpoint: None,
    }
}

#[test]
fn requests_are_classified_by_shape() {
    assert_eq!(
        RequestKind::of(&request(true, Reasoning::Configured)),
        RequestKind::Agent
    );
    assert_eq!(
        RequestKind::of(&request(false, Reasoning::Configured)),
        RequestKind::Summary
    );
    assert_eq!(
        RequestKind::of(&request(false, Reasoning::Lowest)),
        RequestKind::Title
    );
}

#[test]
fn traces_round_trip_through_disk() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("nested/trace.json");
    let trace = sample();
    trace.save(&path).unwrap();
    let loaded = Trace::load(&path).unwrap();
    assert_eq!(
        serde_json::to_value(&loaded).unwrap(),
        serde_json::to_value(&trace).unwrap()
    );
    assert_eq!(loaded.agent_exchanges().count(), 1);
    assert_eq!(loaded.exchanges[0].request.system(), Some("system"));
}

#[test]
fn other_trace_versions_are_refused() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("trace.json");
    let mut value = serde_json::to_value(sample()).unwrap();
    value["version"] = json!(TRACE_VERSION + 1);
    std::fs::write(&path, value.to_string()).unwrap();
    let error = Trace::load(&path).unwrap_err().to_string();
    assert!(error.contains("format version"), "{error}");
}

#[test]
fn tool_digest_changes_with_any_definition_byte() {
    let mut changed = request(true, Reasoning::Configured);
    changed.tools[0].description.push('.');
    assert_ne!(
        RecordedRequest::of(&request(true, Reasoning::Configured)).tools_digest,
        RecordedRequest::of(&changed).tools_digest
    );
}

#[test]
fn provider_errors_survive_a_round_trip() {
    let errors = [
        ModelError::Authentication("a".into()),
        ModelError::RateLimited {
            retry_after: None,
            message: "b".into(),
        },
        ModelError::transient("c"),
        ModelError::permanent("d"),
        ModelError::InvalidRequest("e".into()),
        ModelError::ContextOverflow("f".into()),
        ModelError::Timeout("g".into()),
        ModelError::Interrupted,
        ModelError::Io("h".into()),
        ModelError::Stalled("i".into()),
        ModelError::EmptyResponse("j".into()),
    ];
    for error in errors {
        let recorded = RecordedError::from(&error);
        assert_eq!(recorded.to_model_error(), error);
    }
}

#[test]
fn child_requests_are_told_apart_by_the_helper_prompt() {
    assert!(!child_opening().is_empty());
    let mut child = request(true, Reasoning::Configured);
    child.messages[0] = Message::system(format!("{} Rest of the prompt.", child_opening()));
    assert_eq!(RequestKind::of(&child), RequestKind::Child);
    let recorded = RecordedRequest::of(&child);
    assert_eq!(recorded.child_task(), Some("hi"));
}

#[test]
fn sanitized_configurations_carry_no_secrets() {
    let mut config = ProviderConfig {
        credential_ref: Some("vault-key".into()),
        ..ProviderConfig::default()
    };
    config
        .headers
        .insert("Authorization".into(), "Bearer secret".into());
    let clean = sanitized(&config);
    assert_eq!(clean.credential_ref, None);
    assert!(clean.headers.is_empty());
    assert_eq!(clean.context_tokens, config.context_tokens);
}
