//! Pure view-state reducer for the live turn of each chat.
//!
//! Engine events cannot leak between selected chats: each chat owns one
//! [`TurnState`]. A turn is one chronological transcript: assistant text and
//! tool activity become [`Segment`]s in the order the engine reported them, and
//! calls made by a helper agent (events carrying `parent_id`) fold under that
//! agent's own [`Subagent`] row instead of interleaving with the parent's calls.
//! Every terminal event settles unfinished cards, so nothing spins forever.
use gpui_kit::SharedString;
use uuid::Uuid;
use xylo_core::{
    ChatMessage, ContextUsage, EngineEvent, ErrorKind, MessageStatus, TurnPhase, TurnRequest,
    Usage, now_ms,
};
#[path = "tool_presentation.rs"]
pub mod tool_presentation;

/// The tool that starts a helper agent.
pub const DELEGATE_TOOL: &str = "agent_delegate";
/// The agent's checklist tool; only its newest call is shown.
pub const PLAN_TOOL: &str = "todo_write";

#[derive(Clone, Debug)]
pub struct ToolCard {
    pub id: Uuid,
    pub name: String,
    /// What the action does, as presented by the engine (for changes, a diff).
    pub input: serde_json::Value,
    pub result: Option<String>,
    pub presentation: tool_presentation::ToolPresentation,
    pub approval: bool,
    /// What "always allow" would cover, when the action can be remembered.
    pub rule: Option<String>,
    /// Why the current permission mode asks before this action.
    pub reason: Option<String>,
    /// The `agent_delegate` action whose child agent issued this call.
    pub parent_id: Option<Uuid>,
    pub success: Option<bool>,
    /// The turn ended before this call reported a result.
    pub not_completed: bool,
    /// The call finished unsuccessfully while still waiting for a decision:
    /// the host could not deliver the user's answer.
    pub decision_lost: bool,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}
impl ToolCard {
    /// A call that has just started, with nothing decided or finished yet.
    pub fn new(id: Uuid, name: String, input: serde_json::Value, parent_id: Option<Uuid>) -> Self {
        Self {
            presentation: tool_presentation::ToolPresentation::prepare(&name, &input, None, None),
            id,
            name,
            input,
            result: None,
            approval: false,
            rule: None,
            reason: None,
            parent_id,
            success: None,
            not_completed: false,
            decision_lost: false,
            started_at: now_ms(),
            finished_at: None,
        }
    }
    pub fn refresh_presentation(&mut self) {
        self.presentation = tool_presentation::ToolPresentation::prepare(
            &self.name,
            &self.input,
            self.result.as_deref(),
            self.success,
        );
    }
    /// The parsed result envelope, when the result is structured.
    pub fn output(&self) -> Option<serde_json::Value> {
        self.presentation.parsed_output.as_deref().cloned()
    }
    /// Finished successfully and waiting on nothing.
    pub fn is_done(&self) -> bool {
        !self.approval && self.success == Some(true)
    }
}

/// One chronological piece of a turn's transcript.
#[derive(Clone, Debug, PartialEq)]
pub enum Segment {
    /// Assistant text streamed between tool batches. Shared so an unchanged
    /// segment hands the markdown view the same string every frame.
    Text(SharedString),
    /// Consecutive top-level tool calls, in start order.
    Tools(Vec<Uuid>),
    /// A helper agent started by an `agent_delegate` call with this id.
    Subagent(Uuid),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubagentState {
    Running,
    Completed,
    Failed,
    /// The parent turn ended before the helper reported back.
    NotCompleted,
}

/// A helper agent and the calls it made.
#[derive(Clone, Debug)]
pub struct Subagent {
    /// The `agent_delegate` action that started it.
    pub id: Uuid,
    pub task: String,
    /// Its own tool calls, in start order.
    pub tools: Vec<Uuid>,
    /// Its final answer (markdown) or failure reason, once finished.
    pub summary: Option<SharedString>,
    pub state: SubagentState,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}
impl Subagent {
    pub fn elapsed_ms(&self) -> i64 {
        (self.finished_at.unwrap_or_else(now_ms) - self.started_at).max(0)
    }
}

/// Why a turn failed, as classified by the engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnFailure {
    pub message: String,
    pub kind: ErrorKind,
    pub retryable: bool,
}

/// How a finished turn ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnOutcome {
    Completed,
    /// The user stopped it; streamed text and finished calls are kept.
    Stopped,
    Failed(TurnFailure),
}

