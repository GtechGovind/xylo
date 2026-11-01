//! Lifecycle events run off async workers and inject context at role-safe boundaries.
use super::*;
use serde_json::Value;
use xylo_services::{hooks::HookOutput, settings_files::PermissionDecision};

impl AgentLoop {
    pub(super) async fn hook(&self, event: &str, input: Value) -> anyhow::Result<HookOutput> {
        let tools = self.tools.clone();
        let event = event.to_owned();
        tokio::task::spawn_blocking(move || tools.lifecycle(&event, input))
            .await
            .context("Lifecycle hook task stopped")?
    }
    pub(super) async fn hook_context(
        &self,
        messages: &mut Vec<Message>,
        output: &HookOutput,
    ) -> anyhow::Result<()> {
        if let Some(reason) = &output.stop_reason {
            anyhow::bail!("{reason}");
        }
        anyhow::ensure!(
            output.decision != Some(PermissionDecision::Deny),
            "Configured lifecycle hook denied this operation"
        );
        for text in &output.context {
            messages.push(application_note(text));
            self.show_note(NoteKind::Hook, text.clone()).await?;
        }
        Ok(())
    }
}
