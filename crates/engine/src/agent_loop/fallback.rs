//! Application-owned fallback state belongs to the conversation_state's current turn.
use super::*;

impl AgentLoop {
    /// Restores selection from trusted checkpoint metadata, never prompt content.
    pub(super) async fn restored_fallback(&self) -> anyhow::Result<Option<Arc<dyn ModelProvider>>> {
        let Some(conversation_state) = &self.conversation_state else {
            return Ok(None);
        };
        let Some((model, provider)) = self.provider.fallback() else {
            return Ok(None);
        };
        let (store, session, turn) = (
            conversation_state.store.clone(),
            conversation_state.session,
            conversation_state.turn,
        );
        let selected =
            tokio::task::spawn_blocking(move || store.checkpoint_fallback(session, turn))
                .await
                .context("Fallback restoration task stopped")??;
        Ok((selected.as_deref() == Some(model.as_str())).then_some(provider))
    }

    /// Commits selection before the first alternative request can be dispatched.
    pub(super) async fn persist_fallback(&self, model: &str) -> anyhow::Result<()> {
        let Some(conversation_state) = &self.conversation_state else {
            return Ok(());
        };
        let (store, session, turn, model) = (
            conversation_state.store.clone(),
            conversation_state.session,
            conversation_state.turn,
            model.to_owned(),
        );
        tokio::task::spawn_blocking(move || store.select_checkpoint_fallback(session, turn, &model))
            .await
            .context("Fallback selection task stopped")?
    }
}
