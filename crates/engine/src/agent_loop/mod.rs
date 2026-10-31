//! Xylo's agent loop.
//!
//! One step: fit the conversation to the context window (condensing older
//! history into a durable summary when needed), stream a model call, flush its
//! text deltas, and finish if it asked for no tools. Otherwise checkpoint the
//! assistant message, execute the tool batch, append the results in call
//! order, checkpoint again, apply any queued user guidance, and repeat.
//!
//! Invariants:
//! - **Checkpoint before and after every tool batch.** The assistant message
//!   that requests tools is durable before any of them is dispatched, and the
//!   results are durable before the next model call. A crash in between
//!   resumes by executing only the calls that have no result yet.
//! - **Exactly-once dispatch.** Tools run only through a [`ToolExecutor`],
//!   whose broker journals each call by its id; a resumed call that already
//!   ran returns its recorded result instead of running again.
//! - **Guidance only at role-safe boundaries.** User guidance is inserted
//!   after a complete tool round (or before the first model call), never
//!   between a tool call and its result.
//! - **Ordered events.** A response's text deltas are flushed before any tool
//!   event of that response.
//! - **No hidden prompting.** The only prompt text is the caller's system
//!   prompt, a note when the user changes the permission mode mid-task, and
//!   the step-limit note on the final call of a runaway turn. Both notes are
//!   user-role application notes at their place in the conversation, so the
//!   cached system block holds only the system prompt.
mod dispatch;
mod retry;

pub(crate) use dispatch::{Access, ToolExecutor, error_result};

use crate::{
    model::{
        Assembler, Message, ModelError, ModelProvider, ModelRequest, ModelResponse, Role,
        StreamEvent, ToolSchema,
    },
    provider_guard::ContextBudget,
    steering::SteeringControl,
    tools::PermissionCell,
};
use anyhow::Context;
use futures_util::StreamExt;
use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use xylo_core::{EngineEvent, PermissionMode, TurnPhase, Usage};
use xylo_services::Store;

/// Runaway guard on model calls per turn; not a budget.
pub(crate) const MAX_MODEL_CALLS: u32 = 400;
/// Text deltas are batched for at most this long.
const DELTA_INTERVAL: Duration = Duration::from_millis(32);

/// Where a root loop keeps its durable state.
pub(crate) struct Journal {
    pub store: Store,
    pub session: Uuid,
    pub turn: Uuid,
    pub steering: Option<SteeringControl>,
}

impl Journal {
    async fn checkpoint(&self, messages: &[Message]) -> anyhow::Result<()> {
        let value = serde_json::to_value(messages)?;
        let (store, session, turn) = (self.store.clone(), self.session, self.turn);
        tokio::task::spawn_blocking(move || store.save_checkpoint(session, turn, &value))
            .await
            .context("Checkpoint task stopped")?
    }
}

/// One configured run of the loop.
pub(crate) struct AgentLoop {
    pub provider: Arc<dyn ModelProvider>,
    pub budget: ContextBudget,
    /// Deadline of a context summary request.
    pub summary_deadline: Duration,
    pub tools: Arc<dyn ToolExecutor>,
    /// The system prompt; identical for every call of the run.
    pub system: String,
    pub max_tokens: u32,
    pub max_model_calls: u32,
    pub events: mpsc::Sender<EngineEvent>,
    /// Root loops stream text, phases and context usage to the UI and
    /// checkpoint; child loops report only through their tool events.
    pub journal: Option<Journal>,
    /// The live permission mode and the mode the model was last told about
    /// (the one in the system prompt). A change is announced to the model
    /// with an appended note; the system prompt itself never changes.
    pub permissions: Option<(PermissionCell, PermissionMode)>,
    pub cancel: CancellationToken,
}

/// A finished run.
#[derive(Debug)]
pub(crate) struct LoopOutcome {
    pub messages: Vec<Message>,
    /// The final answer.
    pub text: String,
    pub usage: Usage,
    /// The step limit ended the run; `text` is the model's own summary.
    pub stopped_early: bool,
}

/// A run that did not finish. `streamed` is the text the user saw.
#[derive(Debug)]
pub(crate) struct LoopFailure {
    pub error: anyhow::Error,
    pub streamed: String,
}

