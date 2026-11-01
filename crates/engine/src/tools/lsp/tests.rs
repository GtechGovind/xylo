//! Native authority and outside-content regressions for agent language intelligence.
#![cfg(unix)]
use super::*;
use crate::tools::{
    permissions,
    tests::{fixture, run},
};
use std::sync::Arc;
use xylo_core::{EngineEvent, PermissionMode};

fn configure(dir: &Path, extra: &[&str]) -> std::path::PathBuf {
    let marker = dir.join("server-started");
    let mut args = vec![
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../runtime/src/lsp/agent/test_server.py"
        )
        .to_owned(),
        "--marker".into(),
        marker.to_string_lossy().into_owned(),
    ];
    args.extend(extra.iter().map(|arg| (*arg).to_owned()));
    std::fs::write(
        dir.join("profile/language-servers.json"),
        json!({"servers":[{"languages":["python"],"executable":"/usr/bin/python3","args":args}]})
            .to_string(),
    )
    .unwrap();
    std::fs::write(dir.join("a.py"), "valid\n").unwrap();
    marker
}

async fn approval(
    events: &mut tokio::sync::mpsc::Receiver<EngineEvent>,
) -> (uuid::Uuid, String, Option<String>) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let EngineEvent::ApprovalRequired {
                action_id,
                reason,
                rule_key,
                ..
            } = events.recv().await.unwrap()
            {
                return (action_id, reason.unwrap_or_default(), rule_key);
            }
        }
    })
    .await
    .expect("Native execution or untrusted memory requires an approval")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_h1_read_only_cannot_launch_native_language_servers() {
    let (dir, broker, _events) = fixture(PermissionMode::ReadOnly);
    let marker = configure(dir.path(), &[]);
    let result = run(
        &broker,
        "readonly-lsp",
        "lsp",
        json!({"path":"a.py","operation":"hover"}),
    )
    .await;
    assert!(result.is_err(), "ReadOnly must refuse native execution");
    assert!(
        !marker.exists(),
        "ReadOnly launched an unrestricted process"
    );
    broker.session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_h1_ordinary_edit_authority_cannot_start_a_native_server() {
    let (dir, broker, mut events) = fixture(PermissionMode::AskForApproval);
    let marker = configure(dir.path(), &[]);
    run(&broker, "read", "file_read", json!({"path":"a.py"}))
        .await
        .unwrap();
    let result = run(
        &broker,
        "edit",
        "file_edit",
        json!({"path":"a.py","edits":[{"old_string":"valid","new_string":"BAD_TYPE"}]}),
    )
    .await
    .unwrap();
    assert!(
        !marker.exists(),
        "File edit permission authorized native execution"
    );
    assert!(result.contains("separate approval"), "{result}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.py")).unwrap(),
        "BAD_TYPE\n"
    );
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event, EngineEvent::ApprovalRequired { .. }));
    }
    broker.session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_h1_separate_native_approval_covers_explicit_and_automatic_requests() {
    let (dir, broker, mut events) = fixture(PermissionMode::AskForApproval);
    let marker = configure(dir.path(), &[]);
    let execution = broker.clone();
    let pending = tokio::spawn(async move {
        run(
            &execution,
            "native-lsp",
            "lsp",
            json!({"path":"a.py","operation":"hover"}),
        )
        .await
    });
    let (id, reason, rule) = approval(&mut events).await;
    assert!(!marker.exists(), "Process started before native approval");
    assert!(
        reason.contains("sandbox") && reason.contains("network"),
        "{reason}"
    );
    assert!(rule.is_some());
    broker
        .session
        .approvals()
        .decide_with(id, true, true)
        .unwrap();
    pending.await.unwrap().unwrap();
    assert!(marker.exists());
    run(&broker, "read", "file_read", json!({"path":"a.py"}))
        .await
        .unwrap();
    let edit = run(
        &broker,
        "edit",
        "file_edit",
        json!({"path":"a.py","edits":[{"old_string":"valid","new_string":"BAD_TYPE"}]}),
    )
    .await
    .unwrap();
    let value: Value = serde_json::from_str(&edit).unwrap();
    assert_eq!(
        value["summary"]["diagnostics"][0]["message"],
        "incompatible type"
    );
    broker.session.permissions().set(PermissionMode::ReadOnly);
    assert!(
        run(
            &broker,
            "revoked",
            "lsp",
            json!({"path":"a.py","operation":"hover"})
        )
        .await
        .is_err()
    );
    broker.session.shutdown().await;
}

