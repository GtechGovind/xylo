//! Repeatable headless application-overhead measurements against a scripted
//! provider. This intentionally does not claim GUI, model-quality or GPU results.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workbench_core::*;
use workbench_services::{SqliteMemory, Store};

struct Worker {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}
impl Worker {
    async fn start(executable: &Path, data: &Path) -> Result<Self> {
        let mut child = tokio::process::Command::new(executable)
            .arg("--data-dir")
            .arg(data)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let input = child.stdin.take().context("Worker input missing")?;
        let output = BufReader::new(child.stdout.take().context("Worker output missing")?);
        let mut worker = Self {
            child,
            input,
            output,
        };
        ensure!(
            matches!(worker.event().await?.payload, EngineEvent::Ready { .. }),
            "Worker did not become ready"
        );
        Ok(worker)
    }
    async fn send(&mut self, session: Uuid, command: WorkerCommand) -> Result<Uuid> {
        let id = Uuid::new_v4();
        let mut bytes = serde_json::to_vec(&Envelope::new(id, Some(session), 0, command))?;
        bytes.push(b'\n');
        self.input.write_all(&bytes).await?;
        self.input.flush().await?;
        Ok(id)
    }
    async fn event(&mut self) -> Result<Envelope<EngineEvent>> {
        let mut bytes = Vec::new();
        let read = tokio::time::timeout(
            Duration::from_secs(30),
            (&mut self.output)
                .take((MAX_PROTOCOL_BYTES + 1) as u64)
                .read_until(b'\n', &mut bytes),
        )
        .await??;
        ensure!(
            read > 0 && read <= MAX_PROTOCOL_BYTES,
            "Worker closed or exceeded protocol size"
        );
        let event: Envelope<EngineEvent> = serde_json::from_slice(&bytes)?;
        event.validate()?;
        Ok(event)
    }
    async fn stop(&mut self) -> Result<()> {
        self.send(Uuid::nil(), WorkerCommand::Shutdown).await?;
        let status = tokio::time::timeout(Duration::from_secs(10), self.child.wait()).await??;
        ensure!(status.success(), "Worker shutdown failed");
        Ok(())
    }
}

async fn fixture(cancel: CancellationToken) -> Result<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/v1", listener.local_addr()?);
    tokio::spawn(async move {
        loop {
            let connection = tokio::select! {_ = cancel.cancelled()=>break,connection=listener.accept()=>connection};
            let Ok((socket, _)) = connection else {
                break;
            };
            let cancel = cancel.clone();
            tokio::spawn(async move {
                if let Err(error) = respond(socket, cancel).await {
                    eprintln!("Scripted provider: {error}");
                }
            });
        }
    });
    Ok(endpoint)
}

async fn respond(socket: tokio::net::TcpStream, cancel: CancellationToken) -> Result<()> {
    let mut socket = BufReader::new(socket);
    let mut length = None;
    let mut headers = 0;
    loop {
        let mut line = Vec::new();
        let read = tokio::time::timeout(
            Duration::from_secs(5),
            (&mut socket).take(65537).read_until(b'\n', &mut line),
        )
        .await??;
        headers += read;
        ensure!(read > 0 && headers <= 65536, "Invalid fixture HTTP header");
        if line == b"\r\n" {
            break;
        }
        let line = String::from_utf8(line)?;
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = Some(value.trim().parse::<usize>()?);
        }
    }
    let length = length.context("Fixture requires HTTP content length")?;
    ensure!(length <= 2 * 1024 * 1024, "Fixture request too large");
    let mut body = vec![0; length];
    socket.read_exact(&mut body).await?;
    let request: Value = serde_json::from_slice(&body)?;
    let messages = request
        .get("messages")
        .and_then(Value::as_array)
        .context("Fixture request has no messages")?;
    let cancelling = messages.iter().any(|message| {
        message.get("role") == Some(&json!("user"))
            && message
                .get("content")
                .and_then(Value::as_str)
                .is_some_and(|text| text.contains("benchmark-cancel"))
    });
    let has_tool = messages
        .iter()
        .any(|message| message.get("role") == Some(&json!("tool")));
    let delta = if cancelling {
        json!({"content":"Waiting for cancellation"})
    } else if has_tool {
        json!({"content":"Read benchmark contents."})
    } else {
        json!({"tool_calls":[{"index":0,"id":"benchmark-read","type":"function","function":{"name":"file_read","arguments":"{\"path\":\"sample.txt\"}"}}]})
    };
    let first = json!({"id":"benchmark","object":"chat.completion.chunk","model":"scripted-provider","choices":[{"index":0,"delta":delta,"finish_reason":null}]});
    let mut socket = socket.into_inner();
    if cancelling {
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await?;
        let body = format!("data: {first}\n\n");
        socket
            .write_all(format!("{:x}\r\n{}\r\n", body.len(), body).as_bytes())
            .await?;
        socket.flush().await?;
        let mut byte = [0; 1];
        tokio::select! {_ = cancel.cancelled()=>{},_ = socket.read(&mut byte)=>{}};
    } else {
        let last = json!({"id":"benchmark","object":"chat.completion.chunk","model":"scripted-provider","choices":[{"index":0,"delta":{},"finish_reason":if has_tool{"stop"}else{"tool_calls"}}],"usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120}});
        let body = format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n");
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).as_bytes()).await?;
        socket.shutdown().await?;
    }
    Ok(())
}

