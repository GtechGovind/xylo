//! Per-engine headless constraints and externally implemented tools.

use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use xylo_core::ToolDefinition;

/// Executes a registered SDK tool after the broker has journaled and authorized it.
/// The implementation must honor cancellation and must not start work before called.
pub type CustomToolHandler =
    Arc<dyn Fn(Uuid, &str, &Value, CancellationToken) -> Result<Value> + Send + Sync>;

/// Observes a policy refusal without changing authorization.
pub type RefusalObserver = Arc<dyn Fn(&str) + Send + Sync>;

/// Run constraints applied before advertising and dispatching any tool.
#[derive(Clone, Default)]
pub struct RunOptions {
    /// Allows waiting for QuestionRequired and PlanRequired replies. Headless defaults decline.
    pub interactive: bool,
    pub allowed_tools: Option<Vec<String>>,
    pub disallowed_tools: Vec<String>,
    pub max_turns: Option<u32>,
    pub append_system_prompt: String,
    pub custom_tools: Vec<ToolDefinition>,
    pub custom_handler: Option<CustomToolHandler>,
    /// Observes policy refusals without changing the broker verdict.
    pub on_refusal: Option<RefusalObserver>,
}