impl LoopFailure {
    /// Whether the run ended because it was cancelled.
    pub(crate) fn cancelled(&self) -> bool {
        matches!(
            self.error.downcast_ref::<ModelError>(),
            Some(ModelError::Interrupted)
        )
    }
}

/// Per-run bookkeeping.
struct Run {
    started: Instant,
    usage: Usage,
    streamed: String,
}

impl AgentLoop {
    fn root(&self) -> bool {
        self.journal.is_some()
    }

    async fn emit(&self, event: EngineEvent) -> anyhow::Result<()> {
        self.events
            .send(event)
            .await
            .context("Task event consumer disconnected")
    }

    async fn phase(&self, phase: TurnPhase) -> anyhow::Result<()> {
        if self.root() {
            self.emit(EngineEvent::Phase { phase }).await?;
        }
        Ok(())
    }

    async fn checkpoint(&self, messages: &[Message]) -> anyhow::Result<()> {
        match &self.journal {
            Some(journal) => journal.checkpoint(messages).await,
            None => Ok(()),
        }
    }

    /// Makes the call ids of `assistant` unique within the turn; see
    /// [`unique_call_ids`]. A root loop also treats ids its action journal
    /// holds as taken: after a compaction and a resume, the calls that used
    /// them are in neither `messages` nor `issued`.
    async fn assign_call_ids(
        &self,
        issued: &mut HashSet<String>,
        assistant: &mut Message,
    ) -> anyhow::Result<()> {
        if assistant.tool_calls.is_empty() {
            return Ok(());
        }
        let Some(journal) = &self.journal else {
            return unique_call_ids(assistant, issued, |_| Ok(false));
        };
        let (store, session, turn) = (journal.store.clone(), journal.session, journal.turn);
        let (mut message, mut ids) = (assistant.clone(), std::mem::take(issued));
        let (message, ids) = tokio::task::spawn_blocking(move || {
            let recorded = |id: &str| {
                let key = crate::tools::ToolBroker::journal_key(None, id);
                Ok(store.action_for_key(session, turn, &key)?.is_some())
            };
            unique_call_ids(&mut message, &mut ids, recorded).map(|()| (message, ids))
        })
        .await
        .context("Call id task stopped")??;
        *assistant = message;
        *issued = ids;
        Ok(())
    }

    /// Runs the loop from `messages` (the conversation without the system
    /// prompt) until the model answers without tools.
    pub(crate) async fn run(&self, messages: Vec<Message>) -> Result<LoopOutcome, LoopFailure> {
        let mut run = Run {
            started: Instant::now(),
            usage: Usage::default(),
            streamed: String::new(),
        };
        let mut messages = messages;
        match self.steps(&mut messages, &mut run).await {
            Ok((text, stopped_early)) => {
                run.usage.elapsed_ms = run.started.elapsed().as_millis() as u64;
                Ok(LoopOutcome {
                    messages,
                    text,
                    usage: run.usage,
                    stopped_early,
                })
            }
            Err(error) => Err(LoopFailure {
                error: if self.cancel.is_cancelled() {
                    ModelError::Interrupted.into()
                } else {
                    error
                },
                streamed: run.streamed,
            }),
        }
    }

