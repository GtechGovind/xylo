//! Xylo's agent loop.
//!
//! One step: fit the conversation to the context window (masking old tool
//! results in batches and condensing older history into a durable summary
//! when needed; a request the provider rejects as too long is compacted
//! harder and sent once more), stream a model call, flush its
//! text deltas, and finish if it asked for no tools. Otherwise checkpoint the
//! assistant message, execute the tool batch, append the results in call
//! order, checkpoint again, apply any queued user guidance, and repeat.
//!
//! Recovery is deterministic and driven by what the harness observes, never
//! by the model's own account:
//! - **Finish reasons.** A reply without tools is classified by its finish
//!   reason ([`recovery`]): one the output limit cut off is continued and
//!   joined into one answer, an empty or unparseable reply is asked for once
//!   more. A tool call the output limit cut off is never dispatched; it gets
//!   an error result asking for smaller arguments, checkpointed with the
//!   call before anything runs so a resume cannot dispatch it either.
//! - **Stalls.** Every tool round feeds a loop detector ([`stall`]) that
//!   escalates from a note to a warning to stopping the turn with
//!   [`ModelError::Stalled`].
//! - **Step budget.** Notes at 75% and 90% of the step limit ask the model to
//!   prioritize finishing.
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
//!   prompt and application notes: the permission-mode change, the step-limit
//!   note on the final call of a runaway turn, and the recovery, stall and
//!   step-budget notes above. Notes are user-role messages at their place in
//!   the conversation, so the cached system block holds only the system
//!   prompt, and a root loop shows each recovery note to the user as it is
//!   added.
mod dispatch;
mod recovery;
mod retry;
mod stall;

pub(crate) use dispatch::{Access, ToolExecutor, error_result};

use crate::{
    model::{
        Assembler, Message, ModelError, ModelProvider, ModelRequest, ModelResponse, Role,
        StreamEvent, ToolSchema, TruncatedCall,
    },
    provider_guard::{ContextBudget, Pressure},
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
use xylo_core::{ContextUsage, EngineEvent, NoteKind, PermissionMode, TurnPhase, Usage};
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
    /// The checkpoint this journal last wrote, so the next write sends only
    /// the messages that changed since.
    written: tokio::sync::Mutex<Option<Written>>,
}

/// One checkpoint write: its store revision and each message serialized.
struct Written {
    revision: u64,
    rows: Arc<[String]>,
}

impl Journal {
    pub(crate) fn new(
        store: Store,
        session: Uuid,
        turn: Uuid,
        steering: Option<SteeringControl>,
    ) -> Self {
        Self {
            store,
            session,
            turn,
            steering,
            written: tokio::sync::Mutex::new(None),
        }
    }

