//! The worker's engine: admits a turn, restores its durable context, runs the
//! agent loop and settles the outcome.
//!
//! A turn ends in exactly one terminal event. Completed turns commit their
//! checkpoint, reply and receipt atomically (a lost `Completed` event is
//! replayed from the receipt without a model call). A first turn's generated
//! title follows `Completed` as a best-effort `TitleUpdated`. Cancelled and failed turns
//! keep their last durable checkpoint and store what had streamed as an
//! interrupted or failed reply. Failures are classified into an
//! [`ErrorKind`] with an honest `retryable` flag.
use crate::{
    AppPaths, ProviderCredentialResolver,
    agent_loop::{AgentLoop, Journal, LoopFailure, MAX_MODEL_CALLS, error_result},
    attachments::{AttachmentProvider, user_message},
    broker_tools::BrokerTools,
    model::{Message, ModelError, ModelProvider, ModelRequest, Role},
    provider_guard::{ContextBudget, GuardedProvider},
    steering::SteeringControl,
    subagent::{CHILD_TIMEOUT, Delegation, MAX_CONCURRENT_CHILDREN},
    tools::{ToolBroker, ToolSession},
};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{collections::HashSet, sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use xylo_capabilities::CapabilityConfig;
use xylo_core::{
    EngineBackend, EngineEvent, ErrorKind, MessageStatus, PermissionMode, ProviderConfig,
    TitleSource, TurnPhase, TurnRequest, WorkerCredential,
};
use xylo_services::{LearningService, Store};

/// Output limit of the title request.
const TITLE_TOKENS: u32 = 64;
/// The title request's deadline.
const TITLE_DEADLINE: Duration = Duration::from_secs(10);
/// Most reply text sent to the title request.
const TITLE_REPLY_BYTES: usize = 2000;
/// Longest stored title.
const TITLE_CHARS: usize = 80;

/// Covers every error return, cancellation and future drop after turn admission.
/// A process kill is covered by Store recovery's same-transaction evidence enqueue.
struct LearningTurnGuard {
    service: LearningService,
    session: uuid::Uuid,
    turn: uuid::Uuid,
    completed: bool,
}

impl Drop for LearningTurnGuard {
    fn drop(&mut self) {
        if !self.completed
            && let Err(error) = self.service.queue_terminal_turn(self.session, self.turn)
        {
            tracing::error!(error=%error,"Could not persist terminal procedural evidence");
        }
    }
}

/// Xylo's native agent engine.
pub struct AgentEngine {
    store: Store,
    paths: AppPaths,
    session: ToolSession,
    steering: SteeringControl,
    provider_credential: Option<WorkerCredential>,
    credential_resolver: Option<Arc<dyn ProviderCredentialResolver>>,
}

impl AgentEngine {
    /// An engine for one worker. `session` carries the approval gate, the live
    /// permission mode and the unlocked secret vault.
    pub fn new(store: Store, paths: AppPaths, session: ToolSession) -> Self {
        Self {
            steering: SteeringControl::new(store.clone()),
            provider_credential: None,
            credential_resolver: None,
            store,
            paths,
            session,
        }
    }

    /// Wakes the running turn to apply newly queued guidance at its next
    /// boundary.
    pub fn notify_steering(&self) {
        self.steering.notify();
    }

    /// Uses the host-authorized provider credential instead of the vault.
    pub fn with_provider_credential(mut self, credential: WorkerCredential) -> Self {
        self.provider_credential = Some(credential);
        self
    }

    /// Asks `resolver` for renewed provider credentials.
    pub fn with_provider_credential_resolver(
        mut self,
        resolver: Arc<dyn ProviderCredentialResolver>,
    ) -> Self {
        self.credential_resolver = Some(resolver);
        self
    }

    fn provider(&self, config: &ProviderConfig) -> Result<Arc<dyn ModelProvider>> {
        if let Some(credential) = &self.provider_credential {
            ensure!(
                config.credential_ref.is_none() || credential.0.is_some(),
                "Host did not authorize the provider credential"
            );
            return crate::providers::build_renewable(
                config,
                credential.clone(),
                self.credential_resolver.clone(),
            );
        }
        let key = match &config.credential_ref {
            Some(reference) => Some(
                self.session
                    .vault()
                    .get(reference)?
                    .with_context(|| format!("Credential {reference} is not configured"))?,
            ),
            None => None,
        };
        crate::providers::build(config, key)
    }

    fn broker(
        &self,
        request: &TurnRequest,
        events: mpsc::Sender<EngineEvent>,
        cancel: CancellationToken,
    ) -> Result<ToolBroker> {
        ToolBroker::new(
            request.clone(),
            self.store.clone(),
            self.capability_config(request)?,
            self.session.clone(),
            events,
            cancel,
        )
    }

    /// The conversation to continue: the checkpoint with every pending tool
    /// call of an earlier turn settled from the journal, plus this turn's user
    /// message unless the same turn is resuming. Nothing is dispatched here;
    /// calls of a resuming turn that never ran are left for the loop.
    async fn restored_messages(
        &self,
        request: &TurnRequest,
        events: mpsc::Sender<EngineEvent>,
        cancel: CancellationToken,
    ) -> Result<Vec<Message>> {
        let checkpoint = self.store.checkpoint(request.session_id)?;
        let Some(checkpoint) = checkpoint else {
            return Ok(vec![user_message(request)]);
        };
        let mut messages: Vec<Message> = serde_json::from_value(checkpoint.messages)
            .context("The saved conversation could not be read")?;
        let resuming = checkpoint.turn_id == request.turn_id;
        let mut previous = request.clone();
        previous.turn_id = checkpoint.turn_id;
        let broker = self.broker(&previous, events, cancel)?;
        let settled = settle_pending_tools(&mut messages, &broker, resuming);
        let shutdown = broker.shutdown().await;
        settled?;
        shutdown?;
        if !resuming {
            // Keep the earlier turn's key namespace until its settled results
            // have a durable checkpoint of their own.
            self.store.save_checkpoint(
                request.session_id,
                checkpoint.turn_id,
                &serde_json::to_value(&messages)?,
            )?;
            messages.push(user_message(request));
        }
        Ok(messages)
    }

    fn capability_config(&self, request: &TurnRequest) -> Result<CapabilityConfig> {
        let path = self.paths.data.join("capabilities.json");
        let mut config: CapabilityConfig = if path.is_file() {
            serde_json::from_slice(&std::fs::read(&path)?)
                .context("Read native capability configuration")?
        } else {
            CapabilityConfig::default()
        };
        config.workspace = request.workspace.clone();
        config.artifacts = self.paths.artifacts.clone();
        config.management_root = Some(self.paths.data.join("native-management"));
        Ok(config)
    }

    /// The system prompt for this turn: environment, permission policy,
    /// project instructions and the skill and fact indexes.
    fn system_prompt(
        &self,
        request: &TurnRequest,
        broker: &ToolBroker,
        mode: PermissionMode,
    ) -> Result<String> {
        let mut context =
            crate::prompts::PromptContext::detect(&request.workspace, mode, xylo_core::now_ms());
        context.skills_index = broker.project_skill_context().unwrap_or_else(|error| {
            tracing::warn!(error=%error, "Skill index unavailable");
            String::new()
        });
        let facts = broker.project_facts()?;
        context.facts_index = crate::prompts::facts_index(
            facts
                .iter()
                .map(|fact| fact.content.lines().next().unwrap_or_default()),
        );
        Ok(crate::prompts::system(&context))
    }

    /// Generates, stores and announces a title after the first completed
    /// turn of a conversation, i.e. while it still has its prompt-derived
    /// title. Runs after the turn is committed and `Completed` was sent, with
    /// its own short deadline; the caller logs and ignores its errors. Never
    /// replaces a user's title.
    async fn title_conversation(
        &self,
        request: &TurnRequest,
        reply: &str,
        provider: &dyn ModelProvider,
        events: &mpsc::Sender<EngineEvent>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let untitled = self
            .store
            .conversation(request.session_id)?
            .is_some_and(|conversation| conversation.title_source == TitleSource::Prompt);
        if !untitled || request.prompt.trim().is_empty() {
            return Ok(());
        }
        let mut conversation = request.prompt.trim().to_owned();
        let reply = crate::provider_guard::context_excerpt(reply.trim(), TITLE_REPLY_BYTES);
        if !reply.is_empty() {
            conversation.push_str("\n\n");
            conversation.push_str(&reply);
        }
        let title_request = ModelRequest {
            messages: vec![
                Message::system(crate::prompts::title()),
                Message::user(conversation),
            ],
            tools: Vec::new(),
            max_tokens: TITLE_TOKENS,
            reasoning: crate::model::Reasoning::Lowest,
        };
        let cancel = cancel.child_token();
        let reply = tokio::time::timeout(
            TITLE_DEADLINE,
            provider.complete(title_request, cancel.clone()),
        )
        .await;
        cancel.cancel();
        let title = match reply {
            Ok(Ok(reply)) => clean_title(&reply.message.content),
            Ok(Err(error)) => {
                tracing::debug!(%error, "Title generation failed");
                None
            }
            Err(_) => None,
        };
        let Some(title) = title else {
            return Ok(());
        };
        let store = self.store.clone();
        let session = request.session_id;
        let stored = tokio::task::spawn_blocking(move || {
            store.rename_conversation(session, &title, TitleSource::Generated)
        })
        .await?;
        match stored {
            Ok(Some(conversation)) if conversation.title_source == TitleSource::Generated => {
                events
                    .send(EngineEvent::TitleUpdated {
                        title: conversation.title,
                    })
                    .await?;
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "Could not store the generated title"),
        }
        Ok(())
    }

    /// Stores what a stopped or failed turn had streamed.
    async fn keep_partial_reply(
        &self,
        request: &TurnRequest,
        text: String,
        status: MessageStatus,
    ) -> Result<()> {
        let store = self.store.clone();
        let (session, turn) = (request.session_id, request.turn_id);
        tokio::task::spawn_blocking(move || {
            store.record_interrupted_reply(session, turn, &text, status)
        })
        .await?
    }

    async fn run_turn(
        &self,
        request: TurnRequest,
        events: &mpsc::Sender<EngineEvent>,
        cancel: CancellationToken,
    ) -> Result<()> {
        if let Some(completed) = self
            .store
            .completed_turn(request.session_id, request.turn_id)?
        {
            events
                .send(EngineEvent::Completed {
                    text: completed.text,
                    usage: completed.usage,
                })
                .await?;
            return Ok(());
        }
        if cancel.is_cancelled() {
            events.send(EngineEvent::Cancelled).await?;
            return Ok(());
        }
        events
            .send(EngineEvent::Phase {
                phase: TurnPhase::Preparing,
            })
            .await?;
        request.provider.validate()?;
        let attachment_store =
            xylo_services::attachments::AttachmentStore::new(self.paths.artifacts.clone())?;
        let validation_store = attachment_store.clone();
        let attachment_refs = request.attachments.clone();
        let attachment_session = request.session_id;
        tokio::task::spawn_blocking(move || {
            validation_store.validate(attachment_session, &attachment_refs)
        })
        .await??;
        ensure!(
            request.workspace.is_absolute() && request.workspace.is_dir(),
            "Select an existing workspace directory"
        );
        let mut learning_turn = LearningTurnGuard {
            service: LearningService::new(self.store.clone()),
            session: request.session_id,
            turn: request.turn_id,
            completed: false,
        };
        let broker = Arc::new(self.broker(&request, events.clone(), cancel.clone())?);
        if cancel.is_cancelled() {
            events.send(EngineEvent::Cancelled).await?;
            return Ok(());
        }
        // Inspect exact native receipts before blocking resume on the journal.
        // This pass never dispatches a tool or probes its target.
        for action in self.store.unresolved_actions(request.session_id)? {
            if action.state == "outcome_unknown" && action.intent.name == "environment_execute" {
                let broker = broker.clone();
                tokio::task::spawn_blocking(move || broker.recover_native_action(action)).await??;
            }
        }
        let unresolved = self.store.unresolved_actions(request.session_id)?;
        ensure!(
            !unresolved
                .iter()
                .any(|a| matches!(a.state.as_str(), "outcome_unknown" | "running")),
            "This task has an outcome-unknown action. Reconcile it before resuming"
        );
        let guarded = GuardedProvider::new(
            Arc::new(AttachmentProvider::new(
                self.provider(&request.provider)?,
                attachment_store,
                request.session_id,
            )),
            &request.provider,
            self.paths.data.join("local-inference.lock"),
            managed_endpoint(&self.paths, &request.provider.endpoint)?,
        );
        let summary_deadline = guarded.timeout();
        let provider: Arc<dyn ModelProvider> = Arc::new(guarded);
        let budget = ContextBudget::new(&request.provider);
        let messages = self
            .restored_messages(&request, events.clone(), cancel.clone())
            .await?;
        let mode = self.session.permissions().get();
        let system = self.system_prompt(&request, &broker, mode)?;
        // Model calls admit their own memory when the model runs locally;
        // a child holds no lease of its own while it waits on tools.
        let resources = xylo_resources::ResourceManager::new();
        let delegation = Arc::new(Delegation {
            provider: provider.clone(),
            budget: budget.clone(),
            summary_deadline,
            max_tokens: request.provider.output_tokens,
            parent: Arc::downgrade(&broker),
            events: events.clone(),
            slots: Arc::new(tokio::sync::Semaphore::new(
                resources
                    .suggested_parallelism(64 * 1024 * 1024)?
                    .clamp(1, MAX_CONCURRENT_CHILDREN),
            )),
            cancel: cancel.clone(),
            timeout: CHILD_TIMEOUT,
            runtime: tokio::runtime::Handle::current(),
            workspace: request.workspace.clone(),
            permissions: self.session.permissions().clone(),
        });
        broker.set_delegate(delegation.handler());
        self.store.save_checkpoint(
            request.session_id,
            request.turn_id,
            &serde_json::to_value(&messages)?,
        )?;
        events.send(EngineEvent::Started).await?;
        let agent = AgentLoop {
            provider: provider.clone(),
            budget,
            summary_deadline,
            tools: Arc::new(BrokerTools::root(broker.clone())),
            system,
            max_tokens: request.provider.output_tokens,
            max_model_calls: MAX_MODEL_CALLS,
            events: events.clone(),
            permissions: Some((self.session.permissions().clone(), mode)),
            journal: Some(Journal {
                store: self.store.clone(),
                session: request.session_id,
                turn: request.turn_id,
                steering: Some(self.steering.clone()),
            }),
            cancel: cancel.clone(),
        };
        let outcome = agent.run(messages).await;
        let shutdown = broker.shutdown().await;
        match outcome {
            Ok(outcome) => {
                shutdown?;
                if outcome.stopped_early {
                    tracing::warn!(session_id=%request.session_id, "Turn reached its step limit");
                }
                self.store.complete_turn(
                    request.session_id,
                    request.turn_id,
                    &serde_json::to_value(&outcome.messages)?,
                    &outcome.text,
                    &outcome.usage,
                )?;
                learning_turn.completed = true; // Receipt and evidence job committed atomically.
                events
                    .send(EngineEvent::Completed {
                        text: outcome.text.clone(),
                        usage: outcome.usage,
                    })
                    .await?;
                // The turn is committed and reported; a title is an extra
                // that can neither delay nor fail it.
                if let Err(error) = self
                    .title_conversation(&request, &outcome.text, provider.as_ref(), events, &cancel)
                    .await
                {
                    tracing::warn!(%error, "Conversation title was not updated");
                }
                Ok(())
            }
            Err(failure) if failure.cancelled() && cancel.is_cancelled() => {
                self.keep_partial_reply(&request, failure.streamed, MessageStatus::Interrupted)
                    .await?;
                // Cleanup faults are still errors after a requested stop.
                shutdown?;
                events.send(EngineEvent::Cancelled).await?;
                Ok(())
            }
            Err(LoopFailure { error, streamed }) => {
                if let Err(stored) = self
                    .keep_partial_reply(&request, streamed, MessageStatus::Failed)
                    .await
                {
                    tracing::error!(error=%stored, "Could not keep the failed reply");
                }
                if let Err(cleanup) = shutdown {
                    tracing::error!(error=%cleanup, "Tool cleanup failed after a turn error");
                }
                Err(error)
            }
        }
    }
}

#[async_trait]
impl EngineBackend for AgentEngine {
    fn name(&self) -> &str {
        "Xylo"
    }

    /// Runs a turn and always ends with one terminal event: failures are sent
    /// as a classified [`EngineEvent::Error`]. Only a closed event channel is
    /// returned as an error.
    async fn run(
        &self,
        request: TurnRequest,
        events: mpsc::Sender<EngineEvent>,
        cancel: CancellationToken,
    ) -> Result<()> {
        let Err(error) = self.run_turn(request, &events, cancel).await else {
            return Ok(());
        };
        if events.is_closed() {
            return Err(error);
        }
        let (kind, retryable) = classify(&error);
        events
            .send(EngineEvent::Error {
                message: format!("{error:#}"),
                retryable,
                kind,
            })
            .await
            .map_err(|_| error)
            .context("Task event consumer disconnected")
    }
}

/// The product classification of a turn failure.
fn classify(error: &anyhow::Error) -> (ErrorKind, bool) {
    match error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ModelError>())
    {
        Some(model) => (model.kind(), model.retryable()),
        None => (ErrorKind::Internal, false),
    }
}

