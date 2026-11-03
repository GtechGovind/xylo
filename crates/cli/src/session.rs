//! Session orchestration over the runtime's exclusive profile and engine broker.
use crate::options::{Format, Options};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    io::Write,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use xylo_core::{
    EngineBackend, EngineEvent, ErrorKind, ProviderConfig, TurnRequest, WorkerCredential,
    stdio::{Command, Event, Input, VERSION},
};
use xylo_engine::{
    AgentEngine, ModelProvider,
    tools::{PermissionCell, ToolSession},
};
use xylo_runtime::{headless::Profile, observability::Observability};

type Replies = Arc<Mutex<HashMap<Uuid, Option<std::result::Result<Value, String>>>>>;

pub async fn serve(profile: &Profile, options: &Options, telemetry: &Observability) -> Result<i32> {
    let conversation = profile.conversation(options.resume, options.latest, &options.cwd)?;
    if (options.resume.is_some() || options.latest)
        && options.cwd != conversation.workspace
        && std::env::args().any(|arg| arg == "--cwd")
    {
        bail!("A resumed conversation keeps its original workspace");
    }
    let mode = profile.permission_mode(options.permission);
    let vault = profile.vault()?;
    let tools = ToolSession::new(PermissionCell::new(mode), vault.clone());
    let (commands, mut input) = mpsc::channel::<Result<Command>>(32);
    let input_reader = if options.stream_input {
        Some(tokio::spawn(read_commands(commands)))
    } else {
        None
    };
    let mut run_options = options.run.clone();
    run_options.interactive = options.stream_input;
    let replies: Replies = Arc::default();
    run_options.custom_handler = Some(custom_handler(replies.clone()));
    let policy_refused = Arc::new(AtomicBool::new(false));
    run_options.on_refusal = Some({
        let policy_refused = policy_refused.clone();
        Arc::new(move |reason| {
            eprintln!("Refused: {reason}");
            policy_refused.store(true, Ordering::Release);
        })
    });
    let mut sequence = 0;
    let mut status = 0;
    loop {
        let command = if options.stream_input {
            match input.recv().await {
                Some(command) => command?,
                None => break,
            }
        } else {
            Command::Run {
                prompt: options.prompt.clone().context("A prompt is required")?,
            }
        };
        let prompt = match command {
            Command::Run { prompt } => prompt,
            Command::RegisterTool { definition } => {
                ensure!(
                    run_options.custom_tools.len() < 128,
                    "Custom tool limit exceeded"
                );
                ensure!(
                    !run_options
                        .custom_tools
                        .iter()
                        .any(|tool| tool.name == definition.name),
                    "Duplicate custom tool {}",
                    definition.name
                );
                run_options.custom_tools.push(definition);
                continue;
            }
            Command::Shutdown => break,
            _ => bail!("Only run, register_tool or shutdown is valid while the session is idle"),
        };
        ensure!(!prompt.trim().is_empty(), "The prompt must not be empty");
        let span = tracing::info_span!("headless.run");
        policy_refused.store(false, Ordering::Release);
        let mut provider = turn_provider(&profile.settings, options.model.as_deref());
        let script = std::env::var_os("XYLO_EVAL_SCRIPT")
            .map(|path| -> Result<_> {
                Ok(serde_json::from_slice::<xylo_evals::scripted::Script>(
                    &std::fs::read(path)?,
                )?)
            })
            .transpose()?;
        if script.is_some() {
            provider = ProviderConfig {
                endpoint: "https://scripted.invalid/v1".into(),
                model: "scripted".into(),
                credential_ref: None,
                context_tokens: 32768,
                output_tokens: 4096,
                family: xylo_core::ApiFamily::OpenAiChat,
                ..ProviderConfig::default()
            };
        }
        let cancel = CancellationToken::new();
        let mut engine =
            AgentEngine::new(profile.store.clone(), profile.paths.clone(), tools.clone())
                .with_run_options(run_options.clone());
        if let Some(script) = script {
            let scripted: Arc<dyn ModelProvider> =
                Arc::new(xylo_evals::scripted::ScriptedProvider::new(script));
            engine = engine
                .with_provider_credential(WorkerCredential(Some("scripted-offline".into())))
                .with_provider_layer(Arc::new(move |_| scripted.clone()));
        } else {
            let accounts =
                xylo_services::providers::ChatGptSignInService::new(&profile.paths.data)?;
            let credential = accounts
                .resolve_provider_credential(&provider, vault.clone(), &cancel)
                .await?;
            engine = engine.with_provider_credential(WorkerCredential(credential));
        }
        let request = TurnRequest {
            session_id: conversation.id,
            turn_id: Uuid::new_v4(),
            prompt,
            workspace: conversation.workspace.clone(),
            provider,
            permissions: mode,
            attachments: Vec::new(),
        };
        let turn_id = request.turn_id;
        profile.store.register_user_turn(&request)?;
        let (events, mut incoming) = mpsc::channel(256);
        let running = tokio::spawn({
            let cancel = cancel.clone();
            use tracing::Instrument;
            async move { engine.run(request, events, cancel).await }.instrument(span.clone())
        });
        let mut terminal = None;
        let mut refused = false;
        let mut shutting_down = false;
        let mut waiting_approvals = HashSet::new();
        let mut waiting_interactions = HashSet::new();
        loop {
            tokio::select! {
                event = incoming.recv() => {
                    let Some(event) = event else { break; };
                    telemetry.event(conversation.id, turn_id, &event);
                    if let EngineEvent::ApprovalRequired { action_id, name, reason, .. } = &event {
                        waiting_approvals.insert(*action_id);
                        if !options.stream_input || shutting_down {
                            tools.approvals().decide_with(*action_id, false, false)?;
                            waiting_approvals.remove(action_id);
                            eprintln!("Refused {name}: {}. Use a permitted mode, saved approval rule, or a stream-json approval client.", reason.as_deref().unwrap_or("approval is required in non-interactive mode"));
                            refused = true;
                            cancel.cancel();
                        }
                    }
                    if let EngineEvent::QuestionRequired { action_id, .. } | EngineEvent::PlanRequired { action_id, .. } = &event {
                        if shutting_down { let _ = tools.answer(*action_id, xylo_core::InteractionAnswer::Reject); cancel.cancel(); }
                        else if options.stream_input { waiting_interactions.insert(*action_id); }
                    }
                    if let EngineEvent::CustomToolRequested { action_id, name, .. } = &event
                        && run_options.custom_tools.iter().any(|tool| tool.name == *name) {
                            replies.lock().map_err(|_| anyhow::anyhow!("Custom tool replies poisoned"))?.entry(*action_id).or_insert(None);
                            if shutting_down { eprintln!("Custom tool client disconnected before replying to {name}"); cancel.cancel(); }
                        }
                    refused |= policy_refused.load(Ordering::Acquire);
                    sequence += 1;
                    let frame = Event { version: VERSION, session_id: conversation.id, turn_id, sequence, exit_code: match &event { EngineEvent::Completed { .. } => Some(if refused { 2 } else { 0 }), EngineEvent::Error { .. } | EngineEvent::Cancelled => Some(if refused { 2 } else { 1 }), _ => None }, payload: event };
                    if options.output == Format::Stream { emit(&frame)?; }
                    if frame.is_terminal() { terminal = Some(frame.payload); break; }
                }
                command = input.recv(), if options.stream_input && !shutting_down => {
                    match command.transpose()? {
                        Some(Command::Approve { action_id, allow }) => { tools.approvals().decide_with(action_id, allow, false)?; waiting_approvals.remove(&action_id); if !allow { refused = true; } }
                        Some(Command::ToolResult { action_id, result }) => { let mut replies = replies.lock().map_err(|_| anyhow::anyhow!("Custom tool replies poisoned"))?; let reply = replies.get_mut(&action_id).context("No custom tool result is pending for this action")?; ensure!(reply.is_none(), "Custom tool result already delivered"); *reply = Some(result); }
                        Some(Command::Answer { action_id, answer }) => { refused |= matches!(answer, xylo_core::InteractionAnswer::Reject); tools.answer(action_id, answer)?; waiting_interactions.remove(&action_id); }
                        Some(Command::Cancel) => cancel.cancel(),
                        Some(Command::Shutdown) => { cancel.cancel(); shutting_down = true; }
                        None => {
                            shutting_down = true;
                            for action_id in waiting_interactions.drain() { let _ = tools.answer(action_id, xylo_core::InteractionAnswer::Reject); refused = true; }
                            for action_id in waiting_approvals.drain() { let _ = tools.approvals().decide_with(action_id, false, false); refused = true; }
                            if refused { eprintln!("Refused pending approval: stream-json input closed before a decision"); cancel.cancel(); }
                            if !replies.lock().map_err(|_| anyhow::anyhow!("Custom tool replies poisoned"))?.is_empty() { eprintln!("Custom tool client disconnected before replying"); cancel.cancel(); }
                        }
                        _ => { cancel.cancel(); bail!("A run is already active; only approve, tool_result, cancel or shutdown is valid"); }
                    }
                }
            }
        }
        refused |= policy_refused.load(Ordering::Acquire);
        if terminal.is_none() {
            let failure = EngineEvent::Error {
                message: "Engine ended without a terminal event".into(),
                retryable: false,
                kind: ErrorKind::Internal,
            };
            telemetry.event(conversation.id, turn_id, &failure);
            sequence += 1;
            if options.output == Format::Stream {
                emit(&Event {
                    version: VERSION,
                    session_id: conversation.id,
                    turn_id,
                    sequence,
                    payload: failure.clone(),
                    exit_code: Some(if refused { 2 } else { 1 }),
                })?;
            }
            terminal = Some(failure);
        }
        cancel.cancel();
        // Terminal receipts are durable; best-effort title generation may still run.
        running.abort();
        let _ = running.await;
        refused |= policy_refused.load(Ordering::Acquire);
        drop(span);
        let terminal = terminal.unwrap_or(EngineEvent::Error {
            message: "Engine ended without a terminal event".into(),
            retryable: false,
            kind: ErrorKind::Internal,
        });
        let (text, usage, error, code) = match terminal {
            EngineEvent::Completed { text, usage } => {
                (text, Some(usage), None, if refused { 2 } else { 0 })
            }
            EngineEvent::Error { message, .. } => (
                String::new(),
                None,
                Some(message),
                if refused { 2 } else { 1 },
            ),
            EngineEvent::Cancelled => (
                String::new(),
                None,
                Some(
                    if refused {
                        "An action required permission and was refused"
                    } else {
                        "Run cancelled"
                    }
                    .into(),
                ),
                if refused { 2 } else { 1 },
            ),
            _ => unreachable!(),
        };
        status = status.max(code);
        match options.output {
            Format::Json => emit(
                &json!({"version":VERSION,"session_id":conversation.id,"turn_id":turn_id,"text":text,"usage":usage,"error":error,"exit_code":code}),
            )?,
            Format::Text => {
                if !text.is_empty() {
                    println!("{text}");
                }
                if let Some(error) = error {
                    eprintln!("{error}");
                }
            }
            Format::Stream => {}
        }
        if let Some(path) = &options.export {
            xylo_runtime::conversation_export::export(&profile.store, conversation.id, path)?;
        }
        if !options.stream_input || shutting_down {
            break;
        }
    }
    if let Some(reader) = input_reader {
        reader.abort();
    }
    tools.shutdown().await;
    Ok(status)
}

