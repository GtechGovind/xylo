//! Durable guidance through the real native worker and streaming transport.
//! HTTP responses are gated locally to reproduce actual process interruption.
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    process::Stdio,
    sync::mpsc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
};
use uuid::Uuid;
use xylo_core::*;
use xylo_services::{DraftKey, DraftSave, DraftSubmission, DraftWrite, Store};
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
        assert!(matches!(worker.event().await, EngineEvent::Ready { .. }));
        worker
    }
    async fn send(&mut self, session: Uuid, payload: WorkerCommand) {
        let mut line =
            serde_json::to_vec(&Envelope::new(Uuid::new_v4(), Some(session), 0, payload)).unwrap();
        line.push(b'\n');
        self.input.write_all(&line).await.unwrap();
        self.input.flush().await.unwrap();
    }
    async fn event(&mut self) -> EngineEvent {
        let mut line = String::new();
        let count = tokio::time::timeout(Duration::from_secs(30), self.output.read_line(&mut line))
            .await
            .expect("worker response timeout")
            .unwrap();
        assert!(count > 0, "worker exited");
        let envelope: Envelope<EngineEvent> = serde_json::from_str(&line).unwrap();
        envelope.validate().unwrap();
        envelope.payload
    }
    async fn receipt(&mut self, state: SteeringState) -> SteeringReceipt {
        loop {
            match self.event().await {
                EngineEvent::Steering { receipt } if receipt.state == state => return receipt,
                EngineEvent::Error { message, .. } => panic!("worker: {message}"),
                EngineEvent::Completed { .. } | EngineEvent::Cancelled => {
                    panic!("task ended before expected guidance receipt")
                }
                _ => {}
            }
        }
    }
    async fn completed(&mut self) {
        loop {
            match self.event().await {
                EngineEvent::Completed { .. } => return,
                EngineEvent::Error { message, .. } => panic!("worker: {message}"),
                EngineEvent::Cancelled => panic!("unexpected cancellation"),
                _ => {}
            }
        }
    }
    async fn kill(mut self) {
        self.child.kill().await.unwrap();
        self.child.wait().await.unwrap();
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
struct Request {
    body: Value,
    release: mpsc::Sender<()>,
}
fn provider(count: usize) -> (String, tokio::sync::mpsc::UnboundedReceiver<Request>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let (send, recv) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for connection in listener.incoming().take(count) {
            let mut connection = connection.unwrap();
            connection
                .set_read_timeout(Some(Duration::from_secs(30)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0; 8192];
            let header_end = loop {
                let n = connection.read(&mut buffer).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buffer[..n]);
                if let Some(index) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                    break index + 4;
                }
                assert!(bytes.len() < 65536);
            };
            let len = String::from_utf8_lossy(&bytes[..header_end])
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|n| n.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            while bytes.len() < header_end + len {
                let n = connection.read(&mut buffer).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buffer[..n]);
            }
            let body = serde_json::from_slice(&bytes[header_end..header_end + len]).unwrap();
            let (release, gate) = mpsc::channel();
            if send.send(Request { body, release }).is_err() {
                return;
            }
            if gate.recv_timeout(Duration::from_secs(30)).is_err() {
                continue;
            }
            let first = json!({"id":"local-guidance","object":"chat.completion.chunk","model":"fixture","choices":[{"index":0,"delta":{"content":"The revised answer follows the user's guidance."},"finish_reason":null}]});
            let last = json!({"id":"local-guidance","object":"chat.completion.chunk","model":"fixture","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120}});
            let body = format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n");
            let _ = write!(
                connection,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = connection.flush();
        }
    });
    (endpoint, recv)
}
async fn request(recv: &mut tokio::sync::mpsc::UnboundedReceiver<Request>) -> Request {
    tokio::time::timeout(Duration::from_secs(30), recv.recv())
        .await
        .expect("provider was not called")
        .expect("fixture ended")
}
fn turn(workspace: &Path, endpoint: String) -> TurnRequest {
    TurnRequest {
        session_id: Uuid::new_v4(),
        turn_id: Uuid::new_v4(),
        prompt: "Explain the chosen approach concisely.".into(),
        workspace: workspace.into(),
        provider: ProviderConfig {
            endpoint,
            model: "fixture".into(),
            ..Default::default()
        },
        permissions: PermissionMode::AskForWrites,
        attachments: Vec::new(),
    }
}
fn queue(store: &Store, turn: &TurnRequest) -> SteeringRequest {
    let key = DraftKey {
        session_id: turn.session_id,
        workspace: Some(turn.workspace.clone()),
    };
    let write = DraftWrite {
        key: key.clone(),
        expected_revision: 0,
        mutation_id: Uuid::new_v4(),
        text: "Use the verified option αβ 🦀".into(),
        attachments: Vec::new(),
    };
    let DraftSave::Saved(saved) = store.save_draft(&write).unwrap() else {
        panic!("unexpected conflict")
    };
    let guidance = SteeringRequest {
        id: Uuid::new_v4(),
        session_id: turn.session_id,
        turn_id: turn.turn_id,
        text: saved.text.clone(),
    };
    let submission = DraftSubmission {
        key,
        expected_revision: saved.revision,
        text: saved.text,
        attachments: Vec::new(),
    };
    assert_eq!(
        store
            .queue_steering(&guidance, &submission)
            .unwrap()
            .receipt
            .state,
        SteeringState::Queued
    );
    guidance
}
fn occurrences(body: &Value, guidance: &SteeringRequest) -> usize {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "user" && message["content"] == guidance.text)
        .count()
}
#[tokio::test]
async fn guidance_during_final_stream_requires_another_provider_call_and_exact_receipt() {
    let workspace = tempfile::tempdir().unwrap();
    let profile = tempfile::tempdir().unwrap();
    let paths = xylo_runtime::AppPaths::at(profile.path().into()).unwrap();
    let store = Store::open(&paths.database).unwrap();
    let (endpoint, mut calls) = provider(2);
    let turn = turn(workspace.path(), endpoint);
    let mut worker = Worker::start(profile.path()).await;
    worker
        .send(turn.session_id, WorkerCommand::Run { turn: turn.clone() })
        .await;
    let first = request(&mut calls).await;
    let guidance = queue(&store, &turn);
    worker
        .send(
            turn.session_id,
            WorkerCommand::SteerDurable {
                guidance: guidance.clone(),
            },
        )
        .await;
    assert_eq!(worker.receipt(SteeringState::Queued).await.id, guidance.id);
    assert!(
        !store
            .draft(&DraftKey {
                session_id: turn.session_id,
                workspace: Some(turn.workspace.clone())
            })
            .unwrap()
            .is_empty()
    );
    first.release.send(()).unwrap();
    assert_eq!(worker.receipt(SteeringState::Applied).await.id, guidance.id);
    let second = request(&mut calls).await;
    assert_eq!(occurrences(&second.body, &guidance), 1);
    second.release.send(()).unwrap();
    worker.completed().await;
    worker
        .send(
            turn.session_id,
            WorkerCommand::SteerDurable {
                guidance: guidance.clone(),
            },
        )
        .await;
    assert_eq!(worker.receipt(SteeringState::Applied).await.id, guidance.id);
    assert_eq!(
        store
            .messages(turn.session_id, 20)
            .unwrap()
            .iter()
            .filter(|message| message.id == Uuid::new_v5(&guidance.id, b"steering-message"))
            .count(),
        1
    );
    worker.shutdown().await;
}
#[tokio::test]
async fn actual_process_interruptions_recover_queued_and_applied_context_without_duplicate_guidance()
 {
    let workspace = tempfile::tempdir().unwrap();
    let profile = tempfile::tempdir().unwrap();
    let paths = xylo_runtime::AppPaths::at(profile.path().into()).unwrap();
    let store = Store::open(&paths.database).unwrap();
    let (endpoint, mut calls) = provider(4);
    let turn = turn(workspace.path(), endpoint);
    let mut worker = Worker::start(profile.path()).await;
    worker
        .send(turn.session_id, WorkerCommand::Run { turn: turn.clone() })
        .await;
    let first = request(&mut calls).await;
    let guidance = queue(&store, &turn);
    worker
        .send(
            turn.session_id,
            WorkerCommand::SteerDurable {
                guidance: guidance.clone(),
            },
        )
        .await;
    worker.receipt(SteeringState::Queued).await;
    worker.kill().await;
    drop(first);
    assert_eq!(
        store.steering(guidance.id).unwrap().unwrap().receipt.state,
        SteeringState::Queued
    );
    // Standalone restart retains pending guidance in the checkpoint namespace.
    // The desktop host cancels unconfirmed queued guidance on startup, keeping
    // its composer for explicit retry (covered by the Store recovery test).
    let mut system_count = None;
    for restart in 0..3 {
        let mut worker = Worker::start(profile.path()).await;
        if restart > 0 {
            worker
                .send(
                    turn.session_id,
                    WorkerCommand::SteerDurable {
                        guidance: guidance.clone(),
                    },
                )
                .await;
            worker.receipt(SteeringState::Applied).await;
        }
        worker
            .send(turn.session_id, WorkerCommand::Run { turn: turn.clone() })
            .await;
        if restart == 0 {
            worker.receipt(SteeringState::Applied).await;
        }
        let resumed = loop {
            tokio::select! {
                request = request(&mut calls) => break request,
                event = worker.event() => match event {
                    EngineEvent::Error { message, .. } => panic!("Resumed worker failed: {message}"),
                    EngineEvent::Completed { .. } | EngineEvent::Cancelled => panic!("Resumed worker terminated before provider request: {event:?}"),
                    _ => {}
                }
            }
        };
        assert_eq!(occurrences(&resumed.body, &guidance), 1);
        let count = resumed.body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "system")
            .count();
        assert_eq!(
            *system_count.get_or_insert(count),
            count,
            "Repeated resume must not accumulate old policy prefixes"
        );
        if restart < 2 {
            worker.kill().await;
            drop(resumed);
        } else {
            resumed.release.send(()).unwrap();
            worker.completed().await;
            worker.shutdown().await;
        }
        assert_eq!(
            store.steering(guidance.id).unwrap().unwrap().receipt.state,
            SteeringState::Applied
        );
    }
    assert_eq!(
        store
            .messages(turn.session_id, 20)
            .unwrap()
            .iter()
            .filter(|message| message.id == Uuid::new_v5(&guidance.id, b"steering-message"))
            .count(),
        1
    );
}