    async fn steps(
        &self,
        messages: &mut Vec<Message>,
        run: &mut Run,
    ) -> anyhow::Result<(String, bool)> {
        // A resumed turn may end with calls that never produced a result.
        if let Some(index) = messages.iter().rposition(|m| m.role == Role::Assistant)
            && unanswered(&messages[index..]).next().is_some()
        {
            let calls = unanswered(&messages[index..]).cloned().collect::<Vec<_>>();
            self.execute(messages, index, calls).await?;
        }
        let mut calls = 0;
        let mut announced = self.permissions.as_ref().map(|(_, mode)| *mode);
        let mut issued = call_ids(messages);
        loop {
            if self.cancel.is_cancelled() {
                return Err(ModelError::Interrupted.into());
            }
            self.apply_guidance(messages).await?;
            if let Some((cell, _)) = &self.permissions {
                let mode = cell.get();
                if announced != Some(mode) {
                    announced = Some(mode);
                    messages.push(application_note(&crate::prompts::permission_change(mode)));
                }
            }
            calls += 1;
            let final_call = calls >= self.max_model_calls;
            let response = self.model_call(messages, final_call, run).await?;
            let mut assistant = response.message;
            self.assign_call_ids(&mut issued, &mut assistant).await?;
            let text = assistant.content.clone();
            if assistant.tool_calls.is_empty() {
                messages.push(assistant);
                // Guidance that arrived while the answer streamed continues
                // the task instead of being left unread.
                if !final_call && self.apply_guidance(messages).await? > 0 {
                    continue;
                }
                return Ok((text, false));
            }
            if final_call {
                let ids = assistant
                    .tool_calls
                    .iter()
                    .map(|call| call.id.clone())
                    .collect::<Vec<_>>();
                messages.push(assistant);
                for id in ids {
                    messages.push(Message::tool_result(
                        id,
                        error_result("Not run: this task reached its step limit."),
                    ));
                }
                self.checkpoint(messages).await?;
                let summary = if text.trim().is_empty() {
                    last_text(messages)
                } else {
                    text
                };
                return Ok((summary, true));
            }
            let prepared = assistant
                .tool_calls
                .clone()
                .into_iter()
                .map(|call| {
                    let prepared = dispatch::prepare(&call, self.tools.as_ref());
                    if let dispatch::Prepared::Run { name, .. } = &prepared
                        && *name != call.name
                    {
                        assistant.rename_tool_call(&call.id, name);
                    }
                    (call.id, prepared)
                })
                .collect::<Vec<_>>();
            messages.push(assistant);
            self.checkpoint(messages).await?;
            self.run_batch(messages, prepared).await?;
        }
    }

    /// Applies queued user guidance at a role-safe boundary; returns how many
    /// guidance messages were added.
    async fn apply_guidance(&self, messages: &mut Vec<Message>) -> anyhow::Result<usize> {
        match &self.journal {
            Some(Journal {
                steering: Some(steering),
                session,
                turn,
                ..
            }) => {
                steering
                    .apply(*session, *turn, messages, &self.events)
                    .await
            }
            _ => Ok(0),
        }
    }

    /// Executes the unanswered `calls` of the assistant message at `index`.
    async fn execute(
        &self,
        messages: &mut Vec<Message>,
        index: usize,
        calls: Vec<crate::model::ToolCall>,
    ) -> anyhow::Result<()> {
        let mut assistant = messages[index].clone();
        let prepared = calls
            .into_iter()
            .map(|call| {
                let prepared = dispatch::prepare(&call, self.tools.as_ref());
                if let dispatch::Prepared::Run { name, .. } = &prepared
                    && *name != call.name
                {
                    assistant.rename_tool_call(&call.id, name);
                }
                (call.id, prepared)
            })
            .collect();
        messages[index] = assistant;
        self.checkpoint(messages).await?;
        self.run_batch(messages, prepared).await
    }

    async fn run_batch(
        &self,
        messages: &mut Vec<Message>,
        prepared: Vec<(String, dispatch::Prepared)>,
    ) -> anyhow::Result<()> {
        let delegating = prepared.iter().any(|(_, prepared)| {
            matches!(prepared, dispatch::Prepared::Run { name, .. } if name == "agent_delegate")
        });
        self.phase(if delegating {
            TurnPhase::Delegating
        } else {
            TurnPhase::RunningTools
        })
        .await?;
        let results = dispatch::run_batch(self.tools.clone(), prepared).await;
        messages.extend(results);
        self.checkpoint(messages).await
    }

