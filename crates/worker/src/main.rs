//! Supervised native task worker. Stdout is exclusively versioned JSONL; logs go
//! to stderr. One process executes one root task at a time.

mod steering;
use anyhow::{Context, Result, ensure};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter},
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;
use workbench_core::*;
use workbench_engine::{HermesEngine, tools::ApprovalGate};
use workbench_runtime::AppPaths;
use workbench_services::Store;

struct ActiveTask {
    session_id: uuid::Uuid,
    turn_id: uuid::Uuid,
    cancel: CancellationToken,
    engine: Arc<HermesEngine>,
    handle: tokio::task::JoinHandle<()>,
    busy: Arc<AtomicBool>,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--version") {
        println!(
            "Xylo worker {} / Hermes {}",
            env!("CARGO_PKG_VERSION"),
            HERMES_REVISION
        );
        return Ok(());
    }
    let paths = match args.iter().position(|arg| arg == "--data-dir") {
        Some(index) => AppPaths::at(PathBuf::from(
            args.get(index + 1).context("--data-dir requires a path")?,
        ))?,
        None => AppPaths::discover()?,
    };
    let store = Store::open(&paths.database)?;
    let gate = ApprovalGate::default();
    let (output, mut pending) = mpsc::channel::<Envelope<EngineEvent>>(EVENT_QUEUE_CAPACITY);
    let sequence = Arc::new(AtomicU64::new(1));
    let writer = tokio::spawn(async move {
        let mut stdout = BufWriter::new(tokio::io::stdout());
        while let Some(event) = pending.recv().await {
            let mut line = serde_json::to_vec(&event)?;
            ensure!(
                line.len() <= MAX_PROTOCOL_BYTES,
                "Worker output exceeds protocol budget"
            );
            line.push(b'\n');
            stdout.write_all(&line).await?;
            stdout.flush().await?;
        }
        Ok::<_, anyhow::Error>(())
    });
    send(
        &output,
        &sequence,
        uuid_nil(),
        None,
        EngineEvent::Ready {
            engine: "Hermes Ultra".into(),
            revision: HERMES_REVISION.into(),
        },
    )
    .await?;
    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut line = Vec::new();
    let mut active: Option<ActiveTask> = None;
    loop {
        line.clear();
        let length = (&mut stdin)
            .take((MAX_PROTOCOL_BYTES + 1) as u64)
            .read_until(b'\n', &mut line)
            .await?;
        if length == 0 {
            break;
        }
        if line.len() > MAX_PROTOCOL_BYTES {
            send(
                &output,
                &sequence,
                uuid_nil(),
                None,
                EngineEvent::Error {
                    message: "Command exceeds protocol size limit".into(),
                    retryable: false,
                },
            )
            .await?;
            break;
        }
        let envelope: Envelope<WorkerCommand> = match serde_json::from_slice(&line) {
            Ok(value) => value,
            Err(error) => {
                send(
                    &output,
                    &sequence,
                    uuid_nil(),
                    None,
                    EngineEvent::Error {
                        message: format!("Invalid worker command: {error}"),
                        retryable: false,
                    },
                )
                .await?;
                continue;
            }
        };
        if let Err(error) = envelope.validate() {
            send(
                &output,
                &sequence,
                envelope.request_id,
                envelope.session_id,
                EngineEvent::Error {
                    message: error.to_string(),
                    retryable: false,
                },
            )
            .await?;
            continue;
        }
        let request_id = envelope.request_id;
        let session_id = envelope.session_id;
        let (command, credential) = match envelope.payload {
            WorkerCommand::RunAuthorized { turn, credential } => {
                (WorkerCommand::Run { turn }, Some(credential))
            }
            command => (command, None),
        };
        match command {
            WorkerCommand::RunAuthorized { .. } => unreachable!(),
            WorkerCommand::Hello => {
                send(
                    &output,
                    &sequence,
                    request_id,
                    session_id,
                    EngineEvent::Ready {
                        engine: "Hermes Ultra".into(),
                        revision: HERMES_REVISION.into(),
                    },
                )
                .await?
            }
            WorkerCommand::Run { turn } => {
                if active
                    .as_ref()
                    .is_some_and(|task| task.busy.load(Ordering::Acquire))
                {
                    send(
                        &output,
                        &sequence,
                        request_id,
                        session_id,
                        EngineEvent::Error {
                            message: "Worker is busy; steer or cancel the current turn".into(),
                            retryable: true,
                        },
                    )
                    .await?;
                    continue;
                }
                let engine = HermesEngine::new(store.clone(), paths.clone(), gate.clone());
                let engine = Arc::new(match credential {
                    Some(credential) => engine.with_provider_credential(credential),
                    None => engine,
                });
                let cancel = CancellationToken::new();
                let run_engine = engine.clone();
                let run_cancel = cancel.clone();
                let run_output = output.clone();
                let run_sequence = sequence.clone();
                let run_store = store.clone();
                let terminal_store = store.clone();
                let task_session = turn.session_id;
                let task_turn = turn.turn_id;
                let busy = Arc::new(AtomicBool::new(true));
                let run_busy = busy.clone();
                let handle = tokio::spawn(async move {
                    let result = async {
                        let registered_turn = turn.clone();
                        tokio::task::spawn_blocking(move || {
                            run_store.register_user_turn(&registered_turn)
                        })
                        .await??;
                        let (events, mut incoming) = mpsc::channel(EVENT_QUEUE_CAPACITY);
                        let forward_output = run_output.clone();
                        let forward_sequence = run_sequence.clone();
                        let id = turn.session_id;
                        let forwarding = tokio::spawn(async move {
                            let mut terminal = None;
                            while let Some(event) = incoming.recv().await {
                                if matches!(
                                    event,
                                    EngineEvent::Completed { .. }
                                        | EngineEvent::Cancelled
                                        | EngineEvent::Error { .. }
                                ) {
                                    terminal = Some(event);
                                    continue;
                                }
                                send(
                                    &forward_output,
                                    &forward_sequence,
                                    request_id,
                                    Some(id),
                                    event,
                                )
                                .await?;
                            }
                            Ok::<_, anyhow::Error>(terminal)
                        });
                        let completion = run_engine.run(turn, events, run_cancel).await;
                        let terminal = forwarding.await??;
                        completion?;
                        terminal.context("Task ended without a final result")
                    }
                    .await;
                    // Completion becomes visible only after engine cleanup and
                    // event forwarding finish. A new turn can start as soon as
                    // the caller receives that terminal event, without racing
                    // JoinHandle::is_finished on the final send.
                    run_busy.store(false, Ordering::Release);
                    if let Err(error) = steering::finish(
                        terminal_store,
                        task_session,
                        task_turn,
                        &run_output,
                        &run_sequence,
                    )
                    .await
                    {
                        tracing::error!(%error,"Persist undelivered guidance outcome");
                    }
                    let event = match result {
                        Ok(event) => event,
                        Err(error) => EngineEvent::Error {
                            message: format!("{error:#}"),
                            retryable: true,
                        },
                    };
                    let _ = send(&run_output, &run_sequence, request_id, session_id, event).await;
                });
                active = Some(ActiveTask {
                    session_id: task_session,
                    turn_id: task_turn,
                    cancel,
                    engine,
                    handle,
                    busy,
                });
            }
            WorkerCommand::Cancel => {
                if let Some(task) = &active {
                    task.cancel.cancel();
                }
                send(
                    &output,
                    &sequence,
                    request_id,
                    session_id,
                    EngineEvent::Status {
                        text: "Cancellation requested; new tool dispatch stopped".into(),
                    },
                )
                .await?;
            }
            WorkerCommand::Steer { .. } => {
                send(
                    &output,
                    &sequence,
                    request_id,
                    session_id,
                    EngineEvent::Status {
                        text: "Use acknowledged guidance to update this task".into(),
                    },
                )
                .await?;
            }
            WorkerCommand::SteerDurable { guidance } => {
                let receipt = steering::receive(store.clone(), active.as_ref(), guidance).await?;
                send(
                    &output,
                    &sequence,
                    request_id,
                    Some(receipt.session_id),
                    EngineEvent::Steering { receipt },
                )
                .await?;
            }
            WorkerCommand::Approve { action_id, allow } => {
                if let Err(error) = gate.decide(action_id, allow) {
                    send(
                        &output,
                        &sequence,
                        request_id,
                        session_id,
                        EngineEvent::Error {
                            message: error.to_string(),
                            retryable: false,
                        },
                    )
                    .await?;
                }
            }
            WorkerCommand::Shutdown => break,
        }
    }
    if let Some(mut task) = active {
        task.cancel.cancel();
        if tokio::time::timeout(Duration::from_secs(5), &mut task.handle)
            .await
            .is_err()
        {
            task.handle.abort();
        }
    }
    drop(output);
    writer.await??;
    Ok(())
}

fn uuid_nil() -> uuid::Uuid {
    uuid::Uuid::nil()
}

async fn send(
    output: &mpsc::Sender<Envelope<EngineEvent>>,
    sequence: &AtomicU64,
    request_id: uuid::Uuid,
    session_id: Option<uuid::Uuid>,
    event: EngineEvent,
) -> Result<()> {
    output
        .send(Envelope::new(
            request_id,
            session_id,
            sequence.fetch_add(1, Ordering::Relaxed),
            event,
        ))
        .await
        .context("Worker output closed")
}