#[derive(Default)]
pub struct TurnState {
    /// Changes only when a reducer event alters this turn, never while rendering.
    pub render_revision: u64,
    pub running: bool,
    /// The transcript in event order.
    pub segments: Vec<Segment>,
    /// Every card of this turn, including helper-agent calls.
    pub tools: Vec<ToolCard>,
    pub subagents: Vec<Subagent>,
    /// Helpers of earlier turns in this chat, newest last, for the Subagents panel.
    pub earlier_subagents: Vec<Subagent>,
    /// The latest engine status note.
    pub status: Option<String>,
    /// `None` while running or before the first turn.
    pub outcome: Option<TurnOutcome>,
    pub usage: Usage,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    /// What the turn is doing now; `None` once it ends.
    pub phase: Option<TurnPhase>,
    /// When `phase` last changed, for countdowns.
    pub phase_since: i64,
    /// Context accounting of the latest model request; kept after the turn ends.
    pub context: Option<ContextUsage>,
    /// The newest checklist; earlier `todo_write` calls fold into it.
    pub plan: Option<Uuid>,
    /// The request of the chat's latest admitted turn, recorded by the UI at
    /// admission. Retrying a failed turn sends it again with the same id.
    pub request: Option<TurnRequest>,
    /// The user retried this failed turn; when it reopens, its partial reply
    /// is replaced rather than kept as history.
    pub resend: bool,
}

/// Earlier helpers kept for the Subagents panel.
const EARLIER_SUBAGENTS: usize = 64;