    /// Fits, sends and (when transient failures allow) retries one model
    /// call, streaming its text to the UI.
    async fn model_call(
        &self,
        messages: &mut Vec<Message>,
        final_call: bool,
        run: &mut Run,
    ) -> anyhow::Result<ModelResponse> {
        let tools = self.tools.schemas();
        let view = self.fit(messages, &tools).await?;
        let mut request_messages = view;
        if final_call {
            request_messages.push(application_note(crate::prompts::step_limit()));
        }
        let request = ModelRequest {
            messages: request_messages,
            tools,
            max_tokens: self.max_tokens,
            reasoning: crate::model::Reasoning::Configured,
        };
        let mut shown = String::new();
        let mut attempt = 0;
        loop {
            self.phase(TurnPhase::Thinking).await?;
            let error = match self.stream_once(request.clone(), &mut shown, run).await {
                Ok(response) => {
                    if let Some(usage) = response.usage {
                        run.usage.input_tokens += usage.input_tokens;
                        run.usage.output_tokens += usage.output_tokens;
                        run.usage.context_tokens = usage.input_tokens;
                        if self.root() {
                            let mut context = self.budget.usage(
                                &request.messages,
                                &request.tools,
                                self.max_tokens,
                            );
                            context.deferred_tool_count = self.tools.deferred_count() as u32;
                            context.provider_input = Some(usage.input_tokens);
                            self.emit(EngineEvent::ContextUsage { usage: context })
                                .await?;
                        }
                    }
                    return Ok(response);
                }
                Err(StreamFailure::Fatal(error)) => return Err(error),
                Err(StreamFailure::Model(error)) => error,
            };
            attempt += 1;
            let Some(delay) = retry::delay(&error, attempt) else {
                return Err(error.into());
            };
            tracing::warn!(attempt, delay_ms = delay.as_millis() as u64, %error, "Retrying model request");
            self.phase(TurnPhase::Retrying {
                attempt,
                delay_ms: delay.as_millis() as u64,
            })
            .await?;
            tokio::select! {
                _ = self.cancel.cancelled() => return Err(ModelError::Interrupted.into()),
                _ = tokio::time::sleep(delay) => {}
            }
        }
    }

    /// The request view of `messages`: the system prompt plus history,
    /// condensed into a durable summary when it passes the threshold.
    async fn fit(
        &self,
        messages: &mut Vec<Message>,
        tools: &[ToolSchema],
    ) -> anyhow::Result<Vec<Message>> {
        let system = vec![Message::system(&self.system)];
        let assemble = |history: &[Message]| {
            let mut view = system.clone();
            view.extend_from_slice(history);
            view
        };
        let mut view = assemble(messages);
        if self.budget.over_threshold(&view, tools, self.max_tokens)
            && let Some(plan) = self
                .budget
                .summary_plan(&system, messages, tools, self.max_tokens)
        {
            self.phase(TurnPhase::Compacting).await?;
            match self
                .budget
                .summarize(
                    self.provider.as_ref(),
                    messages,
                    &plan,
                    self.summary_deadline,
                    &self.cancel,
                )
                .await
            {
                Ok(condensed) => {
                    *messages = condensed;
                    self.checkpoint(messages).await?;
                    view = assemble(messages);
                }
                Err(ModelError::Interrupted) => return Err(ModelError::Interrupted.into()),
                Err(error) => {
                    tracing::warn!(%error, "Context summary unavailable; using excerpts");
                }
            }
        }
        let view = self.budget.fit(&view, tools, self.max_tokens)?;
        if self.root() {
            let mut usage = self.budget.usage(&view, tools, self.max_tokens);
            usage.deferred_tool_count = self.tools.deferred_count() as u32;
            self.emit(EngineEvent::ContextUsage { usage }).await?;
        }
        Ok(view)
    }

    /// Streams one attempt. Text the user already saw in an earlier attempt
    /// (`shown`) is not repeated; a retry that diverges from it is marked by
    /// a paragraph break.
    async fn stream_once(
        &self,
        request: ModelRequest,
        shown: &mut String,
        run: &mut Run,
    ) -> Result<ModelResponse, StreamFailure> {
        let mut stream = self.provider.stream(request, self.cancel.clone());
        let mut response = Assembler::default();
        let mut attempt_text = String::new();
        let mut matching = true;
        let mut pending = String::new();
        let mut last_flush = Instant::now();
        let mut responding = false;
        let mut saw_calls = false;
        let result = loop {
            let flush_at = tokio::time::Instant::from_std(last_flush + DELTA_INTERVAL);
            let next = tokio::select! {
                next = stream.next() => next,
                _ = tokio::time::sleep_until(flush_at), if !pending.is_empty() => {
                    self.flush(&mut pending, run).await.map_err(StreamFailure::Fatal)?;
                    last_flush = Instant::now();
                    continue;
                }
            };
            let event = match next {
                None => break std::mem::take(&mut response).finish(),
                Some(Err(error)) => break Err(error),
                Some(Ok(event)) => event,
            };
            if let StreamEvent::Text(text) = &event
                && self.root()
            {
                attempt_text.push_str(text);
                let visible = if !matching {
                    text.clone()
                } else if attempt_text.len() <= shown.len() && shown.starts_with(&attempt_text) {
                    String::new()
                } else if let Some(extension) = attempt_text.strip_prefix(shown.as_str()) {
                    extension.to_owned()
                } else {
                    matching = false;
                    format!("\n\n{attempt_text}")
                };
                if !visible.is_empty() {
                    shown.push_str(&visible);
                    pending.push_str(&visible);
                    if !responding {
                        responding = true;
                        self.phase(TurnPhase::Responding)
                            .await
                            .map_err(StreamFailure::Fatal)?;
                    }
                    if last_flush.elapsed() >= DELTA_INTERVAL {
                        self.flush(&mut pending, run)
                            .await
                            .map_err(StreamFailure::Fatal)?;
                        last_flush = Instant::now();
                    }
                }
            }
            saw_calls |= matches!(event, StreamEvent::ToolCall { .. });
            response.push(event);
        };
        self.flush(&mut pending, run)
            .await
            .map_err(StreamFailure::Fatal)?;
        match result {
            Ok(response) => Ok(response),
            Err(ModelError::Interrupted) => {
                Err(StreamFailure::Fatal(ModelError::Interrupted.into()))
            }
            // Never replay a request whose tool calls may have been observed.
            Err(error) if saw_calls => Err(StreamFailure::Fatal(error.into())),
            Err(error) => Err(StreamFailure::Model(error)),
        }
    }

