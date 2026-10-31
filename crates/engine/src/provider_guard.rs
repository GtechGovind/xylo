//! Model request admission and the context budget.
//!
//! [`GuardedProvider`] wraps the real provider: it admits a request through the
//! resource manager, holds the cross-process local-generation lease only while
//! a response streams (never across tool use), clamps the output limit and
//! enforces first-response and idle deadlines. Cancellation ends a stream
//! promptly with [`ModelError::Interrupted`].
//!
//! [`ContextBudget`] decides what fits the window. When history passes the
//! proactive threshold (80 % of the context), older history is replaced by a
//! model-written summary that becomes part of the durable checkpoint, so later
//! steps and later turns reuse it instead of re-summarising. Only if no summary
//! is possible are bounded excerpts used, and those affect one request only.
use crate::model::{
    EventStream, Message, ModelError, ModelProvider, ModelRequest, Role, ToolSchema,
};
use futures_util::StreamExt;
use std::{
    fs::{File, OpenOptions},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tiktoken_rs::CoreBPE;
use tokio_util::sync::CancellationToken;
use xylo_core::{ApiFamily, ContextUsage, ProviderConfig};
use xylo_resources::{ResourceManager, TaskDemand};

/// Non-OpenAI and custom tokenizers are estimated from o200k counts with a
/// safety margin; observed Claude, Gemini and Llama-family code/prose ratios
/// stay below it. Exact counts are used where the model's encoding is known.
const FOREIGN_TOKEN_SCALE: f64 = 1.3;
/// Output limit of a summary request.
const SUMMARY_TOKENS: u32 = 2048;
/// The summarizer's own deadline never exceeds this.
const SUMMARY_DEADLINE: Duration = Duration::from_secs(90);
/// Request framing not attributable to any message.
const FRAMING_TOKENS: usize = 256;
/// Per-tool framing on top of its schema.
const TOOL_FRAMING_TOKENS: usize = 64;

#[derive(Clone)]
pub(crate) struct GuardedProvider {
    inner: Arc<dyn ModelProvider>,
    context_tokens: u32,
    output_tokens: u32,
    local_lock: Option<PathBuf>,
    timeout: Duration,
    /// Local memory admission; only loopback and application-managed
    /// endpoints run on this machine, so cloud calls reserve nothing.
    resources: Option<ResourceManager>,
}

impl GuardedProvider {
    pub(crate) fn new(
        inner: Arc<dyn ModelProvider>,
        config: &ProviderConfig,
        lock: PathBuf,
        managed_single_slot: bool,
    ) -> Self {
        let local = url::Url::parse(&config.endpoint)
            .ok()
            .is_some_and(|url| matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")));
        Self {
            inner,
            context_tokens: config.context_tokens,
            output_tokens: config.output_tokens,
            local_lock: (local && managed_single_slot).then_some(lock),
            timeout: Duration::from_secs(config.timeout_seconds),
            resources: (local || managed_single_slot).then(ResourceManager::new),
        }
    }

    /// The provider's request deadline.
    pub(crate) fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Reserves memory for a local generation, waiting at most the provider
    /// deadline so a busy machine ends in a clear, retryable error.
    async fn admit(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Option<xylo_resources::ResourceLease>, ModelError> {
        let Some(resources) = &self.resources else {
            return Ok(None);
        };
        let demand = TaskDemand::interactive(self.context_tokens);
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(ModelError::Interrupted),
            admitted = tokio::time::timeout(self.timeout, resources.acquire(demand, cancel.clone())) => match admitted {
                Ok(lease) => lease.map(Some).map_err(admission_error),
                Err(_) => Err(ModelError::Timeout(
                    "Waiting for enough free memory to run the local model".into(),
                )),
            },
        }
    }

    async fn lease(&self, cancel: &CancellationToken) -> Result<Option<File>, ModelError> {
        let Some(path) = &self.local_lock else {
            return Ok(None);
        };
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| ModelError::Io(format!("Open local inference lock: {error}")))?;
        let start = tokio::time::Instant::now();
        loop {
            match fs2::FileExt::try_lock_exclusive(&file) {
                Ok(()) => return Ok(Some(file)),
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {}
                Err(error) => {
                    return Err(ModelError::Io(format!("Lock local inference: {error}")));
                }
            }
            tokio::select! {
                _ = cancel.cancelled() => return Err(ModelError::Interrupted),
                _ = tokio::time::sleep(Duration::from_millis(20)) => {}
            }
            if start.elapsed() >= self.timeout {
                return Err(ModelError::Timeout(
                    "Waiting for the local model to finish another generation".into(),
                ));
            }
        }
    }
}

fn admission_error(error: anyhow::Error) -> ModelError {
    if error.is::<xylo_resources::AdmissionCancelled>() {
        ModelError::Interrupted
    } else {
        ModelError::Io(error.to_string())
    }
}

impl ModelProvider for GuardedProvider {
    fn stream(&self, mut request: ModelRequest, cancel: CancellationToken) -> EventStream {
        request.max_tokens = request.max_tokens.min(self.output_tokens).max(1);
        let this = self.clone();
        Box::pin(async_stream::try_stream! {
            let _resources = this.admit(&cancel).await?;
            let _lease = this.lease(&cancel).await?;
            let mut stream = this.inner.stream(request, cancel.clone());
            let mut first = true;
            loop {
                let next = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => Err(ModelError::Interrupted),
                    next = tokio::time::timeout(this.timeout, stream.next()) => next.map_err(|_| {
                        ModelError::Timeout(if first {
                            "Model first response deadline exceeded"
                        } else {
                            "Model stream idle deadline exceeded"
                        }.into())
                    }),
                }?;
                first = false;
                let Some(event) = next else { break };
                yield event?;
            }
        })
    }
}

