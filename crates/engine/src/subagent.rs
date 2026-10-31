//! Delegated helper agents (`agent_delegate`).
//!
//! A child runs its own agent loop with the subagent prompt (the parent's
//! environment, permission mode and project instructions), a scoped child
//! broker (whose tool events carry the parent `agent_delegate` action as
//! `parent_id`) and no further delegation (depth 1). At most two children run
//! at once per turn, each bounded in time. The child's final answer, bounded
//! to 64 KB, becomes the `agent_delegate` tool result.
use crate::{
    agent_loop::{AgentLoop, MAX_MODEL_CALLS},
    broker_tools::BrokerTools,
    model::{Message, ModelProvider},
    provider_guard::ContextBudget,
    tools::{DelegateCall, PermissionCell, ToolBroker},
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, Weak},
    time::Duration,
};
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use xylo_core::EngineEvent;

/// Children running at once within one turn.
pub(crate) const MAX_CONCURRENT_CHILDREN: usize = 2;
/// Longest a child may work.
pub(crate) const CHILD_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Largest child answer returned to the parent model.
const MAX_RESULT_BYTES: usize = 64 * 1024;

/// Everything a child loop shares with its parent turn.
pub(crate) struct Delegation {
    pub provider: Arc<dyn ModelProvider>,
    pub budget: ContextBudget,
    pub summary_deadline: Duration,
    pub max_tokens: u32,
    pub parent: Weak<ToolBroker>,
    pub events: mpsc::Sender<EngineEvent>,
    pub slots: Arc<Semaphore>,
    /// The parent turn's cancellation.
    pub cancel: CancellationToken,
    pub timeout: Duration,
    pub runtime: tokio::runtime::Handle,
    /// The parent's project, for the child's environment and instructions.
    pub workspace: PathBuf,
    /// The live permission mode, shared with the parent.
    pub permissions: PermissionCell,
}

impl Delegation {
    /// The broker callback. It runs on the blocking thread executing the
    /// parent's `agent_delegate` call.
    pub(crate) fn handler(self: Arc<Self>) -> crate::tools::Delegate {
        Arc::new(move |call: &DelegateCall| self.runtime.block_on(self.run(call)))
    }

    async fn run(&self, call: &DelegateCall) -> Result<Value> {
        let task = call
            .arguments
            .get("task")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|task| !task.is_empty())
            .context("Subtask text is required")?;
        let _slot = tokio::select! {
            _ = self.cancel.cancelled() => bail!("Delegation cancelled"),
            permit = self.slots.acquire() => permit?,
        };
        let mut child = self
            .parent
            .upgrade()
            .context("Parent task has ended")?
            .scoped_child(call.parent_action_id)?;
        if let Some(allowed) = call.arguments.get("allowed_tools") {
            let allowed = allowed
                .as_array()
                .context("allowed_tools must be an array")?
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .context("allowed_tools entries must be strings")
                })
                .collect::<Result<Vec<_>>>()?;
            child.restrict_tools(&allowed)?;
        }
        let child = Arc::new(child);
        let cancel = child.cancellation_token();
        let mut request = task.to_owned();
        if let Some(context) = call
            .arguments
            .get("context")
            .and_then(Value::as_str)
            .filter(|context| !context.trim().is_empty())
        {
            request.push_str("\n\n");
            request.push_str(context);
        }
        let mode = self.permissions.get();
        let context =
            crate::prompts::PromptContext::detect(&self.workspace, mode, xylo_core::now_ms());
        let child_loop = AgentLoop {
            provider: self.provider.clone(),
            budget: self.budget.clone(),
            summary_deadline: self.summary_deadline,
            tools: Arc::new(BrokerTools::child(child.clone(), call.parent_key.clone())),
            system: crate::prompts::subagent(&context, task),
            max_tokens: self.max_tokens,
            max_model_calls: MAX_MODEL_CALLS,
            events: self.events.clone(),
            journal: None,
            permissions: Some((self.permissions.clone(), mode)),
            cancel: cancel.clone(),
        };
        let run = child_loop.run(vec![Message::user(request)]);
        tokio::pin!(run);
        let outcome = tokio::select! {
            outcome = &mut run => Ok(outcome),
            _ = tokio::time::sleep(self.timeout) => {
                // Stop the child and let its in-flight tools settle, so no
                // action is left running behind the timeout.
                cancel.cancel();
                let _ = run.await;
                Err(())
            }
        };
        let result = match outcome {
            Err(()) => {
                json!({"status":"timeout","result":format!(
                    "The helper agent did not finish within {} minutes.",
                    self.timeout.as_secs().div_ceil(60)
                )})
            }
            Ok(Ok(outcome)) => {
                json!({"status":"completed","result":bounded(&outcome.text)})
            }
            Ok(Err(failure)) if failure.cancelled() && self.cancel.is_cancelled() => {
                child.shutdown().await?;
                bail!("Delegation cancelled");
            }
            Ok(Err(failure)) => {
                json!({"status":"error","result":format!("{:#}", failure.error)})
            }
        };
        child.shutdown().await?;
        Ok(result)
    }
}

/// `text` cut at a character boundary to [`MAX_RESULT_BYTES`].
fn bounded(text: &str) -> String {
    if text.len() <= MAX_RESULT_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_RESULT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[answer truncated at 64 KB]", &text[..end])
}

#[cfg(test)]
mod tests;