    async fn flush(&self, pending: &mut String, run: &mut Run) -> anyhow::Result<()> {
        if pending.is_empty() {
            return Ok(());
        }
        let text = std::mem::take(pending);
        run.streamed.push_str(&text);
        self.emit(EngineEvent::Delta { text }).await
    }
}

enum StreamFailure {
    /// A provider failure the retry policy may repeat.
    Model(ModelError),
    /// Cancellation, event-channel or other failures that end the run.
    Fatal(anyhow::Error),
}

/// Tool calls of the assistant message at the start of `messages` without a
/// result after it.
fn unanswered(messages: &[Message]) -> impl Iterator<Item = &crate::model::ToolCall> {
    let answered = messages[1..]
        .iter()
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect::<HashSet<_>>();
    messages[0]
        .tool_calls
        .iter()
        .filter(move |call| !answered.contains(call.id.as_str()))
}

/// A note from the application (not the user) at its place in the
/// conversation. Notes are user-role messages so every protocol keeps their
/// position and the cached system block holds only the system prompt.
pub(crate) fn application_note(text: &str) -> Message {
    Message::user(crate::prompts::application_note(text))
}

/// Every tool call id in `messages`.
fn call_ids(messages: &[Message]) -> HashSet<String> {
    messages
        .iter()
        .flat_map(|message| message.tool_calls.iter().map(|call| call.id.clone()))
        .collect()
}

/// Call ids key the action journal, so they must be unique within the turn,
/// not only within the current conversation: a compaction removes earlier
/// calls from it while their journal entries remain. Some servers restart
/// ids like `call_0` in every response; an id that is `issued` already (or
/// `recorded` in the journal) gets a `#n` suffix before it is checkpointed or
/// dispatched, which keeps the journal key stable across a resume. Every id
/// the response ends up with is added to `issued`.
fn unique_call_ids(
    assistant: &mut Message,
    issued: &mut HashSet<String>,
    recorded: impl Fn(&str) -> anyhow::Result<bool>,
) -> anyhow::Result<()> {
    let taken = |issued: &HashSet<String>, id: &str| -> anyhow::Result<bool> {
        Ok(issued.contains(id) || recorded(id)?)
    };
    let mut kept: std::collections::HashMap<String, usize> = Default::default();
    for call in assistant.tool_calls.clone() {
        if !taken(issued, &call.id)? {
            issued.insert(call.id.clone());
            *kept.entry(call.id).or_default() += 1;
            continue;
        }
        let mut suffix = 2;
        let renamed = loop {
            let candidate = format!("{}#{suffix}", call.id);
            if !taken(issued, &candidate)? {
                break candidate;
            }
            suffix += 1;
        };
        let occurrence = kept.get(&call.id).copied().unwrap_or(0);
        assistant.rename_tool_call_id(&call.id, occurrence, &renamed);
        issued.insert(renamed);
    }
    Ok(())
}

/// The newest non-empty assistant text.
fn last_text(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant && !m.content.trim().is_empty())
        .map(|m| m.content.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