/// A model reply reduced to one plain line, or `None` if nothing usable.
fn clean_title(reply: &str) -> Option<String> {
    let line = reply
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?
        .trim_matches(|c: char| {
            matches!(c, '"' | '\'' | '`' | '*' | '#' | '.') || c.is_whitespace()
        });
    let title: String = line
        .chars()
        .filter(|c| !c.is_control())
        .take(TITLE_CHARS)
        .collect();
    let title = title.trim();
    (!title.is_empty()).then(|| title.to_owned())
}

/// Only application-owned endpoints with an explicit internal capability marker
/// use the single-slot cross-process lease. External servers own their scheduling.
fn managed_endpoint(paths: &AppPaths, endpoint: &str) -> Result<bool> {
    let marker = paths.data.join("managed-endpoints.json");
    if !marker.is_file() {
        return Ok(false);
    }
    let value: Value = serde_json::from_slice(&std::fs::read(marker)?)
        .context("Read managed endpoint capabilities")?;
    let normalized = url::Url::parse(endpoint)?
        .to_string()
        .trim_end_matches('/')
        .to_owned();
    Ok(value
        .get(&normalized)
        .and_then(|v| v.get("single_slot"))
        .and_then(Value::as_bool)
        == Some(true))
}

/// The journal operation of a call: `call_tool` is journaled under its target.
fn journaled_operation(name: &str, arguments: &Value) -> Result<(String, Value)> {
    if name == "call_tool" {
        Ok((
            arguments
                .get("name")
                .and_then(Value::as_str)
                .context("Missing restored tool name")?
                .to_owned(),
            arguments
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({})),
        ))
    } else {
        Ok((name.to_owned(), arguments.clone()))
    }
}