impl TurnState {
    /// Actions currently waiting for the user's decision.
    pub fn pending_approvals(&self) -> impl Iterator<Item = &ToolCard> {
        self.tools.iter().filter(|tool| tool.approval)
    }
    pub fn tool(&self, id: Uuid) -> Option<&ToolCard> {
        self.tools.iter().find(|tool| tool.id == id)
    }
    pub fn subagent(&self, id: Uuid) -> Option<&Subagent> {
        self.subagents.iter().find(|agent| agent.id == id)
    }
    pub fn failure(&self) -> Option<&TurnFailure> {
        match &self.outcome {
            Some(TurnOutcome::Failed(failure)) => Some(failure),
            _ => None,
        }
    }
    pub fn stopped(&self) -> bool {
        self.outcome == Some(TurnOutcome::Stopped)
    }
    /// Milliseconds since the turn started, frozen once it ends.
    pub fn elapsed_ms(&self) -> i64 {
        self.started_at.map_or(0, |start| {
            (self.finished_at.unwrap_or_else(now_ms) - start).max(0)
        })
    }
    /// All streamed text of this turn, in order.
    pub fn transcript_text(&self) -> String {
        self.segments
            .iter()
            .filter_map(|segment| match segment {
                Segment::Text(text) => Some(text.as_ref()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }
    /// The finished turn's reply as a history message would hold it: the last
    /// text it streamed (the final answer once completed) and how it ended.
    pub fn reply(&self) -> Option<(String, MessageStatus)> {
        let status = match self.outcome.as_ref()? {
            TurnOutcome::Completed => MessageStatus::Complete,
            TurnOutcome::Stopped => MessageStatus::Interrupted,
            TurnOutcome::Failed(_) => MessageStatus::Failed,
        };
        self.segments
            .iter()
            .rev()
            .find_map(|segment| match segment {
                Segment::Text(text) if !text.trim().is_empty() => Some((text.to_string(), status)),
                _ => None,
            })
    }
    /// Clears the transcript of a turn that history now holds. Context, usage
    /// and helper records survive.
    pub fn archive(&mut self) {
        self.render_revision = self.render_revision.wrapping_add(1);
        let finished = std::mem::take(&mut self.subagents);
        self.earlier_subagents.extend(finished);
        let excess = self
            .earlier_subagents
            .len()
            .saturating_sub(EARLIER_SUBAGENTS);
        self.earlier_subagents.drain(..excess);
        self.segments.clear();
        self.tools.clear();
        self.plan = None;
        self.outcome = None;
        self.status = None;
        self.phase = None;
        self.started_at = None;
        self.finished_at = None;
        self.resend = false;
    }
    /// Admission opened this turn (`Phase { Preparing }`) but the host never
    /// started it: the activity line goes away without leaving a turn behind.
    pub fn abandon_admission(&mut self) {
        if self.running
            && self.segments.is_empty()
            && self.tools.is_empty()
            && self.phase == Some(TurnPhase::Preparing)
        {
            self.running = false;
            self.phase = None;
            self.started_at = None;
            self.render_revision = self.render_revision.wrapping_add(1);
        }
    }
    pub fn apply(&mut self, event: EngineEvent) {
        match event {
            EngineEvent::Ready { .. }
            | EngineEvent::TitleUpdated { .. }
            | EngineEvent::Steering { .. }
            | EngineEvent::ProviderCredentialRequested { .. } => return,
            EngineEvent::Started => {
                // Admission may already have opened this turn with `Preparing`.
                if !(self.running && self.segments.is_empty() && self.tools.is_empty()) {
                    self.begin();
                }
            }
            EngineEvent::Phase { phase } => {
                if !self.running {
                    if phase != TurnPhase::Preparing {
                        return;
                    }
                    self.begin();
                }
                self.phase = Some(phase);
                self.phase_since = now_ms();
            }
            EngineEvent::Delta { text } => self.push_text(&text),
            EngineEvent::Status { text } => self.status = Some(text),
            EngineEvent::ToolStarted {
                action_id,
                name,
                arguments,
                parent_id,
            } => {
                self.card(action_id, name, arguments, parent_id);
            }
            EngineEvent::ApprovalRequired {
                action_id,
                name,
                arguments,
                rule,
                reason,
                parent_id,
                ..
            } => {
                let card = self.card(action_id, name, arguments, parent_id);
                card.approval = true;
                if rule.is_some() {
                    card.rule = rule;
                }
                if reason.is_some() {
                    card.reason = reason;
                }
            }
            EngineEvent::ContextUsage { usage } => self.context = Some(usage),
            EngineEvent::ToolFinished {
                action_id,
                result,
                success,
                ..
            } => self.finish_tool(action_id, result, success),
            EngineEvent::Completed { text, usage } => {
                if !text.trim().is_empty() {
                    match self.segments.last_mut() {
                        Some(Segment::Text(tail)) => *tail = text.into(),
                        _ => self.segments.push(Segment::Text(text.into())),
                    }
                }
                self.usage = usage;
                self.finish(TurnOutcome::Completed);
            }
            EngineEvent::Cancelled => self.finish(TurnOutcome::Stopped),
            EngineEvent::Error {
                message,
                retryable,
                kind,
            } => self.finish(TurnOutcome::Failed(TurnFailure {
                message,
                kind,
                retryable,
            })),
        }
        self.render_revision = self.render_revision.wrapping_add(1);
    }
    fn begin(&mut self) {
        self.archive();
        let now = now_ms();
        self.running = true;
        self.started_at = Some(now);
        self.phase = Some(TurnPhase::Preparing);
        self.phase_since = now;
    }
    fn push_text(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        match self.segments.last_mut() {
            Some(Segment::Text(text)) => {
                let mut joined = String::with_capacity(text.len() + delta.len());
                joined.push_str(text);
                joined.push_str(delta);
                *text = joined.into();
            }
            _ => self.segments.push(Segment::Text(delta.to_owned().into())),
        }
    }
    /// The card for `id`, created and placed in the transcript on first sight.
    fn card(
        &mut self,
        id: Uuid,
        name: String,
        input: serde_json::Value,
        parent_id: Option<Uuid>,
    ) -> &mut ToolCard {
        if let Some(index) = self.tools.iter().position(|tool| tool.id == id) {
            return &mut self.tools[index];
        }
        let parent = match parent_id {
            Some(parent) => self
                .subagents
                .iter()
                .position(|agent| agent.id == parent)
                .or_else(|| self.promote_to_subagent(parent)),
            None => None,
        };
        if let Some(parent) = parent {
            self.subagents[parent].tools.push(id);
        } else if name == DELEGATE_TOOL {
            let task = input
                .get("task")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
            self.subagents.push(Subagent {
                id,
                task,
                tools: Vec::new(),
                summary: None,
                state: SubagentState::Running,
                started_at: now_ms(),
                finished_at: None,
            });
            self.segments.push(Segment::Subagent(id));
        } else {
            match self.segments.last_mut() {
                Some(Segment::Tools(ids)) => ids.push(id),
                _ => self.segments.push(Segment::Tools(vec![id])),
            }
        }
        let top_level_plan = name == PLAN_TOOL && parent_id.is_none();
        let index = self.tools.len();
        self.tools.push(ToolCard::new(id, name, input, parent_id));
        if top_level_plan {
            self.refresh_plan();
        }
        &mut self.tools[index]
    }
    /// A top-level call whose child agent reports its own calls (a forked
    /// skill) becomes a helper row in place of its tool row.
    fn promote_to_subagent(&mut self, id: Uuid) -> Option<usize> {
        let card = self
            .tools
            .iter()
            .find(|tool| tool.id == id && tool.parent_id.is_none())?;
        let (segment, slot) = self
            .segments
            .iter()
            .enumerate()
            .find_map(|(index, segment)| match segment {
                Segment::Tools(ids) => ids
                    .iter()
                    .position(|tool| *tool == id)
                    .map(|slot| (index, slot)),
                _ => None,
            })?;
        let task = if card.presentation.target.is_empty() {
            card.presentation.verb.clone()
        } else {
            card.presentation.target.clone()
        };
        let agent = Subagent {
            id,
            task,
            tools: Vec::new(),
            summary: None,
            state: match card.success {
                None => SubagentState::Running,
                Some(true) => SubagentState::Completed,
                Some(false) => SubagentState::Failed,
            },
            started_at: card.started_at,
            finished_at: card.finished_at,
        };
        self.subagents.push(agent);
        // Split the tool group around the promoted call so order is kept.
        let mut replacement = Vec::with_capacity(3);
        if let Segment::Tools(ids) = &mut self.segments[segment] {
            let after = ids.split_off(slot + 1);
            ids.pop();
            if !ids.is_empty() {
                replacement.push(Segment::Tools(std::mem::take(ids)));
            }
            replacement.push(Segment::Subagent(id));
            if !after.is_empty() {
                replacement.push(Segment::Tools(after));
            }
        }
        self.segments.splice(segment..=segment, replacement);
        Some(self.subagents.len() - 1)
    }
    fn finish_tool(&mut self, id: Uuid, result: String, success: bool) {
        let Some(card) = self.tools.iter_mut().find(|tool| tool.id == id) else {
            return;
        };
        card.decision_lost = card.approval && !success;
        card.approval = false;
        card.not_completed = false;
        card.result = Some(result);
        card.success = Some(success);
        card.finished_at = Some(now_ms());
        card.refresh_presentation();
        let summary = delegate_summary(card);
        let plan = card.name == PLAN_TOOL && card.parent_id.is_none();
        if let Some(agent) = self.subagents.iter_mut().find(|agent| agent.id == id) {
            agent.state = if success {
                SubagentState::Completed
            } else {
                SubagentState::Failed
            };
            agent.summary = summary;
            agent.finished_at = Some(now_ms());
        }
        if plan {
            self.refresh_plan();
        }
    }
    /// The newest top-level checklist that did not fail.
    fn refresh_plan(&mut self) {
        self.plan = self
            .tools
            .iter()
            .rev()
            .find(|tool| {
                tool.name == PLAN_TOOL && tool.parent_id.is_none() && tool.success != Some(false)
            })
            .map(|tool| tool.id);
    }
    fn finish(&mut self, outcome: TurnOutcome) {
        let now = now_ms();
        self.running = false;
        self.finished_at = Some(now);
        self.phase = None;
        self.outcome = Some(outcome);
        // A finished turn has no pending decisions; unfinished cards stop spinning.
        for tool in &mut self.tools {
            tool.approval = false;
            if tool.success.is_none() {
                tool.not_completed = true;
            }
        }
        for agent in &mut self.subagents {
            if agent.state == SubagentState::Running {
                agent.state = SubagentState::NotCompleted;
                agent.finished_at = Some(now);
            }
        }
    }
}

/// A finished helper's answer, or why it failed.
fn delegate_summary(card: &ToolCard) -> Option<SharedString> {
    if card.name != DELEGATE_TOOL {
        return None;
    }
    match (&card.presentation.body, &card.presentation.error) {
        (_, Some(error)) => Some(error.clone().into()),
        (tool_presentation::ToolBody::Markdown(text), None) => Some(text.clone().into()),
        _ => None,
    }
}

pub fn message(session_id: Uuid, role: &str, content: String) -> ChatMessage {
    ChatMessage {
        id: Uuid::new_v4(),
        session_id,
        role: role.into(),
        content,
        created_at: now_ms(),
        attachments: Vec::new(),
        status: MessageStatus::Complete,
    }
}

#[cfg(test)]
#[path = "state/tests.rs"]
mod tests;
