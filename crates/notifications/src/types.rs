use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Preferences belong to the app, separately from provider/tool configuration.
/// System permission can only be requested through the explicit UI action.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Preferences {
    pub enabled: bool,
    pub completion: bool,
    pub approval: bool,
    pub failure: bool,
    pub schedule: bool,
    pub background_only: bool,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            enabled: false,
            completion: true,
            approval: true,
            failure: true,
            schedule: true,
            background_only: true,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    Unknown,
    NotDetermined,
    Denied,
    Allowed,
    Quiet,
    NotRequired,
    Unsupported,
    Unavailable,
}
impl Permission {
    pub fn allows_delivery(self) -> bool {
        matches!(self, Self::Allowed | Self::Quiet | Self::NotRequired)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Started,
    Approval,
    Completed,
    Failed,
    Cancelled,
    ScheduleCompleted,
    ScheduleFailed,
}
impl Kind {
    pub(crate) fn terminal(self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::Failed
                | Self::Cancelled
                | Self::ScheduleCompleted
                | Self::ScheduleFailed
        )
    }
    pub(crate) fn enabled(self, preferences: &Preferences) -> bool {
        match self {
            Self::Completed => preferences.completion,
            Self::Approval => preferences.approval,
            Self::Failed => preferences.failure,
            Self::ScheduleCompleted | Self::ScheduleFailed => preferences.schedule,
            _ => false,
        }
    }
    pub(crate) fn text(self) -> Option<(&'static str, &'static str)> {
        Some(match self {
            Self::Completed => ("Task complete", "Open Xylo to view the result."),
            Self::Failed => ("Task needs attention", "Open Xylo to review the task."),
            Self::Approval => ("Approval needed", "Open Xylo to review a requested action."),
            Self::ScheduleCompleted => ("Scheduled task complete", "Open Xylo to view the result."),
            Self::ScheduleFailed => (
                "Scheduled task needs attention",
                "Open Xylo to review the task.",
            ),
            _ => return None,
        })
    }
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Approval => "approval",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::ScheduleCompleted => "schedule_completed",
            Self::ScheduleFailed => "schedule_failed",
        }
    }
}
/// Only trusted host lifecycle producers construct these. There is deliberately
/// no title/body/prompt/path field that an agent or tool could put on a lockscreen.
#[derive(Clone, Debug)]
pub struct NoticeEvent {
    pub session_id: Uuid,
    pub turn_id: Uuid,
    /// Stable tool-intent/event ID, not a fresh UUID for each replay.
    pub event_id: Uuid,
    pub kind: Kind,
    pub occurred_at_ms: i64,
    pub background: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Activation {
    pub session_id: Uuid,
    pub turn_id: Uuid,
}
#[derive(Clone, Debug, Serialize)]
pub struct Capabilities {
    pub request_permission: bool,
    pub live_activation: bool,
    pub cold_activation: bool,
    pub retraction: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub sequence: u64,
    pub loaded: bool,
    pub preferences: Preferences,
    pub permission: Permission,
    pub capabilities: Capabilities,
    pub message: Option<String>,
}
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Submitted,
    Duplicate,
    Suppressed,
    Cancelled,
    Stale,
}
#[derive(Clone, Debug)]
pub struct AppIdentity {
    pub id: String,
    pub name: String,
}
/// A deliberate UI command, never produced by observe() or an agent tool.
#[derive(Clone, Copy, Debug)]
pub enum UserAction {
    RequestPermission,
    SendTest,
}

#[derive(Clone, Debug)]
pub struct NativeNotice {
    pub tag: String,
    pub title: &'static str,
    pub body: &'static str,
}