/// Where older history should be condensed.
pub(crate) struct SummaryPlan {
    /// History index from which messages are kept verbatim.
    pub cut: usize,
    /// The current request, moved after the summary when the cut passes it.
    pub anchor: Option<usize>,
    /// The first history index the new summary covers (after a prior summary).
    pub floor: usize,
}

/// Token accounting and fitting for one model and context window.
#[derive(Clone)]
pub(crate) struct ContextBudget {
    context_tokens: u32,
    output_tokens: u32,
    bpe: &'static CoreBPE,
    token_scale: f64,
}

impl ContextBudget {
    pub(crate) fn new(config: &ProviderConfig) -> Self {
        let openai = matches!(
            config.family,
            ApiFamily::OpenAiChat | ApiFamily::OpenAiResponses | ApiFamily::Azure
        );
        let exact = openai
            .then(|| tiktoken_rs::bpe_for_model(&config.model).ok())
            .flatten();
        Self {
            context_tokens: config.context_tokens,
            output_tokens: config.output_tokens,
            bpe: exact.unwrap_or_else(tiktoken_rs::o200k_base_singleton),
            token_scale: if exact.is_some() {
                1.0
            } else {
                FOREIGN_TOKEN_SCALE
            },
        }
    }

    fn tokens(&self, text: &str) -> usize {
        let count = self.bpe.encode_ordinary(text).len();
        (count as f64 * self.token_scale).ceil() as usize
    }

    /// Wire-format estimate of one message, including image patches. Image
    /// bytes are hydrated after budgeting, so base64 is never counted as text.
    fn message_cost(&self, message: &Message) -> usize {
        let image_tokens = message
            .attachments
            .iter()
            .map(|image| {
                let width = u64::from(image.width).max(1);
                let height = u64::from(image.height).max(1);
                let longest = width.max(height);
                let scale = 4096_f64.min(longest as f64) / longest as f64;
                let width = (width as f64 * scale).ceil() as usize;
                let height = (height as f64 * scale).ceil() as usize;
                width
                    .div_ceil(32)
                    .saturating_mul(height.div_ceil(32))
                    .saturating_add(1024)
            })
            .fold(0usize, usize::saturating_add);
        let encoded = serde_json::to_string(message).unwrap_or_default();
        self.tokens(&encoded)
            .saturating_add(image_tokens)
            .saturating_add(8)
    }

