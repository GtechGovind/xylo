//! Durable conversation export requested by a native save dialog.
use super::*;
impl HostState {
    pub(super) async fn export_conversation(&self, session: Uuid, path: PathBuf) -> Result<()> {
        let store = self.store.clone();
        let destination = path.clone();
        let result = tokio::task::spawn_blocking(move || {
            crate::conversation_export::export(&store, session, &destination)
        })
        .await?;
        let event = match result {
            Ok(()) => HostEvent::Saved(path),
            Err(error) => HostEvent::SaveFailed {
                path,
                message: format!("{error:#}"),
            },
        };
        self.events.send(event).await?;
        Ok(())
    }
}
