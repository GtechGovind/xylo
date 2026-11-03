//! Headless constraints stay authoritative across delegated brokers and aliases.
use super::*;
use crate::RunOptions;
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_tool_constraints_cannot_widen_a_parent_allow_list() {
    let (_root, broker, _events) = fixture(PermissionMode::FullAccess);
    let mut broker = Arc::try_unwrap(broker).ok().unwrap();
    broker
        .configure_run(&RunOptions {
            allowed_tools: Some(vec!["Read".into()]),
            ..Default::default()
        })
        .unwrap();
    let mut child = broker.scoped_child(Uuid::new_v4()).unwrap();
    child.restrict_tools(&["Write".into()]).unwrap();
    let error = tokio::task::spawn_blocking(move || {
        child.execute(
            "write",
            "file_write",
            json!({"path":"blocked.txt","content":"blocked"}),
        )
    })
    .await
    .unwrap()
    .unwrap_err();
    assert!(error.to_string().contains("excluded"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_callback_runs_only_after_approval_and_is_active_action() {
    let (_root, broker, mut events) = fixture(PermissionMode::AskForApproval);
    let mut broker = Arc::try_unwrap(broker).ok().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    broker
        .configure_run(&RunOptions {
            custom_tools: vec![ToolDefinition {
                name: "sdk_fixture".into(),
                description: "Test callback".into(),
                parameters: json!({"type":"object"}),
                writes: true,
            }],
            custom_handler: Some({
                let calls = calls.clone();
                Arc::new(move |_, _, _, _| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(json!({"answer":42}))
                })
            }),
            ..Default::default()
        })
        .unwrap();
    let approvals = broker.session.approvals().clone();
    let broker = Arc::new(broker);
    let execution = {
        let broker = broker.clone();
        tokio::task::spawn_blocking(move || broker.execute("custom", "sdk_fixture", json!({})))
    };
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        if let EngineEvent::ApprovalRequired { action_id, .. } = event {
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            approvals.decide_with(action_id, true, false).unwrap();
            break;
        }
        assert!(!matches!(event, EngineEvent::CustomToolRequested { .. }));
    }
    let result = execution.await.unwrap().unwrap();
    assert!(result.contains("42"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let replay =
        tokio::task::spawn_blocking(move || broker.execute("custom", "sdk_fixture", json!({})))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(result, replay);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
