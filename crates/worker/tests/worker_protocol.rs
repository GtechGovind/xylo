//! End-to-end checks use the real worker process, Hermes streaming provider,
//! native broker and SQLite journal against a local deterministic HTTP fixture.

use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
};
use uuid::Uuid;
use workbench_core::*;

struct Worker {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    _profile: tempfile::TempDir,
}

impl Worker {
    async fn start() -> Self {
        Self::with_profile(tempfile::tempdir().unwrap()).await
    }
    async fn with_profile(profile: tempfile::TempDir) -> Self {
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_workbench-worker"))
            .arg("--data-dir")
            .arg(profile.path())
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
            _profile: profile,
        };
        assert!(matches!(
            worker.event().await.payload,
            EngineEvent::Ready { .. }
        ));
        worker
    }
    async fn send(&mut self, session: Uuid, payload: WorkerCommand) {
        let envelope = Envelope::new(Uuid::new_v4(), Some(session), 0, payload);
        let mut line = serde_json::to_vec(&envelope).unwrap();
        line.push(b'\n');
        self.input.write_all(&line).await.unwrap();
        self.input.flush().await.unwrap();
    }
    async fn event(&mut self) -> Envelope<EngineEvent> {
        let mut line = String::new();
        let length =
            tokio::time::timeout(Duration::from_secs(45), self.output.read_line(&mut line))
                .await
                .expect("Worker did not respond before timeout")
                .unwrap();
        assert!(length > 0, "Worker exited unexpectedly");
        let envelope: Envelope<EngineEvent> = serde_json::from_str(&line).unwrap();
        envelope.validate().unwrap();
        envelope
    }
    async fn finish(&mut self) {
        self.send(Uuid::nil(), WorkerCommand::Shutdown).await;
        let status = tokio::time::timeout(Duration::from_secs(10), self.child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(status.success());
    }
    async fn restart(mut self) -> Self {
        self.finish().await;
        Self::with_profile(self._profile).await
    }
    async fn stop(mut self) {
        self.finish().await;
    }
}

/// A minimal real OpenAI streaming transport fixture. Each response closes the
/// connection, avoiding accidental assumptions about HTTP pooling or framing.
fn provider_fixture(tool: &str, args: Value) -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let saved = requests.clone();
    let tool = tool.to_string();
    std::thread::spawn(move || {
        for (index, connection) in listener.incoming().take(2).enumerate() {
            let mut connection = connection.unwrap();
            connection
                .set_read_timeout(Some(Duration::from_secs(20)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0; 8192];
            let header_end = loop {
                let count = connection.read(&mut buffer).unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&buffer[..count]);
                if let Some(index) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                    break index + 4;
                }
                assert!(bytes.len() < 65536);
            };
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_lowercase()
                        .strip_prefix("content-length:")
                        .map(|n| n.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            while bytes.len() < header_end + content_length {
                let count = connection.read(&mut buffer).unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&buffer[..count]);
            }
            let request: Value =
                serde_json::from_slice(&bytes[header_end..header_end + content_length]).unwrap();
            saved.lock().unwrap().push(request);
            let delta = if index == 0 {
                json!({"tool_calls":[{"index":0,"id":"fixture-call-1","type":"function","function":{"name":tool,"arguments":args.to_string()}}]})
            } else {
                let final_text = if tool == "file_write" {
                    "The write was declined. I did not change sample.txt."
                } else {
                    "The contents of sample.txt are: durable fixture contents."
                };
                json!({"content":final_text})
            };
            let first = json!({"id":"fixture","object":"chat.completion.chunk","model":"fixture","choices":[{"index":0,"delta":delta,"finish_reason":null}]});
            let last = json!({"id":"fixture","object":"chat.completion.chunk","model":"fixture","choices":[{"index":0,"delta":{},"finish_reason":if index==0{"tool_calls"}else{"stop"}}],"usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120}});
            let body = format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n");
            write!(connection,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
            connection.flush().unwrap();
        }
    });
    (endpoint, requests)
}

fn turn(session: Uuid, workspace: &std::path::Path, endpoint: String) -> TurnRequest {
    TurnRequest {
        session_id: session,
        turn_id: Uuid::new_v4(),
        prompt: "Read sample.txt using the file_read tool, then report its content.".into(),
        workspace: workspace.to_owned(),
        provider: ProviderConfig {
            endpoint,
            model: "fixture".into(),
            ..Default::default()
        },
        permissions: PermissionMode::AskForWrites,
        attachments: Vec::new(),
    }
}

#[tokio::test]
async fn real_worker_streams_executes_and_checkpoints() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("sample.txt"),
        "durable fixture contents",
    )
    .unwrap();
    let (endpoint, requests) = provider_fixture("file_read", json!({"path":"sample.txt"}));
    let mut worker = Worker::start().await;
    let session = Uuid::new_v4();
    let request = turn(session, workspace.path(), endpoint);
    worker
        .send(
            session,
            WorkerCommand::Run {
                turn: request.clone(),
            },
        )
        .await;
    let mut tool_finished = false;
    let mut streamed = false;
    let mut last_sequence = 0;
    loop {
        let event = worker.event().await;
        assert!(event.sequence > last_sequence);
        last_sequence = event.sequence;
        match event.payload {
            EngineEvent::Delta { text } => streamed |= !text.is_empty(),
            EngineEvent::ToolFinished {
                success, result, ..
            } => {
                assert!(success);
                assert!(result.contains("durable fixture contents"));
                tool_finished = true;
            }
            EngineEvent::Completed { text, .. } => {
                assert!(text.contains("durable fixture contents"));
                break;
            }
            EngineEvent::Error { message, .. } => panic!("Worker failed: {message}"),
            _ => {}
        }
    }
    assert!(tool_finished && streamed);
    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[1]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["role"] == "tool")
        );
    }
    // A completion acknowledgement permits the very next Run immediately. The
    // final event must not race the old task's JoinHandle completion flag.
    for _ in 0..32 {
        worker
            .send(
                session,
                WorkerCommand::Run {
                    turn: request.clone(),
                },
            )
            .await;
        loop {
            match worker.event().await.payload {
                EngineEvent::Completed { .. } => break,
                EngineEvent::Error { message, .. } => {
                    panic!("Immediate next turn was rejected: {message}")
                }
                EngineEvent::ToolStarted { .. } => panic!("Receipt replay repeated an action"),
                _ => {}
            }
        }
    }
    // The fixture has closed its listener. A newly started process can only
    // answer this replay by loading the committed completion receipt.
    worker = worker.restart().await;
    worker
        .send(session, WorkerCommand::Run { turn: request })
        .await;
    loop {
        match worker.event().await.payload {
            EngineEvent::Completed { text, .. } => {
                assert!(text.contains("durable fixture contents"));
                break;
            }
            EngineEvent::Error { message, .. } => panic!("Receipt replay failed: {message}"),
            EngineEvent::ToolStarted { .. } => {
                panic!("A completed action was repeated after restart")
            }
            _ => {}
        }
    }
    assert_eq!(requests.lock().unwrap().len(), 2);
    worker.stop().await;
}

