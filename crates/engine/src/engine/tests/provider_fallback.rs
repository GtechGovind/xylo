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
