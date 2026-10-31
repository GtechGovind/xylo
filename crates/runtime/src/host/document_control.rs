//! Application-owned document edit recovery. File conversion stays in the
//! supervised helper; these commands only access the dedicated DB executor.
use super::*;
use crate::{
    DocumentDraftCursor, DocumentDraftKey, DocumentDraftRelinkRequest, DocumentDraftWrite,
};

pub(super) enum DocumentCommand {
    Load(Uuid, DocumentDraftKey),
    Save(Uuid, Box<DocumentDraftWrite>),
    List(Uuid, PathBuf, Option<DocumentDraftCursor>, usize),
    Relink(Uuid, Box<DocumentDraftRelinkRequest>),
    RelinkStatus(Uuid, DocumentDraftKey, Uuid),
}
impl DocumentCommand {
    pub(super) fn cancellable_id(&self) -> Option<Uuid> {
        match self {
            Self::Relink(id, _) => Some(*id),
            _ => None,
        }
    }
    pub(super) fn refused(&self, message: &str) -> HostEvent {
        match self {
            Self::Relink(id, _) => HostEvent::DocumentDraftRelinked {
                request_id: *id,
                result: Box::new(Err(message.into())),
            },
            Self::RelinkStatus(id, ..) => HostEvent::DocumentDraftRelinkStatus {
                request_id: *id,
                result: Box::new(Err(message.into())),
            },
            Self::Load(id, _) => HostEvent::DocumentDraftLoaded {
                request_id: *id,
                result: Err(message.into()),
            },
            Self::Save(id, _) => HostEvent::DocumentDraftSaved {
                request_id: *id,
                result: Err(message.into()),
            },
            Self::List(id, ..) => HostEvent::DocumentDrafts {
                request_id: *id,
                result: Err(message.into()),
            },
        }
    }
}
impl HostClient {
    pub fn document_draft_relink(
        &self,
        id: Uuid,
        request: DocumentDraftRelinkRequest,
    ) -> Result<()> {
        self.enqueue(HostCommand::Document(DocumentCommand::Relink(
            id,
            Box::new(request),
        )))
    }
    pub fn document_draft_relink_status(
        &self,
        id: Uuid,
        key: DocumentDraftKey,
        operation_id: Uuid,
    ) -> Result<()> {
        self.enqueue(HostCommand::Document(DocumentCommand::RelinkStatus(
            id,
            key,
            operation_id,
        )))
    }
    pub fn document_draft_load(&self, id: Uuid, key: DocumentDraftKey) -> Result<()> {
        self.enqueue(HostCommand::Document(DocumentCommand::Load(id, key)))
    }
    pub fn document_draft_save(&self, id: Uuid, write: DocumentDraftWrite) -> Result<()> {
        self.enqueue(HostCommand::Document(DocumentCommand::Save(
            id,
            Box::new(write),
        )))
    }
    pub fn document_draft_list(
        &self,
        id: Uuid,
        workspace: PathBuf,
        before: Option<DocumentDraftCursor>,
        limit: usize,
    ) -> Result<()> {
        self.enqueue(HostCommand::Document(DocumentCommand::List(
            id, workspace, before, limit,
        )))
    }
}
impl HostState {
    async fn checked_document_relink(
        &self,
        request: DocumentDraftRelinkRequest,
        cancel: CancellationToken,
    ) -> Result<crate::DocumentDraftRelinkReceipt> {
        let store = self.store.clone();
        let key = request.key.clone();
        let operation = request.operation_id;
        let existing =
            tokio::task::spawn_blocking(move || store.document_draft_relink_status(key, operation))
                .await??;
        if let Some(saved) = existing {
            ensure!(
                saved.request == request,
                "This location request was reused for different content"
            );
            return Ok(saved);
        }
        let data = self.paths.data.clone();
        let data = tokio::task::spawn_blocking(move || data.canonicalize()).await??;
        let lease = xylo_resources::shared::acquire(
            data.join("native-management").join("admission"),
            TaskDemand {
                memory_bytes: 16 * 1024 * 1024,
                cpu_share: 0.25,
            },
            cancel.clone(),
        )
        .await?;
        let store = self.store.clone();
        let artifacts = self.paths.artifacts.clone();
        tokio::task::spawn_blocking(move || {
            // Move the OS reservation into the actual read/transaction task. It
            // survives cancellation or loss of the enclosing async waiter.
            let _lease = lease;
            ensure!(
                request.key.source.starts_with(&request.key.workspace)
                    || request.key.source.starts_with(&artifacts),
                "Original is outside its project and saved files"
            );
            let management_root = data.join("native-management");
            crate::verify_document_relink_source(&request, &artifacts, &management_root, &cancel)?;
            store.relink_document_draft_cancellable(request, &cancel)
        })
        .await?
    }
    pub(super) async fn handle_document(&self, command: DocumentCommand) -> Result<()> {
        let control_id = command.cancellable_id();
        let cancel = if let Some(id) = control_id {
            self.capability_requests
                .lock()
                .await
                .get(&id)
                .cloned()
                .context("Original location request is no longer pending")?
        } else {
            CancellationToken::new()
        };
        let store = self.store.clone();
        let event = match command {
            DocumentCommand::Relink(request_id, request) => {
                let result = self
                    .checked_document_relink(*request, cancel)
                    .await
                    .map_err(|error| error.to_string());
                HostEvent::DocumentDraftRelinked {
                    request_id,
                    result: Box::new(result),
                }
            }
            DocumentCommand::RelinkStatus(request_id, key, operation_id) => {
                let result = tokio::task::spawn_blocking(move || {
                    store.document_draft_relink_status(key, operation_id)
                })
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result)
                .map_err(|error| error.to_string());
                HostEvent::DocumentDraftRelinkStatus {
                    request_id,
                    result: Box::new(result),
                }
            }
            DocumentCommand::Load(request_id, key) => {
                let result = tokio::task::spawn_blocking(move || store.document_draft(key))
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|result| result)
                    .map_err(|error| error.to_string());
                HostEvent::DocumentDraftLoaded { request_id, result }
            }
            DocumentCommand::Save(request_id, write) => {
                let artifacts = self.paths.artifacts.clone();
                let result = tokio::task::spawn_blocking(move || {
                    ensure!(
                        write.key.source.starts_with(&write.key.workspace)
                            || write.key.source.starts_with(&artifacts),
                        "Document is outside its project and the application artifacts folder"
                    );
                    store.save_document_draft(*write)
                })
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result)
                .map_err(|error| error.to_string());
                HostEvent::DocumentDraftSaved { request_id, result }
            }
            DocumentCommand::List(request_id, workspace, before, limit) => {
                let result = tokio::task::spawn_blocking(move || {
                    store.document_drafts(workspace, before, limit)
                })
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result)
                .map_err(|error| error.to_string());
                HostEvent::DocumentDrafts { request_id, result }
            }
        };
        if let Some(id) = control_id {
            self.capability_requests.lock().await.remove(&id);
        }
        self.events.send(event).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
