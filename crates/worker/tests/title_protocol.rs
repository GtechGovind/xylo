//! A generated title never holds back a turn's result: the real worker sends
//! `Completed` as soon as the engine has it, forwards the title after it, and
//! abandons a still-running title when the next turn starts.

use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
};
use uuid::Uuid;
use xylo_core::*;

struct Worker {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    _profile: tempfile::TempDir,
}

impl Worker {
    /// Starts a worker whose profile holds an untitled conversation `session`.
    async fn start(session: Uuid) -> Self {
        let profile = tempfile::tempdir().unwrap();
        let store = xylo_services::Store::open(
            xylo_runtime::AppPaths::at(profile.path().to_owned())
                .unwrap()
                .database,
        )
        .unwrap();
        store
            .upsert_conversation(&Conversation {
                id: session,
                title: "Say hello".into(),
                workspace: profile.path().to_owned(),
                updated_at: now_ms(),
                title_source: TitleSource::Prompt,
                pinned: false,
                archived: false,
            })
            .unwrap();
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_xylo-worker"))
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
            .send(
                Uuid::nil(),
                WorkerCommand::Unlock {
                    key: WorkerCredential(Some("00".repeat(32))),
                },
            )
            .await;
        worker
    }

    async fn run(&mut self, turn: TurnRequest) {
        self.send(turn.session_id, WorkerCommand::Run { turn })
            .await;
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
        serde_json::from_str(&line).unwrap()
    }

    /// Events up to and including the turn's terminal event.
    async fn until_terminal(&mut self) -> EngineEvent {
        loop {
            let event = self.event().await.payload;
            match event {
                EngineEvent::Completed { .. } => return event,
                EngineEvent::Error { message, .. } => panic!("Turn failed: {message}"),
                EngineEvent::Cancelled => panic!("Turn was cancelled"),
                _ => {}
            }
        }
    }

    async fn stop(mut self) {
        self.send(Uuid::nil(), WorkerCommand::Shutdown).await;
        let status = tokio::time::timeout(Duration::from_secs(10), self.child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(status.success());
    }
}

/// How the fixture answers title requests.
#[derive(Clone, Copy)]
enum TitleReply {
    /// After this delay.
    After(Duration),
    /// Never: the connection stays open until the worker closes it.
    Never,
}

/// Counters the fixture keeps.
#[derive(Default)]
struct Served {
    answers: AtomicUsize,
    title_answered: AtomicBool,
    titles_requested: AtomicUsize,
    titles_abandoned: AtomicUsize,
}

/// An OpenAI chat streaming fixture. Requests with tools are turns and get
/// "Hello there"; requests without tools are titles, answered per `title`.
/// Each connection runs on its own thread so a stalled title blocks nothing.
fn provider_fixture(title: TitleReply) -> (String, Arc<Served>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let served = Arc::new(Served::default());
    let state = served.clone();
    std::thread::spawn(move || {
        for connection in listener.incoming() {
            let Ok(connection) = connection else {
                return;
            };
            let state = state.clone();
            std::thread::spawn(move || answer(connection, title, &state));
        }
    });
    (endpoint, served)
}

fn answer(mut connection: TcpStream, title: TitleReply, served: &Served) {
    let request = read_request(&mut connection);
    let is_title = request["tools"].as_array().is_none_or(Vec::is_empty);
    let text = if is_title {
        served.titles_requested.fetch_add(1, Ordering::SeqCst);
        match title {
            TitleReply::After(delay) => std::thread::sleep(delay),
            TitleReply::Never => {
                // Wait for the worker to drop the request.
                let mut byte = [0; 1];
                if matches!(connection.read(&mut byte), Ok(0)) {
                    served.titles_abandoned.fetch_add(1, Ordering::SeqCst);
                }
                return;
            }
        }
        "Fixture title"
    } else {
        served.answers.fetch_add(1, Ordering::SeqCst);
        "Hello there, this is the fixture answer."
    };
    let first = json!({"id":"fixture","object":"chat.completion.chunk","model":"fixture","choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]});
    let last = json!({"id":"fixture","object":"chat.completion.chunk","model":"fixture","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}});
    let body = format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n");
    let _ = write!(
        connection,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = connection.flush();
    if is_title {
        served.title_answered.store(true, Ordering::SeqCst);
    }
}

fn read_request(connection: &mut TcpStream) -> Value {
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
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]).to_lowercase();
    let length = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .map(|n| n.trim().parse::<usize>().unwrap())
        .unwrap();
    while bytes.len() < header_end + length {
        let count = connection.read(&mut buffer).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buffer[..count]);
    }
    serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap()
}

fn turn(session: Uuid, endpoint: &str) -> TurnRequest {
    let workspace = std::env::temp_dir();
    TurnRequest {
        session_id: session,
        turn_id: Uuid::new_v4(),
        prompt: "Say hello".into(),
        workspace,
        provider: ProviderConfig {
            endpoint: endpoint.into(),
            model: "fixture".into(),
            ..Default::default()
        },
        permissions: PermissionMode::ReadOnly,
        attachments: Vec::new(),
        approved_rules: Vec::new(),
    }
}

#[tokio::test]
async fn completed_arrives_before_a_slow_title_which_follows_it() {
    let (endpoint, served) = provider_fixture(TitleReply::After(Duration::from_secs(2)));
    let session = Uuid::new_v4();
    let mut worker = Worker::start(session).await;
    worker
        .send(
            session,
            WorkerCommand::Run {
                turn: turn(session, &endpoint),
            },
        )
        .await;
    worker.until_terminal().await;
    assert!(
        !served.title_answered.load(Ordering::SeqCst),
        "Completed waited for the title"
    );
    loop {
        match worker.event().await.payload {
            EngineEvent::TitleUpdated { title } => {
                assert_eq!(title, "Fixture title");
                break;
            }
            EngineEvent::Steering { .. } => {}
            other => panic!("Unexpected event after Completed: {other:?}"),
        }
    }
    worker.stop().await;
}

#[tokio::test]
async fn the_next_turn_abandons_a_title_still_being_generated() {
    let (endpoint, served) = provider_fixture(TitleReply::Never);
    let session = Uuid::new_v4();
    let mut worker = Worker::start(session).await;
    worker.run(turn(session, &endpoint)).await;
    worker.until_terminal().await;
    wait_until(|| served.titles_requested.load(Ordering::SeqCst) == 1).await;
    worker.run(turn(session, &endpoint)).await;
    // A "busy" refusal would arrive here as an Error and fail the turn.
    worker.until_terminal().await;
    assert_eq!(served.answers.load(Ordering::SeqCst), 2);
    // Well before the title's own 10 s deadline: the new turn aborted it.
    wait_until(|| served.titles_abandoned.load(Ordering::SeqCst) >= 1).await;
    worker.stop().await;
}

async fn wait_until(condition: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("condition was not met in time");
}
