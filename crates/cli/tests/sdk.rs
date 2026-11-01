//! Exercises the public Rust SDK against the real CLI transport and engine.
use serde_json::json;
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use xylo_sdk::{EngineEvent, PermissionMode, Session, SessionOptions, ToolDefinition};

fn options(
    root: &tempfile::TempDir,
    script: serde_json::Value,
    mode: PermissionMode,
) -> SessionOptions {
    let path = root.path().join("script.json");
    std::fs::write(&path, serde_json::to_vec(&script).unwrap()).unwrap();
    SessionOptions {
        executable: env!("CARGO_BIN_EXE_xylo-cli").into(),
        cwd: Some(root.path().into()),
        data_dir: Some(root.path().join("profile")),
        permission_mode: Some(mode),
        environment: HashMap::from([(
            "XYLO_EVAL_SCRIPT".into(),
            path.to_string_lossy().into_owned(),
        )]),
        ..Default::default()
    }
}

#[tokio::test]
async fn session_runs_streams_hooks_and_resumes_over_the_public_protocol() {
    let root = tempfile::tempdir().unwrap();
    let config = options(
        &root,
        json!({"steps":[{"step":"reply","text":"Hello from scripted provider."}]}),
        PermissionMode::AskForApproval,
    );
    let mut session = Session::new(config.clone()).await.unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    session.on_event({
        let calls = calls.clone();
        Arc::new(move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
        })
    });
    let first = tokio::time::timeout(std::time::Duration::from_secs(30), session.run("hello"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.exit_code, 0);
    assert_eq!(first.text, "Hello from scripted provider.");
    assert_eq!(calls.load(Ordering::SeqCst), first.events.len());
    let second = session.run("again").await.unwrap();
    assert_eq!(second.session_id, first.session_id);
    assert_ne!(second.turn_id, first.turn_id);
    assert_eq!(session.close().await.unwrap(), 0);
    let mut resumed = Session::new(SessionOptions {
        resume: Some(first.session_id),
        ..config
    })
    .await
    .unwrap();
    let mut stream = resumed.stream("continue").await.unwrap();
    let mut terminal = false;
    while let Some(event) = stream.next().await.unwrap() {
        assert_eq!(event.session_id, first.session_id);
        terminal |= event.is_terminal();
    }
    assert!(terminal);
    assert_eq!(resumed.close().await.unwrap(), 0);
}

#[tokio::test]
async fn custom_tool_callback_waits_for_explicit_approval() {
    let root = tempfile::tempdir().unwrap();
    let config = options(
        &root,
        json!({"steps":[{"step":"reply","tool_calls":[{"id":"custom","name":"sdk_echo","arguments":"{\"value\":42}"}],"finish":"tool_calls"},{"step":"reply","text":"Callback finished."}]}),
        PermissionMode::AskForApproval,
    );
    let mut session = Session::new(config).await.unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    session.register_tool(ToolDefinition {name:"sdk_echo".into(),description:"Echo a value".into(),parameters:json!({"type":"object","properties":{"value":{"type":"integer"}},"required":["value"]}),writes:true}, {let calls=calls.clone();Arc::new(move |value| {calls.fetch_add(1,Ordering::SeqCst);Ok(value)})}).await.unwrap();
    let control = session.control();
    let mut stream = session.stream("call echo").await.unwrap();
    let mut approved = false;
    while let Some(event) = tokio::time::timeout(std::time::Duration::from_secs(30), stream.next())
        .await
        .unwrap()
        .unwrap()
    {
        if let EngineEvent::ApprovalRequired { action_id, .. } = event.payload {
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            control.approve(action_id, true).await.unwrap();
            approved = true;
        }
        if let EngineEvent::CustomToolRequested { .. } = event.payload {
            assert!(approved);
        }
        if event.is_terminal() {
            assert_eq!(event.exit_code, Some(0));
        }
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(session.close().await.unwrap(), 0);
}