/// Settles the unanswered calls of the newest assistant message from the
/// journal. A call that already ran gets its recorded result; one that never
/// ran stays pending for a resuming turn and is reported as not run for a new
/// turn. Unknown outcomes block until the user reconciles them.
fn settle_pending_tools(
    messages: &mut Vec<Message>,
    broker: &ToolBroker,
    resuming: bool,
) -> Result<()> {
    refresh_operator_observations(messages, broker)?;
    let Some(index) = messages
        .iter()
        .rposition(|message| message.role == Role::Assistant && !message.tool_calls.is_empty())
    else {
        return Ok(());
    };
    let answered: HashSet<_> = messages[index + 1..]
        .iter()
        .filter_map(|m| m.tool_call_id.clone())
        .collect();
    let calls = messages[index].tool_calls.clone();
    for call in calls
        .into_iter()
        .filter(|call| !answered.contains(&call.id))
    {
        let Ok(arguments) = serde_json::from_str::<Value>(&call.arguments) else {
            if !resuming {
                messages.push(Message::tool_result(
                    call.id,
                    error_result("Not run: the arguments were not valid JSON."),
                ));
            }
            continue;
        };
        let (name, args) = journaled_operation(&call.name, &arguments)?;
        let result = match tokio::task::block_in_place(|| broker.recover_journaled(&call.id))? {
            Some(action) => {
                ensure!(
                    action.intent.name == name && action.intent.arguments == args,
                    "Restored tool call does not match its durable intent"
                );
                match action.state.as_str() {
                    "succeeded" => serde_json::to_string(
                        &broker.model_result(
                            action.intent.id,
                            action
                                .result
                                .context("Successful action has no stored result")?,
                        ),
                    )?,
                    "failed" => match action.result {
                        Some(result) => {
                            serde_json::to_string(&broker.model_result(action.intent.id, result))?
                        }
                        None => {
                            error_result(action.error.unwrap_or_else(|| "Action failed".into()))
                        }
                    },
                    "denied" => {
                        error_result(action.error.unwrap_or_else(|| "Action denied".into()))
                    }
                    "outcome_unknown" | "running" | "pending" => anyhow::bail!(
                        "Interrupted action requires recovery/reconciliation before continuing"
                    ),
                    _ => anyhow::bail!("Invalid persisted action state"),
                }
            }
            None if resuming => continue,
            None => error_result("Not run: the earlier task stopped before this call started."),
        };
        messages.push(Message::tool_result(call.id, result));
    }
    Ok(())
}