    fn costs(&self, messages: &[Message]) -> Vec<usize> {
        messages.iter().map(|m| self.message_cost(m)).collect()
    }

    fn tool_tokens(&self, tools: &[ToolSchema]) -> usize {
        let schemas = serde_json::to_string(tools).unwrap_or_default();
        self.tokens(&schemas)
            .saturating_add(tools.len() * TOOL_FRAMING_TOKENS)
    }

    /// The output reserve actually requested.
    pub(crate) fn output_reserve(&self, max_tokens: u32) -> u32 {
        max_tokens.min(self.output_tokens)
    }

    /// Message tokens available within `context_tokens` after tools, framing
    /// and the output reserve.
    fn allowance(&self, tools: &[ToolSchema], max_tokens: u32, context_tokens: u32) -> usize {
        let fixed = self
            .tool_tokens(tools)
            .saturating_add(FRAMING_TOKENS)
            .saturating_add(self.output_reserve(max_tokens) as usize);
        (context_tokens as usize).saturating_sub(fixed)
    }

    fn proactive_context(&self) -> u32 {
        self.context_tokens / 5 * 4
    }

    /// Whether `messages` pass the proactive compaction threshold.
    pub(crate) fn over_threshold(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        max_tokens: u32,
    ) -> bool {
        self.costs(messages).iter().sum::<usize>()
            > self.allowance(tools, max_tokens, self.proactive_context())
    }

    /// Exact accounting of a fitted request.
    pub(crate) fn usage(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        max_tokens: u32,
    ) -> ContextUsage {
        let mut usage = ContextUsage {
            capacity: u64::from(self.context_tokens),
            output_reserve: u64::from(self.output_reserve(max_tokens)),
            tools: self.tool_tokens(tools) as u64,
            tool_count: tools.len() as u32,
            system: FRAMING_TOKENS as u64,
            autocompact_threshold: u64::from(self.proactive_context()),
            ..ContextUsage::default()
        };
        for message in messages {
            let cost = self.message_cost(message) as u64;
            if message.summary {
                usage.summary += cost;
                usage.compacted = true;
            } else if message.role == Role::System {
                usage.system += cost;
            } else {
                usage.messages += cost;
            }
        }
        usage
    }

    /// Where to condense `history` (the conversation without the system
    /// prompt, which costs `system_cost`). `None` when condensing would not
    /// free enough to be worth a model call. Cuts never separate a tool result
    /// from its call: they start at a user message, or at an assistant message
    /// after the current request.
    pub(crate) fn summary_plan(
        &self,
        system: &[Message],
        history: &[Message],
        tools: &[ToolSchema],
        max_tokens: u32,
    ) -> Option<SummaryPlan> {
        let allowance = self.allowance(tools, max_tokens, self.proactive_context());
        let costs = self.costs(history);
        let fixed = self.costs(system).iter().sum::<usize>() + SUMMARY_TOKENS as usize;
        if allowance <= fixed {
            return None;
        }
        let anchor = history.iter().rposition(|m| m.role == Role::User)?;
        let floor = usize::from(history.first().is_some_and(|m| m.summary));
        let valid = |index: usize| match history[index].role {
            Role::User => !history[index].summary,
            Role::Assistant => index > anchor,
            _ => false,
        };
        // Keep a tail of about half the allowance, leaving room for many
        // further steps before the next summary.
        let tail_budget = (allowance - fixed) / 2;
        let mut cut = None;
        let mut tail = 0usize;
        for index in (floor + 1..history.len()).rev() {
            tail += costs[index];
            let task = if anchor < index { costs[anchor] } else { 0 };
            if valid(index) {
                if tail + task > tail_budget && cut.is_some() {
                    break;
                }
                cut = Some(index);
            }
        }
        let cut = cut.filter(|cut| {
            costs[floor..*cut].iter().sum::<usize>() >= 2 * SUMMARY_TOKENS as usize
        })?;
        Some(SummaryPlan {
            cut,
            anchor: (anchor < cut).then_some(anchor),
            floor,
        })
    }

