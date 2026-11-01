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