/// A tool's earlier unknown response can already exist in the checkpoint. Keep
/// exact current-turn operator receipts visible without appending duplicate
/// results or treating their manual observation as verified native output.
fn refresh_operator_observations(messages: &mut [Message], broker: &ToolBroker) -> Result<()> {
    for (expected_id, key) in broker.operator_reviewed_action_keys()? {
        let Some(call_id) = key.strip_prefix("root:") else {
            continue;
        };
        let Some((index, call)) = messages.iter().enumerate().rev().find_map(|(index, m)| {
            (m.role == Role::Assistant)
                .then(|| m.tool_calls.iter().find(|call| call.id == call_id))
                .flatten()
                .map(|call| (index, call.clone()))
        }) else {
            continue;
        };
        let arguments: Value = serde_json::from_str(&call.arguments)?;
        let (name, args) = journaled_operation(&call.name, &arguments)?;
        let action = broker
            .journaled(call_id)?
            .context("Reviewed tool action is unavailable")?;
        ensure!(
            action.intent.id == expected_id
                && action.intent.name == name
                && action.intent.arguments == args,
            "Reviewed tool result does not match its durable intent"
        );
        let result = action
            .result
            .context("Reviewed action has no stored observation")?;
        ensure!(
            result
                .pointer("/operator_observation/source")
                .and_then(Value::as_str)
                == Some("operator_review"),
            "Reviewed action provenance is unavailable"
        );
        let presented = serde_json::to_string(&broker.model_result(expected_id, result))?;
        for message in &mut messages[index + 1..] {
            if message.role == Role::Assistant {
                break;
            }
            if message.role == Role::Tool && message.tool_call_id.as_deref() == Some(call_id) {
                message.content = presented.clone();
                break;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