    /// Asks the model to condense `history[plan.floor..plan.cut]`, updating a
    /// previous summary when there is one. Has its own deadline of
    /// min(provider timeout, 90 s). Returns the replacement history.
    pub(crate) async fn summarize(
        &self,
        provider: &dyn ModelProvider,
        history: &[Message],
        plan: &SummaryPlan,
        deadline: Duration,
        cancel: &CancellationToken,
    ) -> Result<Vec<Message>, ModelError> {
        let previous = history
            .first()
            .filter(|message| message.summary)
            .map(|message| message.content.as_str());
        // The summarizer's own request must fit comfortably in the context.
        let budget = (self.context_tokens as usize * 3 / 5).saturating_sub(SUMMARY_TOKENS as usize);
        let covered = &history[plan.floor..plan.cut];
        let mut transcript = String::new();
        for each in [4000, 2000, 1000, 500, 250, 120] {
            transcript = render_transcript(covered, each);
            if self.tokens(&transcript) <= budget {
                break;
            }
        }
        if self.tokens(&transcript) > budget {
            transcript = context_excerpt(&transcript, budget.saturating_mul(3));
        }
        let request = ModelRequest {
            messages: vec![
                Message::system(crate::prompts::compaction()),
                Message::user(crate::prompts::compaction_request(previous, &transcript)),
            ],
            tools: Vec::new(),
            max_tokens: SUMMARY_TOKENS.min(self.output_tokens),
            reasoning: crate::model::Reasoning::Configured,
        };
        let reply = tokio::time::timeout(
            deadline.min(SUMMARY_DEADLINE),
            provider.complete(request, cancel.clone()),
        )
        .await
        .map_err(|_| ModelError::Timeout("Context summary deadline exceeded".into()))??;
        let summary = reply.message.content.trim();
        if summary.len() < 40 {
            return Err(ModelError::permanent("Context summary was empty"));
        }
        let mut message = Message::system(crate::prompts::compaction_summary(summary));
        message.summary = true;
        let mut condensed = vec![message];
        if let Some(anchor) = plan.anchor {
            condensed.push(history[anchor].clone());
        }
        condensed.extend_from_slice(&history[plan.cut..]);
        Ok(condensed)
    }

