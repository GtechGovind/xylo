//! Pure view-state reducer: engine events cannot leak between selected chats.
use uuid::Uuid;
use workbench_core::{ChatMessage, EngineEvent, Usage, now_ms};
#[derive(Clone, Debug)]
pub struct ToolCard {
    pub id: Uuid,
    pub name: String,
    pub arguments: String,
    pub result: Option<String>,
    pub approval: bool,
    pub success: Option<bool>,
}
#[derive(Default)]
pub struct TurnState {
    pub running: bool,
    pub text: String,
    pub tools: Vec<ToolCard>,
    pub status: String,
    pub usage: Usage,
    pub started_at: Option<i64>,
    pub first_token_ms: Option<u64>,
}
impl TurnState {
    pub fn apply(&mut self, event: EngineEvent) {
        let is_approval = matches!(&event, EngineEvent::ApprovalRequired { .. });
        match event {
            EngineEvent::Started => {
                self.running = true;
                self.started_at = Some(now_ms());
                self.first_token_ms = None;
                self.text.clear();
                self.tools.clear();
                self.status = "Working".into();
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
            }
            | EngineEvent::ApprovalRequired {
                action_id,
                name,
                arguments,
            } => {
                let approval = is_approval;
                if let Some(card) = self.tools.iter_mut().find(|t| t.id == action_id) {
                    card.approval = approval;
                } else {
                    self.tools.push(ToolCard {
                        id: action_id,
                        name,
                        arguments: serde_json::to_string_pretty(&arguments).unwrap_or_default(),
                        result: None,
                        approval,
                        success: None,
                    });
                }
            }
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
                }
            }
            EngineEvent::Completed { text, usage } => {
                self.text = text;
                self.usage = usage;
                self.running = false;
                self.status = "Completed".into();
            }
            EngineEvent::Cancelled => {
                self.running = false;
                self.status = "Cancelled".into();
            }
            EngineEvent::Error { message, .. } => {
                self.running = false;
                self.status = message;
            }
            EngineEvent::Ready { .. } | EngineEvent::Steering { .. } => {}
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
