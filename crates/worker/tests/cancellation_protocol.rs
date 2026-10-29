//! Repeated cancellation through the real native process and an actual open SSE
//! response. No mock terminal events, provider accounts, sleeps or error matching.
use serde_json::{Value, json};
use std::{
    path::Path,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
};
use uuid::Uuid;
use xylo_core::*;
use xylo_services::Store;
const CANCELLATIONS: usize = 64;
struct Worker {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}
impl Worker {
    async fn start(profile: &Path) -> Self {
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_xylo-worker"))
            .arg("--data-dir")
            .arg(profile)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let mut worker = Self {
            child,
            input,
            output,
        };
        assert!(matches!(
            worker.event().await.payload,
            EngineEvent::Ready { .. }
        ));
        worker
    }
    async fn send(&mut self, session: Uuid, payload: WorkerCommand) -> Uuid {
        let id = Uuid::new_v4();
        let mut line = serde_json::to_vec(&Envelope::new(id, Some(session), 0, payload)).unwrap();
        line.push(b'\n');
        self.input.write_all(&line).await.unwrap();
        self.input.flush().await.unwrap();
        id
    }
    async fn event(&mut self) -> Envelope<EngineEvent> {
        let mut line = String::new();
        let count = tokio::time::timeout(Duration::from_secs(15), self.output.read_line(&mut line))
            .await
            .expect("worker event deadline")
            .unwrap();
        assert!(count > 0, "worker exited");
        let event: Envelope<EngineEvent> = serde_json::from_str(&line).unwrap();
        event.validate().unwrap();
        event
    }
    async fn shutdown(mut self) {
        self.send(Uuid::nil(), WorkerCommand::Shutdown).await;
        assert!(
            tokio::time::timeout(Duration::from_secs(10), self.child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
}
async fn open_stream_fixture() -> (String, tokio::task::JoinHandle<()>, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let task = tokio::spawn(async move {
        for _ in 0..CANCELLATIONS {
            let (mut socket, _) = tokio::time::timeout(Duration::from_secs(15), listener.accept())
                .await
                .expect("next model request")
                .unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0; 8192];
            let header = loop {
                let n = socket.read(&mut buffer).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buffer[..n]);
                if let Some(index) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                    break index + 4;
                }
                assert!(bytes.len() < 65536);
            };
            let length = String::from_utf8_lossy(&bytes[..header])
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|value| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            assert!(length < 1024 * 1024);
            while bytes.len() < header + length {
                let n = socket.read(&mut buffer).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buffer[..n]);
            }
            let request: Value = serde_json::from_slice(&bytes[header..header + length]).unwrap();
            assert_eq!(request["model"], "fixture");
            counted.fetch_add(1, Ordering::SeqCst);
            let delta = json!({"id":"local-open-stream","object":"chat.completion.chunk","model":"fixture","choices":[{"index":0,"delta":{"content":"Partial streamed answer αβ 🦀"},"finish_reason":null}]});
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 1000000\r\nConnection: close\r\n\r\ndata: {delta}\n\n"
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
            // No terminal chunk is sent. Cancellation, not provider EOF/stop, must end
            // the generation and close this body before the next turn is admitted.
            let mut byte = [0];
            let n = tokio::time::timeout(Duration::from_secs(15), socket.read(&mut byte))
                .await
                .expect("cancelled provider body was retained")
                .unwrap();
            assert_eq!(n, 0);
        }
    });
    (endpoint, task, calls)
}
#[tokio::test]
async fn repeated_active_stream_cancellation_emits_correlated_ack_and_cancelled_and_keeps_checkpoint()
 {
    let workspace = tempfile::tempdir().unwrap();
    let profile = tempfile::tempdir().unwrap();
    let paths = xylo_runtime::AppPaths::at(profile.path().into()).unwrap();
    let store = Store::open(&paths.database).unwrap();
    let (endpoint, fixture, calls) = open_stream_fixture().await;
    let mut worker = Worker::start(profile.path()).await;
    for index in 0..CANCELLATIONS {
        let session = Uuid::new_v4();
        let turn = TurnRequest {
            session_id: session,
            turn_id: Uuid::new_v4(),
            prompt: format!("Cancellation boundary fixture {index}"),
            workspace: workspace.path().into(),
            provider: ProviderConfig {
                endpoint: endpoint.clone(),
                model: "fixture".into(),
                ..Default::default()
            },
            permissions: PermissionMode::ReadOnly,
            attachments: Vec::new(),
        };
        let run = worker
            .send(session, WorkerCommand::Run { turn: turn.clone() })
            .await;
        loop {
            let event = worker.event().await;
            match event.payload {
                EngineEvent::Delta { text } => {
                    assert_eq!(event.request_id, run);
                    assert!(!text.is_empty());
                    break;
                }
                EngineEvent::Error { message, .. } => {
                    panic!("generation failed before cancellation: {message}")
                }
                EngineEvent::Completed { .. } | EngineEvent::Cancelled => {
                    panic!("generation ended before explicit cancellation")
                }
                _ => {}
            }
        }
        let cancel = worker.send(session, WorkerCommand::Cancel).await;
        let (mut ack, mut cancelled) = (false, false);
        while !(ack && cancelled) {
            let event = worker.event().await;
            match event.payload {
                EngineEvent::Status { .. } if event.request_id == cancel => ack = true,
                EngineEvent::Cancelled => {
                    assert_eq!(event.request_id, run);
                    assert_eq!(event.session_id, Some(session));
                    cancelled = true;
                }
                EngineEvent::Error { message, .. } => panic!(
                    "expected cancellation was reported as Error at repetition{index}: {message}"
                ),
                EngineEvent::Completed { .. } => panic!("cancelled stream completed"),
                _ => {}
            }
        }
        let checkpoint = store
            .checkpoint(session)
            .unwrap()
            .expect("Cancellation must retain durable context");
        assert_eq!(checkpoint.turn_id, turn.turn_id);
        let original = checkpoint
            .messages
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "user" && message["content"] == turn.prompt)
            .count();
        assert_eq!(original, 1);
        assert!(
            store
                .completed_turn(session, turn.turn_id)
                .unwrap()
                .is_none()
        );
        assert!(store.unresolved_actions(session).unwrap().is_empty());
        assert_eq!(
            store
                .messages(session, 10)
                .unwrap()
                .iter()
                .filter(|message| message.role == "user")
                .count(),
            1
        );
    }
    worker.shutdown().await;
    fixture.await.unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        CANCELLATIONS,
        "Cancellation must not retry model requests"
    );
}
