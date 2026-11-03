//! Full engine configuration and HTTP overload recovery.
use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn configured_fallback_survives_provider_guards_and_a_real_http_529() {
    let (_directory, engine, mut request) = fixture();
    let (endpoint, seen) = serve(|body| {
        if body["model"] == "gpt-6.1-sol" {
            Reply::Status(
                529,
                json!({"error":{"type":"overloaded_error","message":"Busy"}}),
            )
        } else {
            assert_eq!(body["model"], "alternative-model");
            assert!(body["input"].to_string().contains("configured fallback"));
            message("Recovered answer")
        }
    })
    .await;
    request.provider = responses(endpoint);
    request.provider.fallback_model = Some("alternative-model".into());
    let session = request.session_id;
    let (sender, mut events) = mpsc::channel(256);
    engine
        .run(request, sender, CancellationToken::new())
        .await
        .unwrap();
    let events = drain(&mut events);
    let usage = events
        .iter()
        .find_map(|event| match event {
            EngineEvent::Completed { usage, .. } => Some(usage),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        usage.calls.len(),
        1,
        "Failed overload responses have no usage receipt"
    );
    assert_eq!(usage.calls[0].model, "alternative-model");
    assert_eq!((usage.input_tokens, usage.output_tokens), (10, 5));
    assert_eq!(usage.estimated_cost.unwrap().unpriced_calls, 1);
    assert!(events.iter().any(|event| matches!(event, EngineEvent::ModelMetadata {model, ..} if model == "alternative-model")));
    assert!(events.iter().any(
        |event| matches!(event, EngineEvent::Completed { text, .. } if text == "Recovered answer")
    ));
    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::HarnessNote {
            kind: xylo_core::NoteKind::Fallback,
            ..
        }
    )));
    let seen = seen.lock();
    assert_eq!(
        seen.iter()
            .filter(|request| request["model"] == "gpt-6.1-sol" && !is_title(request))
            .count(),
        6
    );
    assert_eq!(
        seen.iter()
            .filter(|request| request["model"] == "alternative-model")
            .count(),
        1
    );
    assert_eq!(
        engine
            .store
            .messages(session, 10)
            .unwrap()
            .last()
            .unwrap()
            .content,
        "Recovered answer"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_turn_attempts_the_recovered_primary_after_the_previous_turn_used_fallback() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let (_directory, engine, mut request) = fixture();
    let recovered = Arc::new(AtomicBool::new(false));
    let response_state = recovered.clone();
    let (endpoint, seen) = serve(move |body| {
        if is_title(body) {
            return message("Title");
        }
        if body["model"] == "gpt-6.1-sol" {
            if response_state.load(Ordering::SeqCst) {
                message("Primary recovered")
            } else {
                Reply::Status(
                    529,
                    json!({"error":{"type":"overloaded_error","message":"Busy"}}),
                )
            }
        } else {
            message("Alternative answer")
        }
    })
    .await;
    request.provider = responses(endpoint);
    request.provider.fallback_model = Some("alternative-model".into());
    let first_turn = request.turn_id;
    let (sender, mut events) = mpsc::channel(256);
    engine
        .run(request.clone(), sender, CancellationToken::new())
        .await
        .unwrap();
    assert!(drain(&mut events).iter().any(
        |event| matches!(event, EngineEvent::Completed { text,.. } if text == "Alternative answer")
    ));
    assert_eq!(
        engine
            .store
            .checkpoint(request.session_id)
            .unwrap()
            .unwrap()
            .turn_id,
        first_turn
    );
    recovered.store(true, Ordering::SeqCst);
    request.turn_id = Uuid::new_v4();
    request.prompt = "A fresh turn".into();
    let (sender, mut events) = mpsc::channel(256);
    engine
        .run(request, sender, CancellationToken::new())
        .await
        .unwrap();
    let events = drain(&mut events);
    assert!(events.iter().any(|event| matches!(event, EngineEvent::Completed { text,.. } if text == "Primary recovered")),"A completed turn's fallback must not route a new turn");
    assert!(!events.iter().any(|event| matches!(
        event,
        EngineEvent::HarnessNote {
            kind: xylo_core::NoteKind::Fallback,
            ..
        }
    )));
    let seen = seen.lock();
    assert_eq!(
        seen.iter()
            .filter(|body| !is_title(body) && body["model"] == "gpt-6.1-sol")
            .count(),
        7
    );
    assert_eq!(
        seen.iter()
            .filter(|body| !is_title(body) && body["model"] == "alternative-model")
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_engine_restart_resumes_the_same_unfinished_turn_on_its_selected_fallback() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let (_directory, engine, mut request) = fixture();
    let resuming = Arc::new(AtomicBool::new(false));
    let response_state = resuming.clone();
    let (endpoint,seen) = serve(move |body| {
        if is_title(body) { return message("Title"); }
        if body["model"] == "gpt-6.1-sol" {
            if response_state.load(Ordering::SeqCst) { message("Wrong primary route") }
            else { Reply::Status(529,json!({"error":{"type":"overloaded_error","message":"Busy"}})) }
        } else if response_state.load(Ordering::SeqCst) { message("Resumed alternative") }
        else { Reply::Status(400,json!({"error":{"type":"invalid_request_error","message":"Interrupted fixture request"}})) }
    }).await;
    request.provider = responses(endpoint);
    request.provider.fallback_model = Some("alternative-model".into());
    let (sender, mut events) = mpsc::channel(256);
    engine
        .run(request.clone(), sender, CancellationToken::new())
        .await
        .unwrap();
    assert!(
        drain(&mut events)
            .iter()
            .any(|event| matches!(event, EngineEvent::Error { .. }))
    );
    assert!(
        engine
            .store
            .completed_turn(request.session_id, request.turn_id)
            .unwrap()
            .is_none()
    );
    let checkpoint = engine
        .store
        .checkpoint(request.session_id)
        .unwrap()
        .unwrap();
    assert_eq!(checkpoint.turn_id, request.turn_id);
    resuming.store(true, Ordering::SeqCst);
    let restarted = AgentEngine::new(
        Store::open_existing(&engine.paths.database).unwrap(),
        engine.paths.clone(),
        ToolSession::for_tests(PermissionMode::ReadOnly),
    );
    let (sender, mut events) = mpsc::channel(256);
    restarted
        .run(request, sender, CancellationToken::new())
        .await
        .unwrap();
    assert!(drain(&mut events).iter().any(
        |event| matches!(event,EngineEvent::Completed {text,..} if text == "Resumed alternative")
    ));
    assert_eq!(
        seen.lock()
            .iter()
            .filter(|body| !is_title(body) && body["model"] == "gpt-6.1-sol")
            .count(),
        6
    );
}

#[tokio::test]
async fn fallback_uses_catalog_protocol_and_output_cap_from_its_own_connection() {
    let (_directory, engine, mut request) = fixture();
    let (endpoint, seen) = serve(|_| message("Catalog fallback")).await;
    request.provider = ProviderConfig {
        endpoint,
        model: "primary".into(),
        fallback_model: Some("secondary".into()),
        custom: Some(Default::default()),
        ..Default::default()
    };
    let mut catalog = request.provider.clone();
    catalog.model = "secondary".into();
    catalog.model_capabilities = Some(xylo_core::ModelCapabilities {
        supported_apis: Some(vec![ApiFamily::OpenAiResponses]),
        max_output_tokens: Some(128),
        ..Default::default()
    });
    let mut settings = engine.store.settings().unwrap();
    settings.providers = vec![catalog];
    engine.store.save_settings(&settings).unwrap();
    let (_, fallback) = engine
        .provider(&request.provider)
        .unwrap()
        .fallback()
        .unwrap();
    let response = fallback
        .complete(
            ModelRequest {
                messages: vec![Message::user("Hello")],
                tools: vec![],
                max_tokens: 1024,
                reasoning: crate::model::Reasoning::Configured,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.message.content, "Catalog fallback");
    let seen = seen.lock();
    assert_eq!(seen[0]["max_output_tokens"], 128);
    assert!(seen[0].get("input").is_some());
}

#[test]
fn fallback_does_not_use_another_connections_catalog_metadata() {
    let (_directory, engine, mut request) = fixture();
    request.provider = ProviderConfig {
        endpoint: "http://127.0.0.1:34567/v1".into(),
        model: "primary".into(),
        fallback_model: Some("secondary".into()),
        custom: Some(Default::default()),
        ..Default::default()
    };
    let mut catalog = request.provider.clone();
    catalog.model = "secondary".into();
    catalog.credential_ref = Some("other-account".into());
    catalog.model_capabilities = Some(xylo_core::ModelCapabilities {
        supported_apis: Some(vec![ApiFamily::OpenAiResponses]),
        ..Default::default()
    });
    let mut settings = engine.store.settings().unwrap();
    settings.providers = vec![catalog];
    engine.store.save_settings(&settings).unwrap();
    let (_, fallback) = engine
        .provider(&request.provider)
        .unwrap()
        .fallback()
        .unwrap();
    assert!(
        !fallback.supports_search(),
        "Other account metadata must not select Responses"
    );
}
