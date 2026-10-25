//! Actual worker streaming and restart against a deterministic local vision API.
use serde_json::{Value, json};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workbench_core::*;
use workbench_runtime::AppPaths;
use workbench_services::{Store, attachments::AttachmentStore};

struct Worker {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}
impl Worker {
    async fn start(paths: &AppPaths) -> Self {
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_workbench-worker"))
            .arg("--data-dir")
            .arg(&paths.data)
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
    async fn send(&mut self, session: Uuid, command: WorkerCommand) {
        let mut bytes =
            serde_json::to_vec(&Envelope::new(Uuid::new_v4(), Some(session), 0, command)).unwrap();
        assert!(
            bytes.len() < 16 * 1024,
            "Worker commands must carry references only"
        );
        bytes.push(b'\n');
        self.input.write_all(&bytes).await.unwrap();
        self.input.flush().await.unwrap();
    }
    async fn event(&mut self) -> Envelope<EngineEvent> {
        let mut line = String::new();
        assert!(
            tokio::time::timeout(Duration::from_secs(30), self.output.read_line(&mut line))
                .await
                .expect("worker response deadline")
                .unwrap()
                > 0
        );
        assert!(!line.contains("data:image"));
        serde_json::from_str(&line).unwrap()
    }
    async fn completed(&mut self) {
        let mut streamed = false;
        loop {
            match self.event().await.payload {
                EngineEvent::Delta { text } => streamed |= !text.is_empty(),
                EngineEvent::Completed { text, .. } => {
                    assert_eq!(text, "Image received.");
                    assert!(streamed);
                    return;
                }
                EngineEvent::Error { message, .. } => panic!("Vision turn failed: {message}"),
                _ => {}
            }
        }
    }
    async fn stop(mut self) {
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

async fn fixture() -> (String, tokio::task::JoinHandle<Vec<Value>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        let mut requests = Vec::new();
        for _ in 0..2 {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            let mut length = 0;
            loop {
                line.clear();
                socket.read_line(&mut line).await.unwrap();
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse::<usize>().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
            }
            assert!(length > 0 && length < 1024 * 1024);
            let mut bytes = vec![0; length];
            socket.read_exact(&mut bytes).await.unwrap();
            requests.push(serde_json::from_slice(&bytes).unwrap());
            let body = format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"delta":{"content":"Image received."},"finish_reason":null}]}),
                json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":4,"total_tokens":104}})
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            for bytes in response.as_bytes().chunks(7) {
                socket.get_mut().write_all(bytes).await.unwrap();
                tokio::task::yield_now().await;
            }
        }
        requests
    });
    (endpoint, handle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_worker_sends_managed_image_and_rehydrates_it_after_process_restart() {
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::at(directory.path().join("profile")).unwrap();
    let workspace = directory.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    let session = Uuid::new_v4();
    let images = AttachmentStore::new(paths.artifacts.clone()).unwrap();
    let image = images
        .import_bytes(
            session,
            "selected.png",
            include_bytes!("../../../packaging/icons/app.png"),
            AttachmentSource::Picker,
            &CancellationToken::new(),
        )
        .unwrap();
    let (endpoint, server) = fixture().await;
    let mut turn = TurnRequest {
        session_id: session,
        turn_id: Uuid::new_v4(),
        prompt: "Describe this image briefly without tools.".into(),
        workspace,
        provider: ProviderConfig {
            endpoint,
            model: "fixture".into(),
            context_tokens: 262144,
            output_tokens: 512,
            ..Default::default()
        },
        permissions: PermissionMode::ReadOnly,
        attachments: vec![image.clone()],
    };
    let mut worker = Worker::start(&paths).await;
    worker
        .send(session, WorkerCommand::Run { turn: turn.clone() })
        .await;
    worker.completed().await;
    worker.stop().await;
    let store = Store::open(&paths.database).unwrap();
    let messages = store.messages(session, 10).unwrap();
    assert_eq!(messages[0].attachments, vec![image.clone()]);
    let checkpoint = store.checkpoint(session).unwrap().unwrap();
    assert!(
        checkpoint
            .messages
            .to_string()
            .contains(&image.id.to_string())
    );
    assert!(!checkpoint.messages.to_string().contains("base64"));
    assert!(!checkpoint.messages.to_string().contains("data:image"));
    turn.turn_id = Uuid::new_v4();
    turn.prompt = "What image did I attach previously?".into();
    turn.attachments.clear();
    let mut restarted = Worker::start(&paths).await;
    restarted.send(session, WorkerCommand::Run { turn }).await;
    restarted.completed().await;
    restarted.stop().await;
    let requests = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests {
        let image = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|message| message["content"].as_array().into_iter().flatten())
            .find(|part| part["type"] == "image_url")
            .expect("Actual worker request lost its image");
        assert!(
            image["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
    }
    assert_eq!(
        store
            .messages(session, 10)
            .unwrap()
            .iter()
            .filter(|message| !message.attachments.is_empty())
            .count(),
        1
    );
}
