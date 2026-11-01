//! Public headless protocol. One UTF-8 JSON object per line, bounded by
//! [`crate::MAX_PROTOCOL_BYTES`]. Version changes require an explicit client update.

use crate::{EngineEvent, ToolDefinition};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// The stable CLI and SDK wire schema version.
pub const VERSION: u16 = 1;

/// An input frame. No private worker or credential commands are exposed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Input {
    pub version: u16,
    pub payload: Command,
}

impl Input {
    /// Wraps a public command in the current schema.
    pub fn new(payload: Command) -> Self {
        Self {
            version: VERSION,
            payload,
        }
    }
    /// Rejects incompatible peers before doing any work.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == VERSION,
            "Unsupported CLI protocol version {} (expected {VERSION})",
            self.version
        );
        Ok(())
    }
}

/// Commands supported by a persistent headless session.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Run {
        prompt: String,
    },
    Approve {
        action_id: Uuid,
        allow: bool,
    },
    RegisterTool {
        definition: ToolDefinition,
    },
    ToolResult {
        action_id: Uuid,
        result: std::result::Result<Value, String>,
    },
    Cancel,
    Shutdown,
}

/// One engine event, with durable conversation and turn identity.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub version: u16,
    pub session_id: Uuid,
    pub turn_id: Uuid,
    pub sequence: u64,
    pub payload: EngineEvent,
    /// Present on terminal events: 0 success, 1 error or cancellation, 2 refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

impl Event {
    /// Rejects incompatible peers.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == VERSION,
            "Unsupported CLI protocol version {} (expected {VERSION})",
            self.version
        );
        if self.is_terminal() {
            ensure!(
                self.exit_code.is_some_and(|code| (0..=2).contains(&code)),
                "Terminal CLI event requires a valid exit code"
            );
        } else {
            ensure!(
                self.exit_code.is_none(),
                "Only terminal CLI events may carry an exit code"
            );
        }
        Ok(())
    }
    /// Whether this event settles the current run.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.payload,
            EngineEvent::Completed { .. } | EngineEvent::Cancelled | EngineEvent::Error { .. }
        )
    }
}

#[cfg(test)]
#[path = "stdio/tests.rs"]
mod tests;
