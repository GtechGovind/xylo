//! Uses the real Host and current Cargo-built worker with an open offline SSE
//! response. UI state or a fake terminal event cannot satisfy this busy proof.
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;
use xylo_core::*;
use xylo_runtime::{
    AppPaths,
    host::{HostClient, HostEvent},
};
use xylo_services::{
    ActionIntent, ActionResolutionOutcome, ActionResolutionRequest, ActionReviewScope, Store,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_host_denies_resolution_during_active_worker_then_records_after_cancel() {
    assert!(std::path::Path::new(env!("CARGO_BIN_EXE_xylo-worker")).is_file());
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().canonicalize().unwrap();
    let paths = AppPaths::at(workspace.clone()).unwrap();
    let store = Store::open(&paths.database).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let provider = tokio::spawn(async move {
        let (mut socket, _) = tokio::time::timeout(Duration::from_secs(15), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut received = Vec::new();
        let mut buffer = [0; 8192];
        let header = loop {
            let count = socket.read(&mut buffer).await.unwrap();
            assert!(count > 0);
            received.extend_from_slice(&buffer[..count]);
            if let Some(at) = received.windows(4).position(|v| v == b"\r\n\r\n") {
                break at + 4;
            }
            assert!(received.len() < 65536)
        };
        let length = String::from_utf8_lossy(&received[..header])
            .lines()
            .find_map(|line| {
                line.to_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        assert!(length < 1024 * 1024);
        while received.len() < header + length {
            let count = socket.read(&mut buffer).await.unwrap();
            assert!(count > 0);
            received.extend_from_slice(&buffer[..count]);
        }
        let delta = json!({"id":"real-host-review","object":"chat.completion.chunk","model":"fixture-model","choices":[{"index":0,"delta":{"content":"ACTIVE_WORKER_STREAM"},"finish_reason":null}]});
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 1000000\r\nConnection: close\r\n\r\ndata: {delta}\n\n"
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.flush().await.unwrap();
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(15), socket.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0,
            "Cancellation must close the actual provider stream"
        );
    });
    let session = Uuid::new_v4();
    let turn = TurnRequest {
        session_id: session,
        turn_id: Uuid::new_v4(),
        prompt: "Keep working until cancelled.".into(),
        workspace: workspace.clone(),
        provider: ProviderConfig {
            endpoint,
            model: "fixture-model".into(),
            context_tokens: 65536,
            output_tokens: 256,
            timeout_seconds: 15,
            ..Default::default()
        },
        permissions: PermissionMode::AskForWrites,
        attachments: vec![],
    };
    let (host, events) = HostClient::spawn_at(paths).unwrap();
    host.send_to(session, WorkerCommand::Run { turn }).unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match events.recv().await.unwrap() {
                HostEvent::Engine(envelope) if envelope.session_id == Some(session) => {
                    match envelope.payload {
                        EngineEvent::Delta { text } if text.contains("ACTIVE_WORKER_STREAM") => {
                            break;
                        }
                        EngineEvent::Error { message, .. } => {
                            panic!("Worker failed before busy proof: {message}")
                        }
                        _ => {}
                    }
                }
                HostEvent::Error(error) => panic!("Host failed before busy proof: {error}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    // Introduce a quiescent, qualified interrupted local write while that real
    // worker is generating. Host admission must reject review despite free lease.
    let scope = ActionReviewScope {
        session_id: session,
        workspace: workspace.clone(),
    };
    let action = Uuid::new_v4();
    let guard = store.action_execution_lease(action, true).unwrap();
    store
        .begin_action(&ActionIntent {
            id: action,
            session_id: session,
            turn_id: Uuid::new_v4(),
            name: "file_write".into(),
            arguments: json!({"path":"partial.txt","content":"requested"}),
            writes: true,
            idempotency_key: format!("root:{action}"),
        })
        .unwrap();
    store.mark_action_running(action).unwrap();
    std::fs::write(workspace.join("partial.txt"), "retained partial effect").unwrap();
    store
        .mark_action_unknown_with_result(
            action,
            &json!({"status":"failed","artifacts":[workspace.join("partial.txt")]}),
            "Interrupted local write",
        )
        .unwrap();
    drop(guard);
    let request = ActionResolutionRequest {
        operation_id: Uuid::new_v4(),
        scope: scope.clone(),
        action_id: action,
        expected_revision: store
            .action_review(scope.clone(), action)
            .unwrap()
            .action
            .revision,
        outcome: ActionResolutionOutcome::DidNotComplete,
        evidence: "I inspected the retained partial file.".into(),
    };
    let busy_id = Uuid::new_v4();
    host.resolve_action(busy_id, request.clone()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let HostEvent::ActionResolved { request_id, result } = events.recv().await.unwrap()
                && request_id == busy_id
            {
                assert!(result.unwrap_err().contains("chat has stopped working"));
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        store
            .action_resolution(scope.clone(), request.operation_id)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store.action(action).unwrap().unwrap().state,
        "outcome_unknown"
    );
    host.send_to(session, WorkerCommand::Cancel).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let HostEvent::Engine(envelope) = events.recv().await.unwrap()
                && envelope.session_id == Some(session)
                && matches!(envelope.payload, EngineEvent::Cancelled)
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    provider.await.unwrap();
    let after_id = Uuid::new_v4();
    host.resolve_action(after_id, request.clone()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let HostEvent::ActionResolved { request_id, result } = events.recv().await.unwrap()
                && request_id == after_id
            {
                let receipt = result.unwrap();
                assert_eq!(receipt.operation_id, request.operation_id);
                assert_eq!(receipt.outcome, ActionResolutionOutcome::DidNotComplete);
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(store.action(action).unwrap().unwrap().state, "failed");
    assert_eq!(
        std::fs::read_to_string(workspace.join("partial.txt")).unwrap(),
        "retained partial effect"
    );
    host.shutdown().unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !matches!(events.recv().await.unwrap(), HostEvent::Stopped) {}
    })
    .await
    .unwrap();
}