    /// Fits a request by bounded excerpts when no summary could: compacts
    /// history before the current request, shortens large tool results and
    /// condenses whole completed tool rounds. Pending tool calls are never
    /// dropped. Fails with [`ModelError::ContextOverflow`] when even that
    /// cannot fit the window.
    pub(crate) fn fit(
        &self,
        messages: &[Message],
        tools: &[ToolSchema],
        max_tokens: u32,
    ) -> Result<Vec<Message>, ModelError> {
        let mut view = messages.to_vec();
        let proactive = self.allowance(tools, max_tokens, self.proactive_context());
        let hard = self.allowance(tools, max_tokens, self.context_tokens);
        let mut costs = self.costs(&view);
        if costs.iter().sum::<usize>() <= proactive {
            return Ok(view);
        }
        let last_user = view.iter().rposition(|m| m.role == Role::User).unwrap_or(0);
        if last_user > 1 {
            let mut compacted = view[..last_user]
                .iter()
                .filter(|m| m.role == Role::System)
                .cloned()
                .collect::<Vec<_>>();
            let allowance = (self.context_tokens as usize / 8).clamp(256, 8192);
            let older = &view[..last_user];
            let each = (allowance / older.len().max(1)).clamp(32, 512);
            let mut notes = format!("{}\n", crate::prompts::compaction_excerpts_header());
            for message in older.iter().filter(|m| m.role != Role::System) {
                if !message.content.is_empty() {
                    notes.push_str(&format!(
                        "{:?}: {}\n",
                        message.role,
                        context_excerpt(&message.content, each)
                    ));
                }
            }
            compacted.push(Message::assistant(context_excerpt(&notes, allowance)));
            compacted.extend_from_slice(&view[last_user..]);
            view = compacted;
            costs = self.costs(&view);
        }
        // Tool outputs can overflow even a short conversation. Keep both ends
        // (including artifact references) and the call/result pairing; the full
        // observation remains in the action journal.
        for limit in [4096, 2048, 1024, 512] {
            if costs.iter().sum::<usize>() <= proactive {
                break;
            }
            for (message, cost) in view.iter_mut().zip(costs.iter_mut()) {
                if message.role == Role::Tool && message.content.len() > limit {
                    message.content = context_excerpt(&message.content, limit);
                    *cost = self.message_cost(message);
                }
            }
        }
        // Condense whole completed rounds; never orphan a call or result.
        let mut index = 0;
        while costs.iter().sum::<usize>() > hard && index < view.len() {
            if view[index].tool_calls.is_empty() {
                index += 1;
                continue;
            }
            let end = (index + 1..view.len())
                .find(|i| view[*i].role != Role::Tool)
                .unwrap_or(view.len());
            let calls = &view[index].tool_calls;
            let results = &view[index + 1..end];
            let complete = calls.iter().all(|call| {
                results
                    .iter()
                    .any(|result| result.tool_call_id.as_deref() == Some(call.id.as_str()))
            });
            if !complete {
                index += 1;
                continue;
            }
            let mut notes = format!("{}\n", crate::prompts::compaction_tool_notes_header());
            for call in calls {
                if let Some(result) = results
                    .iter()
                    .find(|result| result.tool_call_id.as_deref() == Some(call.id.as_str()))
                {
                    notes.push_str(&format!(
                        "{} ({}): {}\n",
                        call.name,
                        call.id,
                        context_excerpt(&result.content, 192)
                    ));
                }
            }
            let replacement = Message::assistant(notes);
            let replacement_cost = self.message_cost(&replacement);
            if replacement_cost < costs[index..end].iter().sum::<usize>() {
                view.splice(index..end, [replacement]);
                costs.splice(index..end, [replacement_cost]);
            }
            index += 1;
        }
        let input = costs.iter().sum::<usize>();
        if input > hard {
            return Err(ModelError::ContextOverflow(format!(
                "Context budget exceeded: input estimate {input} tokens exceeds the {hard} available after the output reserve in the configured {} token context. Start a new chat or increase the configured model context.",
                self.context_tokens
            )));
        }
        Ok(view)
    }
}

/// Role-labelled plain-text rendering of history for the summarizer.
fn render_transcript(history: &[Message], each: usize) -> String {
    let mut output = String::new();
    for message in history {
        let role = match message.role {
            Role::System => "System note",
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::Tool => "Tool result",
        };
        // User requests are kept in full more often; observations shrink first.
        let budget = match message.role {
            Role::User => each * 2,
            Role::Tool => each / 2,
            _ => each,
        };
        if !message.content.trim().is_empty() {
            output.push_str(&format!(
                "{role}: {}\n",
                context_excerpt(&message.content, budget)
            ));
        }
        for call in &message.tool_calls {
            output.push_str(&format!(
                "Assistant called {}({})\n",
                call.name,
                context_excerpt(&call.arguments, budget / 2)
            ));
        }
    }
    output
}

/// UTF-8-safe bounded excerpts explicitly signal loss; never label them as a
/// complete semantic summary of the original conversation or tool output.
pub(crate) fn context_excerpt(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_owned();
    }
    const MARKER: &str = "\n[… omitted; full content retained in conversation/action history …]\n";
    let keep = budget.saturating_sub(MARKER.len()) / 2;
    let mut head = keep.min(text.len());
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len().saturating_sub(keep);
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!("{}{}{}", &text[..head], MARKER, &text[tail..])
}

#[cfg(test)]
mod tests;