#[tokio::test]
async fn authorized_worker_does_not_read_the_platform_vault() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("sample.txt"),
        "durable fixture contents",
    )
    .unwrap();
    let (endpoint, _) = provider_fixture("file_read", json!({"path":"sample.txt"}));
    let mut worker = Worker::start().await;
    let session = Uuid::new_v4();
    let mut request = turn(session, workspace.path(), endpoint);
    request.provider.credential_ref = Some(format!("nonexistent-{}", Uuid::new_v4()));
    worker
        .send(
            session,
            WorkerCommand::RunAuthorized {
                turn: request,
                credential: WorkerCredential(Some("fixture-private-secret".into())),
            },
        )
        .await;
    loop {
        let event = worker.event().await;
        assert!(
            !serde_json::to_string(&event)
                .unwrap()
                .contains("fixture-private-secret")
        );
        match event.payload {
            EngineEvent::Completed { .. } => break,
            EngineEvent::Error { message, .. } => panic!("Authorized worker failed: {message}"),
            _ => {}
        }
    }
    worker.stop().await;
}

#[tokio::test]
async fn denied_write_never_changes_the_workspace() {
    let workspace = tempfile::tempdir().unwrap();
    let (endpoint, _requests) = provider_fixture(
        "file_write",
        json!({"path":"sample.txt","content":"must not be written"}),
    );
    let mut worker = Worker::start().await;
    let session = Uuid::new_v4();
    let mut request = turn(session, workspace.path(), endpoint);
    request.prompt = "Use file_write to create sample.txt with the requested content.".into();
    worker
        .send(session, WorkerCommand::Run { turn: request })
        .await;
    let mut denied = false;
    loop {
        match worker.event().await.payload {
            EngineEvent::ApprovalRequired { action_id, .. } => {
                worker
                    .send(
                        session,
                        WorkerCommand::Approve {
                            action_id,
                            allow: false,
                            remember: false,
                        },
                    )
                    .await;
                denied = true;
            }
            EngineEvent::Completed { .. } => break,
            EngineEvent::Error { message, .. } => panic!("Worker failed: {message}"),
            _ => {}
        }
    }
    assert!(denied);
    assert!(!workspace.path().join("sample.txt").exists());
    worker.stop().await;
}
