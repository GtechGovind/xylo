//! Pure view-state reducer: engine events cannot leak between selected chats.
use uuid::Uuid;
use workbench_core::{ChatMessage, EngineEvent, Usage, now_ms};
#[derive(Clone, Debug)]
pub struct ToolCard {
    pub id: Uuid,
    pub name: String,
    /// What the action does, as presented by the engine (for changes, a diff).
    pub input: serde_json::Value,
    pub result: Option<String>,
    pub approval: bool,
    /// What "always allow" would cover, when the action can be remembered.
    pub rule: Option<String>,
    pub success: Option<bool>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}
impl ToolCard {
    /// The parsed result envelope, when the result is structured.
    pub fn output(&self) -> Option<serde_json::Value> {
        serde_json::from_str(self.result.as_deref()?).ok()
    }
}
#[derive(Default)]
pub struct TurnState {
    pub running: bool,
    pub text: String,
    pub tools: Vec<ToolCard>,
    pub status: String,
    pub error: Option<String>,
    pub usage: Usage,
    pub started_at: Option<i64>,
    pub first_token_ms: Option<u64>,
}
impl TurnState {
    /// Actions currently waiting for the user's decision.
    pub fn pending_approvals(&self) -> impl Iterator<Item = &ToolCard> {
        self.tools.iter().filter(|tool| tool.approval)
    }
    pub fn apply(&mut self, event: EngineEvent) {
        match event {
            EngineEvent::Started => {
                self.running = true;
                self.started_at = Some(now_ms());
                self.first_token_ms = None;
                self.text.clear();
                self.tools.clear();
                self.error = None;
                self.status = "Working…".into();
            }
            EngineEvent::Delta { text } => {
                if self.first_token_ms.is_none() {
                    self.first_token_ms = self.started_at.map(|t| (now_ms() - t).max(0) as u64);
                }
                self.text.push_str(&text);
            }
            EngineEvent::Status { text } => self.status = text,
            EngineEvent::ToolStarted {
                action_id,
                name,
                arguments,
            } => self.card(action_id, name, arguments, false, None),
            EngineEvent::ApprovalRequired {
                action_id,
                name,
                arguments,
                rule,
            } => self.card(action_id, name, arguments, true, rule),
            EngineEvent::ToolFinished {
                action_id,
                result,
                success,
                ..
            } => {
                if let Some(t) = self.tools.iter_mut().find(|t| t.id == action_id) {
                    t.approval = false;
                    t.result = Some(result);
                    t.success = Some(success);
                    t.finished_at = Some(now_ms());
                }
            }
            EngineEvent::Completed { text, usage } => {
                self.text = text;
                self.usage = usage;
                self.running = false;
                self.status = "Completed".into();
                self.settle_tools();
            }
            EngineEvent::Cancelled => {
                self.running = false;
                self.status = "Cancelled".into();
                self.settle_tools();
            }
            EngineEvent::Error { message, .. } => {
                self.running = false;
                self.status = message.clone();
                self.error = Some(message);
                self.settle_tools();
            }
            EngineEvent::Ready { .. } | EngineEvent::Steering { .. } => {}
        }
    }
    fn card(
        &mut self,
        id: Uuid,
        name: String,
        input: serde_json::Value,
        approval: bool,
        rule: Option<String>,
    ) {
        if let Some(card) = self.tools.iter_mut().find(|t| t.id == id) {
            card.approval = approval;
            if rule.is_some() {
                card.rule = rule;
            }
        } else {
            self.tools.push(ToolCard {
                id,
                name,
                input,
                result: None,
                approval,
                rule,
                success: None,
                started_at: now_ms(),
                finished_at: None,
            });
        }
    }
    /// A finished turn has no pending decisions; unfinished cards stop spinning.
    fn settle_tools(&mut self) {
        for tool in &mut self.tools {
            tool.approval = false;
        }
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
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn approval_and_finish_share_one_tool_card() {
        let mut state = TurnState::default();
        let id = Uuid::new_v4();
        state.apply(EngineEvent::ToolStarted {
            action_id: id,
            name: "write_file".into(),
            arguments: serde_json::json!({"path":"a.rs"}),
        });
        state.apply(EngineEvent::ApprovalRequired {
            action_id: id,
            name: "write_file".into(),
            arguments: serde_json::json!({}),
            rule: None,
        });
        assert_eq!(state.tools.len(), 1);
        assert!(state.tools[0].approval);
        state.apply(EngineEvent::ToolFinished {
            action_id: id,
            name: "write_file".into(),
            result: "saved".into(),
            success: true,
        });
        assert!(!state.tools[0].approval);
        assert_eq!(state.tools[0].success, Some(true));
    }
    #[test]
    fn cancellation_preserves_partial_output() {
        let mut state = TurnState::default();
        state.apply(EngineEvent::Started);
        state.apply(EngineEvent::Delta {
            text: "partial".into(),
        });
        state.apply(EngineEvent::Cancelled);
        assert_eq!(state.text, "partial");
        assert!(!state.running);
    }
}
