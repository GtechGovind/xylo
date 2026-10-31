//! Supervised native task worker. Stdout is exclusively versioned JSONL; logs go
//! to stderr. One process executes one root task at a time.
//!
//! The host sends `Unlock` with the secret-vault key before any `Run`; the
//! worker never opens the OS credential store itself. `SetPermissions`
//! updates the live permission mode every broker reads at each tool call.

mod credentials;
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
use xylo_core::*;
use xylo_engine::{
    AgentEngine,
    tools::{PermissionCell, ToolSession},
};
use xylo_runtime::AppPaths;
use xylo_services::{CredentialVault, Store, VaultKey};

struct ActiveTask {
    session_id: uuid::Uuid,
    turn_id: uuid::Uuid,
    cancel: CancellationToken,
    engine: Arc<AgentEngine>,
    handle: tokio::task::JoinHandle<()>,
    busy: Arc<AtomicBool>,
    credentials: Arc<credentials::CredentialBroker>,
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
            "Xylo worker {} (protocol {PROTOCOL_VERSION})",
            env!("CARGO_PKG_VERSION")
        );
        return Ok(());
    }
    let paths = match args.iter().position(|arg| arg == "--data-dir") {
        Some(index) => AppPaths::at(PathBuf::from(
            args.get(index + 1).context("--data-dir requires a path")?,
        ))?,
        None => AppPaths::discover()?,
    };
    let store = Store::open_existing(&paths.database)?;
    let permissions = PermissionCell::default();
    let mut session: Option<ToolSession> = None;
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
    send(&output, &sequence, uuid_nil(), None, ready()).await?;
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
                    kind: ErrorKind::Internal,
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
                        kind: ErrorKind::Internal,
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
                    kind: ErrorKind::Internal,
                },
            )
            .await?;
            continue;
        }
        let request_id = envelope.request_id;
        let session_id = envelope.session_id;
        if let WorkerCommand::ProviderCredentialUpdated {
            request_id: renewal_id,
            result,
        } = &envelope.payload
        {
            if let Some(task) = &active
                && !task.cancel.is_cancelled()
            {
                task.credentials
                    .reply(request_id, session_id, *renewal_id, result.clone());
            }
            continue;
        }
        let (command, credential) = match envelope.payload {
            WorkerCommand::RunAuthorized { turn, credential } => {
                (WorkerCommand::Run { turn }, Some(credential))
            }
            command => (command, None),
        };
        match command {
            WorkerCommand::RunAuthorized { .. }
            | WorkerCommand::ProviderCredentialUpdated { .. } => unreachable!(),
            WorkerCommand::Hello => {
                send(&output, &sequence, request_id, session_id, ready()).await?
            }
            WorkerCommand::Run { turn } => {
                let Some(tools) = session.clone() else {
                    send(
                        &output,
                        &sequence,
                        request_id,
                        session_id,
                        EngineEvent::Error {
                            message: "Worker is locked: the host must send Unlock before Run"
                                .into(),
                            retryable: false,
                            kind: ErrorKind::Internal,
                        },
                    )
                    .await?;
                    continue;
                };
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
                            kind: ErrorKind::Internal,
                        },
                    )
                    .await?;
                    continue;
                }
                // The previous turn already reported its result; whatever it
                // still does (generating a title) is best-effort and yields.
                if let Some(previous) = active.take() {
                    previous.handle.abort();
                }
                let cancel = CancellationToken::new();
                let credentials = Arc::new(credentials::CredentialBroker::new(
                    output.clone(),
                    sequence.clone(),
                    request_id,
                    turn.session_id,
                ));
                // The admitted turn carries the mode current at admission;
                // later changes arrive as SetPermissions.
                permissions.set(turn.permissions);
                let engine = AgentEngine::new(store.clone(), paths.clone(), tools);
                let engine = Arc::new(match credential {
                    Some(credential) => engine
                        .with_provider_credential(credential)
                        .with_provider_credential_resolver(credentials.clone()),
                    None => engine,
                });
                let run_engine = engine.clone();
                let run_cancel = cancel.clone();
                let run_store = store.clone();
                let task_session = turn.session_id;
                let task_turn = turn.turn_id;
                let busy = Arc::new(AtomicBool::new(true));
                let ending = RunEnding {
                    busy: busy.clone(),
                    store: store.clone(),
                    session: task_session,
                    turn: task_turn,
                    output: output.clone(),
                    sequence: sequence.clone(),
                    request_id,
                    session_id,
                };
                let handle = tokio::spawn(async move {
                    let registered_turn = turn.clone();
                    let registered = tokio::task::spawn_blocking(move || {
                        run_store.register_user_turn(&registered_turn)
                    })
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|result| result);
                    if let Err(error) = registered {
                        let _ = ending.end(failure(error)).await;
                        return;
                    }
                    let (events, mut incoming) = mpsc::channel(EVENT_QUEUE_CAPACITY);
                    let forward_ending = ending.clone();
                    let forwarding = tokio::spawn(async move {
                        let mut ended = false;
                        while let Some(event) = incoming.recv().await {
                            if is_terminal(&event) {
                                if !ended {
                                    forward_ending.end(event).await?;
                                    ended = true;
                                }
                                continue;
                            }
                            send(
                                &forward_ending.output,
                                &forward_ending.sequence,
                                request_id,
                                Some(forward_ending.session),
                                event,
                            )
                            .await?;
                        }
                        Ok::<_, anyhow::Error>(ended)
                    });
                    let completion = run_engine.run(turn, events, run_cancel).await;
                    let ended = match forwarding.await {
                        Ok(Ok(ended)) => ended,
                        Ok(Err(error)) => {
                            tracing::error!(%error, "Forward task events");
                            return;
                        }
                        Err(error) => {
                            tracing::error!(%error, "Task event forwarding stopped");
                            false
                        }
                    };
                    match (ended, completion) {
                        (true, Ok(())) => {}
                        (true, Err(error)) => {
                            tracing::warn!(%error, "Task failed after its final result");
                        }
                        (false, completion) => {
                            let error = completion.err().unwrap_or_else(|| {
                                anyhow::anyhow!("Task ended without a final result")
                            });
                            let _ = ending.end(failure(error)).await;
                        }
                    }
                });
                active = Some(ActiveTask {
                    session_id: task_session,
                    turn_id: task_turn,
                    cancel,
                    engine,
                    handle,
                    busy,
                    credentials,
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
            WorkerCommand::SetPermissions { mode } => permissions.set(mode),
            WorkerCommand::Unlock { key } => match unlock(&paths, &key) {
                Ok(vault) => {
                    // A repeated Unlock replaces the vault; close the old
                    // session's connections, which hold the old secrets.
                    if let Some(previous) = session.take() {
                        previous.shutdown().await;
                    }
                    session = Some(ToolSession::new(permissions.clone(), vault));
                }
                Err(error) => {
                    send(
                        &output,
                        &sequence,
                        request_id,
                        session_id,
                        EngineEvent::Error {
                            message: format!("Could not unlock the secret vault: {error:#}"),
                            retryable: false,
                            kind: ErrorKind::Authentication,
                        },
                    )
                    .await?
                }
            },
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
            WorkerCommand::Approve {
                action_id,
                allow,
                remember,
            } => {
                let decided = session
                    .as_ref()
                    .context("Worker is locked; no action is waiting for approval")
                    .and_then(|tools| tools.approvals().decide_with(action_id, allow, remember));
                if let Err(error) = decided {
                    // Answered on the Approve's own request: the host forwards
                    // non-Run errors to the session, and the active run's id
                    // would make this look like the turn's terminal error.
                    send(
                        &output,
                        &sequence,
                        request_id,
                        session_id,
                        EngineEvent::Error {
                            message: format!("Approval for action {action_id} failed: {error:#}"),
                            retryable: false,
                            kind: ErrorKind::Internal,
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
    if let Some(tools) = session {
        tools.shutdown().await;
    }
    drop(output);
    writer.await??;
    Ok(())
}

/// Reports the end of one run: marks the worker free, settles guidance that
/// never reached the turn, then sends the terminal event. The terminal event
/// leaves as soon as the engine produces it; only best-effort work (a title)
/// may still follow, and the next `Run` aborts it.
#[derive(Clone)]
struct RunEnding {
    busy: Arc<AtomicBool>,
    store: Store,
    session: uuid::Uuid,
    turn: uuid::Uuid,
    output: mpsc::Sender<Envelope<EngineEvent>>,
    sequence: Arc<AtomicU64>,
    request_id: uuid::Uuid,
    session_id: Option<uuid::Uuid>,
}

impl RunEnding {
    async fn end(&self, terminal: EngineEvent) -> Result<()> {
        // Cleared first so a new turn can start as soon as the host has the
        // terminal event.
        self.busy.store(false, Ordering::Release);
        if let Err(error) = steering::finish(
            self.store.clone(),
            self.session,
            self.turn,
            &self.output,
            &self.sequence,
        )
        .await
        {
            tracing::error!(%error, "Persist undelivered guidance outcome");
        }
        send(
            &self.output,
            &self.sequence,
            self.request_id,
            self.session_id,
            terminal,
        )
        .await
    }
}

fn is_terminal(event: &EngineEvent) -> bool {
    matches!(
        event,
        EngineEvent::Completed { .. } | EngineEvent::Cancelled | EngineEvent::Error { .. }
    )
}

fn failure(error: anyhow::Error) -> EngineEvent {
    EngineEvent::Error {
        message: format!("{error:#}"),
        retryable: true,
        kind: ErrorKind::Internal,
    }
}

fn ready() -> EngineEvent {
    EngineEvent::Ready {
        version: env!("CARGO_PKG_VERSION").into(),
    }
}

fn unlock(paths: &AppPaths, key: &WorkerCredential) -> Result<CredentialVault> {
    CredentialVault::unlocked(&paths.data, VaultKey::try_from(key)?)
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
