//! Real worker-to-host activity stays available in memory, without action tables.
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;
use xylo_core::*;
use xylo_runtime::{
    AppPaths,
    host::{HostClient, HostEvent},
};
use xylo_services::Store;

macro_rules! next {
    ($events:expr) => {
        tokio::time::timeout(Duration::from_secs(25), $events.recv())
            .await
            .unwrap()
            .unwrap()
    };
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_receives_complete_live_tool_activity_before_completion_without_persisting_it() {
    assert!(std::path::Path::new(env!("CARGO_BIN_EXE_xylo-worker")).is_file());
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().canonicalize().unwrap();
    std::fs::write(workspace.join("sample.txt"), "live activity fixture").unwrap();
    let paths = AppPaths::at(workspace.join("profile")).unwrap();
    Store::open(&paths.database).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let provider = tokio::spawn(async move {
        let mut requests = Vec::<Value>::new();
        while requests.len() < 2 {
            let (socket, _) = tokio::time::timeout(Duration::from_secs(20), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut socket = BufReader::new(socket);
            let mut first = String::new();
            socket.read_line(&mut first).await.unwrap();
            let mut length = 0;
            loop {
                let mut header = String::new();
                assert!(socket.read_line(&mut header).await.unwrap() > 0);
                if header == "\r\n" {
                    break;
                }
                if let Some((name, value)) = header.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse::<usize>().unwrap();
                }
            }
            if first.starts_with("GET ") {
                socket
                    .get_mut()
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .unwrap();
                continue;
            }
            assert!((1..=1024 * 1024).contains(&length));
            let mut body = vec![0; length];
            socket.read_exact(&mut body).await.unwrap();
            requests.push(serde_json::from_slice(&body).unwrap());
            let delta = if requests.len() == 1 {
                json!({"tool_calls":[{"index":0,"id":"live-file-read","type":"function","function":{"name":"file_read","arguments":"{\"path\":\"sample.txt\"}"}}]})
            } else {
                json!({"content":"Read live activity fixture."})
            };
            let finish = if requests.len() == 1 {
                "tool_calls"
            } else {
                "stop"
            };
            let event = json!({"id":"live-activity","object":"chat.completion.chunk","model":"fixture","choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
            let body = format!("data: {event}\n\ndata: [DONE]\n\n");
            socket.get_mut().write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
        requests
    });
    let session = Uuid::new_v4();
    let turn = Uuid::new_v4();
    let (host, events) = HostClient::spawn_at(paths.clone()).unwrap();
    host.send_to(
        session,
        WorkerCommand::Run {
            turn: TurnRequest {
                session_id: session,
                turn_id: turn,
                prompt: "Read sample.txt and report its contents.".into(),
                workspace: workspace.clone(),
                provider: ProviderConfig {
                    endpoint,
                    model: "fixture".into(),
                    context_tokens: 65536,
                    output_tokens: 256,
                    timeout_seconds: 15,
                    ..Default::default()
                },
                permissions: PermissionMode::AskForApproval,
                attachments: vec![],
            },
        },
    )
    .unwrap();
    let mut finished = None;
    loop {
        match next!(events) {
            HostEvent::Engine(event) => match event.payload {
                EngineEvent::ToolFinished {
                    action_id,
                    success,
                    result,
                    ..
                } => {
                    assert!(success);
                    assert!(result.contains("live activity fixture"));
                    finished = Some(action_id);
                }
                EngineEvent::Completed { .. } => break,
                EngineEvent::LiveActivity { .. } => panic!("Private activity reached the UI"),
                EngineEvent::Error { message, .. } => panic!("Worker failed: {message}"),
                _ => {}
            },
            HostEvent::Error(message) => panic!("Host failed: {message}"),
            _ => {}
        }
    }
    let action = finished.expect("Real worker must finish its tool");
    let request = Uuid::new_v4();
    host.turn_activity(request, session, turn, None).unwrap();
    loop {
        if let HostEvent::TurnActivity {
            request_id, result, ..
        } = next!(events)
            && request_id == request
        {
            let page = result.unwrap();
            assert!(page.items.iter().any(|item| item.id == action
                && item.name == "file_read"
                && item.state == "succeeded"));
            break;
        }
    }
    let request = Uuid::new_v4();
    host.turn_action(request, session, turn, action).unwrap();
    loop {
        if let HostEvent::TurnAction {
            request_id, result, ..
        } = next!(events)
            && request_id == request
        {
            let action = result.unwrap().unwrap();
            assert!(
                action
                    .result
                    .unwrap()
                    .to_string()
                    .contains("live activity fixture")
            );
            break;
        }
    }
    let reopened = Store::open_existing(&paths.database).unwrap();
    assert!(reopened.action(action).unwrap().is_none());
    assert!(
        reopened
            .turn_activity_page(session, turn, None, 50)
            .unwrap()
            .items
            .is_empty()
    );
    assert!(
        reopened
            .checkpoint(session)
            .unwrap()
            .unwrap()
            .messages
            .to_string()
            .contains("live activity fixture")
    );
    assert_eq!(provider.await.unwrap().len(), 2);
    host.shutdown().unwrap();
}