    /// Saves `messages` as the turn's checkpoint. Serialization and the write
    /// run off the async threads, and only the messages from the first one
    /// that differs from the previous write are stored again: a tool round
    /// appends, while masking or compaction rewrites from the first message
    /// it changed.
    async fn checkpoint(&self, messages: &[Message]) -> anyhow::Result<()> {
        let mut written = self.written.lock().await;
        // Cleared until this write succeeds; the store then finds the
        // changed suffix by comparing with what it holds.
        let previous = written.take();
        let messages = messages.to_vec();
        let (store, session, turn) = (self.store.clone(), self.session, self.turn);
        let saved = tokio::task::spawn_blocking(move || {
            let rows = messages
                .iter()
                .map(serde_json::to_string)
                .collect::<serde_json::Result<Arc<[String]>>>()?;
            let base = previous.map(|previous| xylo_services::CheckpointBase {
                revision: previous.revision,
                unchanged: previous
                    .rows
                    .iter()
                    .zip(rows.iter())
                    .take_while(|(before, now)| before == now)
                    .count(),
            });
            let revision = store.save_checkpoint_messages(session, turn, rows.clone(), base)?;
            anyhow::Ok(Written { revision, rows })
        })
        .await
        .context("Checkpoint task stopped")??;
        *written = Some(saved);
        Ok(())
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
    /// The answer reached the output limit and is shown as far as it got.
    pub cut_off: bool,
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
    /// The final answer reached the output limit.
    cut_off: bool,
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
            cut_off: false,
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
                    cut_off: run.cut_off,
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
        let mut answer = recovery::AnswerRecovery::default();
        let mut stall = stall::StallDetector::default();
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
            if !final_call && step_budget_warning(calls, self.max_model_calls) {
                messages.push(application_note(&crate::prompts::step_budget(
                    calls,
                    self.max_model_calls,
                )));
                self.show_note(
                    NoteKind::StepBudget,
                    format!(
                        "Step {calls} of at most {}: Xylo asked the model to prioritize finishing.",
                        self.max_model_calls
                    ),
                )
                .await?;
            }
            let ModelResponse {
                message: mut assistant,
                finish,
                truncated_calls,
                ..
            } = self.model_call(messages, final_call, run).await?;
            self.assign_call_ids(&mut issued, &mut assistant).await?;
            let text = assistant.content.clone();
            if assistant.tool_calls.is_empty() {
                match answer.reply(finish, &text, final_call) {
                    recovery::Reply::Answer { text, cut_off } => {
                        if cut_off {
                            self.show_note(
                                NoteKind::Truncated,
                                "The answer reached the output limit again; it is shown as far as it got."
                                    .into(),
                            )
                            .await?;
                        }
                        messages.push(assistant);
                        // Guidance that arrived while the answer streamed
                        // continues the task instead of being left unread.
                        if !final_call && self.apply_guidance(messages).await? > 0 {
                            continue;
                        }
                        run.cut_off = cut_off;
                        return Ok((text, false));
                    }
                    recovery::Reply::Continue => {
                        messages.push(assistant);
                        messages.push(application_note(crate::prompts::continue_answer()));
                        self.show_note(
                            NoteKind::Continuation,
                            "The reply reached the output limit; Xylo asked the model to continue it."
                                .into(),
                        )
                        .await?;
                    }
                    recovery::Reply::Retry { note, keep_reply } => {
                        if keep_reply {
                            messages.push(assistant);
                        }
                        messages.push(application_note(note));
                        self.show_note(
                            NoteKind::Retry,
                            "The model's reply had no usable answer or tool call; Xylo asked it once more."
                                .into(),
                        )
                        .await?;
                    }
                    recovery::Reply::Fail(error) => return Err(error.into()),
                }
                continue;
            }
            answer.reset();
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
            let (prepared, refused) = self.prepare_calls(&mut assistant, &truncated_calls).await?;
            let order = call_order(&assistant);
            messages.push(assistant);
            let index = messages.len() - 1;
            // A cut-off call's arguments are already cleared, so a resume
            // could not tell it from a real call without arguments: its
            // refusal is durable together with the call it answers.
            let refusals = !refused.is_empty();
            messages.extend(refused);
            self.checkpoint(messages).await?;
            self.run_batch(messages, prepared).await?;
            if refusals {
                messages[index + 1..].sort_by_key(|result| {
                    result
                        .tool_call_id
                        .as_ref()
                        .and_then(|id| order.iter().position(|call| call == id))
                });
                self.checkpoint(messages).await?;
            }
            self.watch_progress(&mut stall, messages, index).await?;
        }
    }

    /// Validates the calls of `assistant` for dispatch, recording each call
    /// as it will run, and returns them with the error results of calls the
    /// output limit cut off. Those are never dispatched: their partial
    /// arguments could still parse after repair and do the wrong thing, so
    /// each gets an error asking for smaller arguments instead.
    async fn prepare_calls(
        &self,
        assistant: &mut Message,
        truncated: &[TruncatedCall],
    ) -> anyhow::Result<(Vec<(String, dispatch::Prepared)>, Vec<Message>)> {
        let mut prepared = Vec::with_capacity(assistant.tool_calls.len());
        let mut refused = Vec::new();
        for (position, call) in assistant.tool_calls.clone().into_iter().enumerate() {
            if let Some(cut) = truncated.iter().find(|cut| cut.position == position) {
                let error = crate::prompts::truncated_tool_call(&call.name, cut.received);
                refused.push(Message::tool_result(call.id, error_result(error)));
                self.show_note(NoteKind::Retry, format!(
                    "The output limit cut off a `{}` call; Xylo asked the model to send it again in smaller parts.",
                    call.name
                ))
                .await?;
                continue;
            }
            let ready = dispatch::prepare(&call, self.tools.as_ref());
            dispatch::record(assistant, &call, &ready);
            prepared.push((call.id, ready));
        }
        Ok((prepared, refused))
    }

    /// Feeds the tool round of the assistant message at `index` (its results
    /// follow it) to the stall detector and acts on its verdict: a note or a
    /// warning is appended after the results; a third strike stops the turn.
    async fn watch_progress(
        &self,
        stall: &mut stall::StallDetector,
        messages: &mut Vec<Message>,
        index: usize,
    ) -> anyhow::Result<()> {
        let step = stall::Step::new(&messages[index].tool_calls, &messages[index + 1..]);
        let (kind, note, shown) = match stall.observe(step) {
            None => return Ok(()),
            Some(stall::Verdict::Note(pattern)) => (
                NoteKind::StallNotice,
                crate::prompts::stall_note(&pattern.describe()),
                format!(
                    "Xylo noticed {} and asked the model to change approach.",
                    pattern.summary()
                ),
            ),
            Some(stall::Verdict::Warning(pattern)) => (
                NoteKind::StallWarning,
                crate::prompts::stall_warning(&pattern.describe()),
                format!(
                    "Xylo noticed {} again and warned the model that the task stops if it continues.",
                    pattern.summary()
                ),
            ),
            Some(stall::Verdict::Stop(pattern)) => {
                return Err(ModelError::Stalled(format!(
                    "Xylo stopped the task: the model kept repeating itself ({}) after two warnings. \
                     Add guidance and send the task again.",
                    pattern.summary()
                ))
                .into());
            }
        };
        tracing::info!(%shown, "Stall detected");
        messages.push(application_note(&note));
        self.show_note(kind, shown).await
    }

    /// Shows a harness note to the user of a root loop. The model-facing note
    /// itself travels in the conversation as an application note.
    async fn show_note(&self, kind: NoteKind, text: String) -> anyhow::Result<()> {
        if self.root() {
            self.emit(EngineEvent::HarnessNote { kind, text }).await?;
        }
        Ok(())
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
                dispatch::record(&mut assistant, &call, &prepared);
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
        let mut shown = String::new();
        // A context overflow (estimated before sending, or reported by the
        // provider) is recovered once: compact harder and send again.
        let mut pressure = Pressure::Normal;
        'fitting: loop {
            let (mut request_messages, mut fitted) =
                match self.fit(messages, &tools, pressure).await {
                    Err(error) if pressure == Pressure::Normal && context_overflow(&error) => {
                        tracing::warn!(%error, "Request does not fit; compacting harder");
                        pressure = Pressure::Overflow;
                        continue;
                    }
                    fitted => fitted?,
                };
            if final_call {
                let note = application_note(crate::prompts::step_limit());
                fitted.messages += self.budget.message_cost(&note) as u64;
                request_messages.push(note);
            }
            let request = ModelRequest {
                messages: request_messages,
                tools: tools.clone(),
                max_tokens: self.max_tokens,
                reasoning: crate::model::Reasoning::Configured,
            };
            let mut attempt = 0;
            loop {
                self.phase(TurnPhase::Thinking).await?;
                let error = match self.stream_once(request.clone(), &mut shown, run).await {
                    Ok(response) => {
                        if let Some(usage) = response.usage {
                            run.usage.input_tokens += usage.input_tokens;
                            run.usage.output_tokens += usage.output_tokens;
                            run.usage.context_tokens = usage.input_tokens;
                            add_reported(&mut run.usage.cache_read_tokens, usage.cache_read_tokens);
                            add_reported(
                                &mut run.usage.cache_write_tokens,
                                usage.cache_write_tokens,
                            );
                            if self.root() {
                                // The request as fitted; it is not measured again.
                                let mut context = fitted.clone();
                                context.provider_input = Some(usage.input_tokens);
                                context.provider_cache_read = usage.cache_read_tokens;
                                self.emit(EngineEvent::ContextUsage { usage: context })
                                    .await?;
                            }
                            self.budget.observe(
                                &request.messages,
                                &request.tools,
                                usage.input_tokens,
                            );
                        }
                        return Ok(response);
                    }
                    Err(StreamFailure::Fatal(error)) => return Err(error),
                    Err(StreamFailure::Model(ModelError::ContextOverflow(error)))
                        if pressure == Pressure::Normal =>
                    {
                        tracing::warn!(%error, "Provider rejected the request as too long; compacting harder");
                        self.budget.penalize_overflow();
                        pressure = Pressure::Overflow;
                        continue 'fitting;
                    }
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
    }

    /// The request view of `messages`: the system prompt plus history, with
    /// its accounting. Old tool results are masked in batches and history
    /// past the threshold is condensed into a summary; both change `messages`
    /// itself and are checkpointed, so later calls and resumes send identical
    /// bytes.
    async fn fit(
        &self,
        messages: &mut Vec<Message>,
        tools: &[ToolSchema],
        pressure: Pressure,
    ) -> anyhow::Result<(Vec<Message>, ContextUsage)> {
        let system = vec![Message::system(&self.system)];
        let assemble = |history: &[Message]| {
            let mut view = system.clone();
            view.extend_from_slice(history);
            view
        };
        if let Some(plan) =
            self.budget
                .mask_plan(&system, messages, tools, self.max_tokens, pressure)
        {
            tracing::debug!(results = plan.stubs.len(), "Masking old tool results");
            plan.apply(messages);
            self.checkpoint(messages).await?;
        }
        let mut view = assemble(messages);
        if (pressure == Pressure::Overflow
            || self.budget.over_threshold(&view, tools, self.max_tokens))
            && let Some(plan) =
                self.budget
                    .summary_plan(&system, messages, tools, self.max_tokens, pressure)
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
        let (view, costs) = self.budget.fit(view, tools, self.max_tokens)?;
        let mut usage = self.budget.usage_of(&view, &costs, tools, self.max_tokens);
        if self.root() {
            usage.deferred_tool_count = self.tools.deferred_count() as u32;
            self.emit(EngineEvent::ContextUsage {
                usage: usage.clone(),
            })
            .await?;
        }
        Ok((view, usage))
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

/// Whether model call `call` (1-based) of at most `limit` crosses 75% or
/// 90% of the step limit. Thresholds that would coincide with the final call
/// are skipped: that call already carries the step-limit note.
fn step_budget_warning(call: u32, limit: u32) -> bool {
    [75, 90]
        .into_iter()
        .map(|percent| (u64::from(limit) * percent).div_ceil(100))
        .any(|threshold| threshold < u64::from(limit) && u64::from(call) == threshold)
}

/// Whether `error` is a context overflow, which a stronger compaction may fix.
fn context_overflow(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<ModelError>(),
        Some(ModelError::ContextOverflow(_))
    )
}

/// A note from the application (not the user) at its place in the
/// conversation. Notes are user-role messages so every protocol keeps their
/// position and the cached system block holds only the system prompt.
/// Adds a count some calls may not report: the total stays `None` until a
/// call reports one.
fn add_reported(total: &mut Option<u64>, reported: Option<u64>) {
    if let Some(reported) = reported {
        *total = Some(total.unwrap_or_default() + reported);
    }
}

pub(crate) fn application_note(text: &str) -> Message {
    Message::user(crate::prompts::application_note(text))
}

/// The call ids of `assistant` in call order.
fn call_order(assistant: &Message) -> Vec<String> {
    assistant
        .tool_calls
        .iter()
        .map(|call| call.id.clone())
        .collect()
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
