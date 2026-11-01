//! Typed, bounded conversation interactions shared by UI and worker clients.
use crate::PermissionMode;
use serde::{Deserialize, Serialize};

/// One user-visible choice.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

/// A question with an automatic additional free-text Other choice.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserQuestion {
    pub question: String,
    pub header: String,
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub multi_select: bool,
}

/// Selected option indices and optional free text for one question.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionAnswer {
    pub selected: Vec<usize>,
    pub other: Option<String>,
}

/// A reply to a question or a proposed plan. Decline also settles disconnected clients.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InteractionAnswer {
    Questions { answers: Vec<QuestionAnswer> },
    ApprovePlan { mode: PermissionMode },
    KeepPlanning { feedback: String },
    Reject,
}