fn statistics(samples: &[f64]) -> Value {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let percentile = |p: f64| {
        sorted[((sorted.len() as f64 * p).ceil() as usize)
            .saturating_sub(1)
            .min(sorted.len() - 1)]
    };
    json!({"unit":"ms","count":samples.len(),"p50":percentile(0.5),"p95":percentile(0.95),"min":sorted[0],"max":sorted[sorted.len()-1],"raw":samples})
}
fn record(metrics: &mut BTreeMap<String, Vec<f64>>, name: &str, start: Instant) {
    metrics
        .entry(name.into())
        .or_default()
        .push(start.elapsed().as_secs_f64() * 1000.);
}
fn option(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == name)
        .and_then(|index| args.get(index + 1))
        .cloned()
}

/// The OS ledger is distinct from RSS. This remains one worker observation;
/// it deliberately does not claim an app/process-tree memory budget.
#[cfg(target_os = "macos")]
fn physical_footprint(pid: u32) -> Option<u64> {
    let pid = i32::try_from(pid).ok()?;
    let mut usage = std::mem::MaybeUninit::<libc::rusage_info_v2>::zeroed();
    // SAFETY: V2 writes exactly rusage_info_v2 to the aligned, writable buffer.
    // Its value is observed only when libproc reports success.
    let result =
        unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V2, usage.as_mut_ptr().cast()) };
    if result == 0 {
        Some(unsafe { usage.assume_init() }.ri_phys_footprint)
    } else {
        None
    }
}
#[cfg(not(target_os = "macos"))]
fn physical_footprint(_pid: u32) -> Option<u64> {
    None
}