fn custom_handler(replies: Replies) -> xylo_engine::CustomToolHandler {
    Arc::new(move |id, _name, _arguments, cancel| {
        let deadline = std::time::Instant::now() + Duration::from_secs(300);
        let result = loop {
            if cancel.is_cancelled() {
                break Err(anyhow::anyhow!("Custom tool cancelled"));
            }
            if std::time::Instant::now() >= deadline {
                break Err(anyhow::anyhow!("Custom tool timed out"));
            }
            let ready = replies
                .lock()
                .map_err(|_| anyhow::anyhow!("Custom tool replies poisoned"))?
                .get_mut(&id)
                .and_then(Option::take);
            if let Some(result) = ready {
                break result.map_err(anyhow::Error::msg);
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        replies
            .lock()
            .map_err(|_| anyhow::anyhow!("Custom tool replies poisoned"))?
            .remove(&id);
        result
    })
}

fn emit(value: &impl serde::Serialize) -> Result<()> {
    let mut line = serde_json::to_vec(value)?;
    ensure!(
        line.len() < xylo_core::MAX_PROTOCOL_BYTES,
        "Output exceeds protocol frame limit"
    );
    line.push(b'\n');
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&line)?;
    stdout.flush()?;
    Ok(())
}
async fn read_commands(sender: mpsc::Sender<Result<Command>>) {
    let mut stdin = BufReader::new(tokio::io::stdin());
    loop {
        let mut line = Vec::new();
        let result = (&mut stdin)
            .take((xylo_core::MAX_PROTOCOL_BYTES + 1) as u64)
            .read_until(b'\n', &mut line)
            .await;
        let command = match result {
            Ok(0) => break,
            Ok(_) => (|| {
                ensure!(
                    line.len() <= xylo_core::MAX_PROTOCOL_BYTES,
                    "Input exceeds protocol frame limit"
                );
                ensure!(
                    line.last() == Some(&b'\n'),
                    "JSONL input must end with a newline"
                );
                let input: Input =
                    serde_json::from_slice(&line).context("Invalid CLI protocol frame")?;
                input.validate()?;
                Ok(input.payload)
            })(),
            Err(error) => Err(error.into()),
        };
        let failed = command.is_err();
        if sender.send(command).await.is_err() || failed {
            break;
        }
    }
}

/// Headless requests use the same saved defaults as desktop and wakeups.
fn turn_provider(settings: &xylo_core::AppSettings, model: Option<&str>) -> ProviderConfig {
    let mut provider = settings.turn_provider();
    if let Some(model) = model.filter(|model| *model != provider.model) {
        let catalog = settings.providers.iter().find(|saved| {
            saved.model == model
                && saved.family == provider.family
                && saved.endpoint == provider.endpoint
                && saved.credential_ref == provider.credential_ref
                && saved.headers == provider.headers
                && saved.discovery_url == provider.discovery_url
                && saved.custom == provider.custom
        });
        provider.model = model.to_owned();
        provider.model_capabilities = catalog.and_then(|saved| saved.model_capabilities.clone());
        provider.reasoning_options = catalog.and_then(|saved| saved.reasoning_options.clone());
        provider.default_reasoning_effort =
            catalog.and_then(|saved| saved.default_reasoning_effort.clone());
        provider.reasoning_effort = None;
        provider.served_context_tokens = None;
    }
    provider
}

#[cfg(test)]
mod tests;