async fn denied_memory_after_observation(
    broker: &Arc<ToolBroker>,
    events: &mut tokio::sync::mpsc::Receiver<EngineEvent>,
) {
    broker
        .session
        .permissions()
        .set(PermissionMode::ApproveForMe);
    let writing = broker.clone();
    let pending = tokio::spawn(async move {
        run(
            &writing,
            "remember",
            "memory_remember",
            json!({"content":"A dependency recommends this policy","source":"language server"}),
        )
        .await
    });
    let (id, reason, rule) = approval(events).await;
    assert_eq!(reason, permissions::UNTRUSTED_MEMORY_WRITE);
    assert_eq!(rule, None);
    broker
        .session
        .approvals()
        .decide_with(id, false, false)
        .unwrap();
    assert!(pending.await.unwrap().is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_h2_project_hover_and_automatic_diagnostics_remain_untrusted() {
    for automatic in [false, true] {
        let (dir, broker, mut events) = fixture(PermissionMode::FullAccess);
        configure(dir.path(), &["--external-docs"]);
        if automatic {
            run(&broker, "read", "file_read", json!({"path":"a.py"}))
                .await
                .unwrap();
            run(
                &broker,
                "edit",
                "file_edit",
                json!({"path":"a.py","edits":[{"old_string":"valid","new_string":"BAD_TYPE"}]}),
            )
            .await
            .unwrap();
        } else {
            let result = run(
                &broker,
                "hover",
                "lsp",
                json!({"path":"a.py","operation":"hover"}),
            )
            .await
            .unwrap();
            assert!(
                result.contains("External dependency documentation"),
                "{result}"
            );
        }
        denied_memory_after_observation(&broker, &mut events).await;
        broker.session.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_h1_denied_native_authority_does_not_start_a_server() {
    let (dir, broker, mut events) = fixture(PermissionMode::ApproveForMe);
    let marker = configure(dir.path(), &[]);
    let execution = broker.clone();
    let pending = tokio::spawn(async move {
        run(
            &execution,
            "denied-native",
            "lsp",
            json!({"path":"a.py","operation":"diagnostics"}),
        )
        .await
    });
    let (id, reason, _rule) = approval(&mut events).await;
    assert!(reason.contains("sandbox") && reason.contains("network"));
    broker
        .session
        .approvals()
        .decide_with(id, false, false)
        .unwrap();
    assert!(pending.await.unwrap().is_err());
    assert!(!marker.exists());
    assert!(broker.session.lsp.get().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_h3_automatic_uncorrelated_diagnostics_remain_unavailable() {
    let (dir, broker, _events) = fixture(PermissionMode::FullAccess);
    configure(dir.path(), &["--versionless-only"]);
    run(&broker, "read", "file_read", json!({"path":"a.py"}))
        .await
        .unwrap();
    let result = run(
        &broker,
        "edit",
        "file_edit",
        json!({"path":"a.py","edits":[{"old_string":"valid","new_string":"BAD_TYPE"}]}),
    )
    .await
    .unwrap();
    let value: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(value["status"], "success");
    assert_eq!(value["summary"]["diagnostics_freshness"], "unavailable");
    assert!(value["summary"]["diagnostics"].is_null());
    assert!(value["summary"]["diagnostics_unavailable"].is_string());
    broker.session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_h1_restricted_children_cannot_start_native_servers_through_edits() {
    let (dir, broker, _events) = fixture(PermissionMode::FullAccess);
    let marker = configure(dir.path(), &[]);
    let mut child = broker.scoped_child(uuid::Uuid::new_v4()).unwrap();
    child
        .restrict_tools(&["file_read".into(), "file_edit".into()])
        .unwrap();
    let child = Arc::new(child);
    run(&child, "child-read", "file_read", json!({"path":"a.py"}))
        .await
        .unwrap();
    run(
        &child,
        "child-edit",
        "file_edit",
        json!({"path":"a.py","edits":[{"old_string":"valid","new_string":"BAD_TYPE"}]}),
    )
    .await
    .unwrap();
    assert!(!marker.exists());
    assert!(broker.session.lsp.get().is_none());
}