fn digest_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn embedding_provenance(config: Option<&Path>) -> Result<Value> {
    let Some(config) = config else {
        return Ok(Value::Null);
    };
    let bytes = std::fs::read(config)?;
    let assets: workbench_services::EmbeddingAssets = serde_json::from_slice(&bytes)?;
    let model = assets.directory.join("model.onnx");
    let runtime = assets.runtime_library.unwrap_or_else(|| {
        assets
            .directory
            .join(workbench_services::EmbeddingAssets::runtime_filename())
    });
    Ok(
        json!({"model_id":assets.model_id,"dimensions":assets.dimensions,"pooling":assets.pooling,"configured_cpu_threads":assets.cpu_threads,"configuration_sha256":format!("{:x}",Sha256::digest(bytes)),"model_sha256":digest_file(&model)?,"model_bytes":std::fs::metadata(model)?.len(),"runtime_sha256":digest_file(&runtime)?}),
    )
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let executable = PathBuf::from(option(&args, "--worker").context("--worker PATH is required")?);
    let output = PathBuf::from(option(&args, "--output").context("--output PATH is required")?);
    let samples = option(&args, "--samples")
        .unwrap_or_else(|| "100".into())
        .parse::<usize>()?
        .clamp(10, 1000);
    let memories = option(&args, "--memories")
        .unwrap_or_else(|| "10000".into())
        .parse::<usize>()?;
    ensure!(
        (100..=100000).contains(&memories),
        "Choose between 100 and 100,000 fixture memories"
    );
    let profile = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    tokio::fs::write(workspace.path().join("sample.txt"), "benchmark contents").await?;
    let stop = CancellationToken::new();
    let endpoint = fixture(stop.clone()).await?;
    let mut metrics = BTreeMap::new();
    for _ in 0..10 {
        let cold = tempfile::tempdir()?;
        let start = Instant::now();
        let mut worker = Worker::start(&executable, cold.path()).await?;
        record(&mut metrics, "cold_worker_ready", start);
        worker.stop().await?;
    }
    let mut worker = Worker::start(&executable, profile.path()).await?;
    for _ in 0..samples {
        let start = Instant::now();
        let id = worker.send(Uuid::nil(), WorkerCommand::Hello).await?;
        let event = worker.event().await?;
        ensure!(
            event.request_id == id && matches!(event.payload, EngineEvent::Ready { .. }),
            "Uncorrelated worker response"
        );
        record(&mut metrics, "warm_worker_jsonl_roundtrip", start);
    }
    for index in 0..samples {
        let session = Uuid::new_v4();
        let cancel_run = index % 5 == 0;
        let turn = TurnRequest {
            session_id: session,
            turn_id: Uuid::new_v4(),
            workspace: workspace.path().into(),
            prompt: if cancel_run {
                "benchmark-cancel"
            } else {
                "Read sample.txt using file_read"
            }
            .into(),
            provider: ProviderConfig {
                endpoint: endpoint.clone(),
                model: "scripted-provider".into(),
                ..Default::default()
            },
            permissions: PermissionMode::AskForWrites,
            attachments: Vec::new(),
        };
        let start = Instant::now();
        worker.send(session, WorkerCommand::Run { turn }).await?;
        let mut tool_started = None;
        let mut cancel_started = None;
        let mut cancel_id = None;
        let mut tool_finished = false;
        let mut cancelled = false;
        let mut acknowledged = false;
        loop {
            let event = worker.event().await?;
            match event.payload {
                EngineEvent::Started => record(&mut metrics, "worker_turn_started", start),
                EngineEvent::Delta { text: _ } if cancel_run && cancel_started.is_none() => {
                    let started = Instant::now();
                    let id = worker.send(session, WorkerCommand::Cancel).await?;
                    cancel_started = Some(started);
                    cancel_id = Some(id);
                }
                EngineEvent::Status { .. }
                    if Some(event.request_id) == cancel_id && !acknowledged =>
                {
                    record(
                        &mut metrics,
                        "active_cancellation_ack",
                        cancel_started.context("Missing cancellation timer")?,
                    );
                    acknowledged = true;
                }
                EngineEvent::ToolStarted { .. } => {
                    tool_started = Some(Instant::now());
                }
                EngineEvent::ToolFinished { .. } => {
                    record(
                        &mut metrics,
                        "file_tool_roundtrip_including_journal",
                        tool_started.take().context("Missing tool start")?,
                    );
                    tool_finished = true;
                }
                EngineEvent::Completed { .. } => {
                    ensure!(
                        !cancel_run && tool_finished,
                        "Fixture turn did not execute its real tool"
                    );
                    record(&mut metrics, "scripted_file_tool_turn", start);
                    break;
                }
                EngineEvent::Cancelled => {
                    ensure!(cancel_run, "Unexpected cancellation");
                    cancelled = true;
                }
                EngineEvent::Error { message, .. } => {
                    anyhow::bail!("Benchmark worker failed: {message}")
                }
                _ => {}
            }
            if cancelled && acknowledged {
                break;
            }
        }
    }
    let mut system = sysinfo::System::new_all();
    system.refresh_all();
    let worker_rss = worker
        .child
        .id()
        .and_then(|id| system.process(sysinfo::Pid::from_u32(id)))
        .map(|process| process.memory());
    let worker_footprint = worker.child.id().and_then(physical_footprint);
    worker.stop().await?;
    stop.cancel();
    let store = Store::open(profile.path().join("memory-benchmark.db"))?;
    let embedding_config = option(&args, "--embedding-config").map(PathBuf::from);
    let memory = if let Some(config) = &embedding_config {
        ensure!(
            config.is_file(),
            "The selected embedding configuration does not exist"
        );
        SqliteMemory::configured(store.clone(), config)?
    } else {
        SqliteMemory::new(store.clone())
    };
    let seed = Instant::now();
    for index in 0..memories {
        memory.remember_sync(MemoryRecord {
            id: Uuid::new_v4(),
            scope: "benchmark".into(),
            content: format!(
                "Project decision {index}: use SQLite WAL for durable agent checkpoint storage."
            ),
            source: "generated-benchmark-fixture".into(),
            created_at: now_ms(),
            supersedes: None,
        })?;
    }
    let seed_ms = seed.elapsed().as_secs_f64() * 1000.;
    for _ in 0..5 {
        let _ = memory.recall_sync("benchmark", "SQLite durable checkpoint", 10)?;
    }
    for index in 0..samples {
        // Vary the query so embedding measurements include uncached query work.
        let query = format!("SQLite durable checkpoint decision {}", index % memories);
        let start = Instant::now();
        let result = memory.recall_sync("benchmark", &query, 10)?;
        ensure!(!result.is_empty(), "Fixture memory search returned no rows");
        record(
            &mut metrics,
            if embedding_config.is_some() {
                "warm_hybrid_retrieval_including_embedding"
            } else {
                "warm_lexical_retrieval"
            },
            start,
        );
    }
    let metrics: BTreeMap<_, _> = metrics
        .iter()
        .map(|(name, samples)| (name, statistics(samples)))
        .collect();
    let provenance_config = embedding_config.clone();
    let (worker_sha, embedding_provenance) = tokio::task::spawn_blocking(move || -> Result<_> {
        Ok((
            digest_file(&executable)?,
            embedding_provenance(provenance_config.as_deref())?,
        ))
    })
    .await??;
    let harness_sha = digest_file(&std::env::current_exe()?)?;
    let source_fingerprint = option(&args, "--source-fingerprint");
    if let Some(fingerprint) = &source_fingerprint {
        ensure!(
            fingerprint.len() == 64 && fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "Source fingerprint must be a SHA-256 hex digest"
        );
    }
    let load = sysinfo::System::load_average();
    let report = json!({"schema_version":2,"harness_binary_sha256":harness_sha,"source_fingerprint":source_fingerprint,"system_load_average":{"one":load.one,"five":load.five,"fifteen":load.fifteen},"measured_at_ms":now_ms(),"profile":if cfg!(debug_assertions){"debug"}else{"release"},"os":std::env::consts::OS,"os_version":sysinfo::System::long_os_version(),"architecture":std::env::consts::ARCH,"cpu_threads":std::thread::available_parallelism()?.get(),"total_memory_bytes":system.total_memory(),"worker_binary_sha256":worker_sha,"scripted_provider":true,"model":"scripted-provider","context_tokens":8192,"memory_count":memories,"memory_seed_ms":seed_ms,"embedding_enabled":embedding_config.is_some(),"embedding_provenance":embedding_provenance,"warm_worker_rss_bytes":worker_rss,"warm_worker_physical_footprint_bytes":worker_footprint,"metrics":metrics,"limitations":["Headless worker and storage measurements exclude the native GUI and application host routing.","RSS and macOS physical footprint are point-in-time samples of the worker alone; neither proves full app plus worker attributable RAM.","Worker ready time is not application launch-to-usable time.","Tool time includes native file read, journal and JSONL delivery; it is not isolated dispatch overhead.","No live model quality, network latency, browser, generation-model weights or KV-cache measurement is claimed."]});
    if let Some(parent) = output.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&output, serde_json::to_vec_pretty(&report)?).await?;
    println!("Saved {}", output.display());
    Ok(())
}