#[tokio::test]
async fn cancellation_before_application_reports_cancelled_and_preserves_draft() {
    let workspace = tempfile::tempdir().unwrap();
    let profile = tempfile::tempdir().unwrap();
    let paths = xylo_runtime::AppPaths::at(profile.path().into()).unwrap();
    let store = Store::open(&paths.database).unwrap();
    let (endpoint, mut calls) = provider(1);
    let turn = turn(workspace.path(), endpoint);
    let mut worker = Worker::start(profile.path()).await;
    worker
        .send(turn.session_id, WorkerCommand::Run { turn: turn.clone() })
        .await;
    let in_flight = request(&mut calls).await;
    let guidance = queue(&store, &turn);
    worker
        .send(
            turn.session_id,
            WorkerCommand::SteerDurable {
                guidance: guidance.clone(),
            },
        )
        .await;
    worker.receipt(SteeringState::Queued).await;
    worker.send(turn.session_id, WorkerCommand::Cancel).await;
    // Let the in-flight response finish after cancellation; neither that answer
    // nor its finalization boundary may acknowledge guidance as applied.
    in_flight.release.send(()).unwrap();
    assert_eq!(
        worker.receipt(SteeringState::Cancelled).await.id,
        guidance.id
    );
    loop {
        match worker.event().await {
            EngineEvent::Cancelled => break,
            EngineEvent::Error { message, .. } => {
                panic!("Cancelled guidance task failed: {message}")
            }
            EngineEvent::Completed { .. } => panic!("Cancelled task completed"),
            _ => {}
        }
    }
    assert_eq!(
        store.steering(guidance.id).unwrap().unwrap().receipt.state,
        SteeringState::Cancelled
    );
    assert_eq!(
        store
            .draft(&DraftKey {
                session_id: turn.session_id,
                workspace: Some(turn.workspace)
            })
            .unwrap()
            .text,
        guidance.text
    );
    assert!(
        !store
            .messages(turn.session_id, 20)
            .unwrap()
            .iter()
            .any(|message| message.id == Uuid::new_v5(&guidance.id, b"steering-message"))
    );
    worker.shutdown().await;
}
