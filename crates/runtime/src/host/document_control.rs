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
        let lease = crate::resources::shared::acquire(
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
mod tests {
    use super::*;
    use crate::{DocumentDraftSave, DocumentDraftState};
    use xylo_core::{DocumentEdit, DocumentKind, DocumentSource};
    #[tokio::test]
    async fn document_draft_host_routes_exact_receipts_and_rejects_foreign_source() {
        let (_dir, host, events) = super::super::tests::state();
        let root = host.paths.data.canonicalize().unwrap();
        let key = DocumentDraftKey {
            workspace: root.clone(),
            source: root.join("note.txt"),
        };
        let write = DocumentDraftWrite {
            key: key.clone(),
            expected_revision: 0,
            mutation_id: Uuid::new_v4(),
            state: Some(DocumentDraftState {
                source: DocumentSource {
                    path: key.source.clone(),
                    revision: "a".repeat(64),
                    bytes: 0,
                    kind: DocumentKind::Text,
                },
                edits: vec![DocumentEdit::Text {
                    text: "restored".into(),
                }],
                export_name: "copy".into(),
                sheet: None,
                selected: None,
                pending_copy: None,
                pending_relink: None,
            }),
        };
        let id = Uuid::new_v4();
        host.handle(HostCommand::Document(DocumentCommand::Save(
            id,
            Box::new(write.clone()),
        )))
        .await
        .unwrap();
        assert!(
            matches!(events.recv().await.unwrap(),HostEvent::DocumentDraftSaved{request_id,result:Ok(DocumentDraftSave::Saved(snapshot))} if request_id==id && snapshot.revision==1)
        );
        let load = Uuid::new_v4();
        host.handle(HostCommand::Document(DocumentCommand::Load(
            load,
            key.clone(),
        )))
        .await
        .unwrap();
        assert!(
            matches!(events.recv().await.unwrap(),HostEvent::DocumentDraftLoaded{request_id,result:Ok(snapshot)} if request_id==load && snapshot.key==key && snapshot.state.is_some())
        );
        let mut foreign = write;
        foreign.key.source = root.parent().unwrap().join("foreign.txt");
        foreign.state.as_mut().unwrap().source.path = foreign.key.source.clone();
        host.handle(HostCommand::Document(DocumentCommand::Save(
            id,
            Box::new(foreign),
        )))
        .await
        .unwrap();
        assert!(matches!(
            events.recv().await.unwrap(),
            HostEvent::DocumentDraftSaved { result: Err(_), .. }
        ));
        assert!(
            host.helpers.lock().await.is_empty(),
            "draft persistence must never start a conversion helper"
        );
    }
    #[tokio::test]
    async fn installing_refuses_document_draft_with_correlated_typed_event() {
        let (_dir, host, events) = super::super::tests::state();
        host.installing.store(true, Ordering::Release);
        let id = Uuid::new_v4();
        let key = DocumentDraftKey {
            workspace: host.paths.data.clone(),
            source: host.paths.data.join("draft.txt"),
        };
        assert!(
            !host
                .prepare_command(&HostCommand::Document(DocumentCommand::Load(id, key)))
                .await
                .unwrap()
        );
        assert!(
            matches!(events.recv().await.unwrap(),HostEvent::DocumentDraftLoaded{request_id,result:Err(_)} if request_id==id)
        );
    }
    #[tokio::test]
    async fn document_relink_host_verifies_file_and_replays_receipt_without_it() {
        let (_dir, host, events) = super::super::tests::state();
        let workspace = host.paths.data.canonicalize().unwrap();
        let original = workspace.join("original.txt");
        let moved = workspace.join("moved.txt");
        std::fs::write(&moved, b"abc").unwrap();
        let key = DocumentDraftKey {
            workspace: workspace.clone(),
            source: original.clone(),
        };
        let request = crate::DocumentDraftRelinkRequest {
            operation_id: Uuid::new_v4(),
            key: key.clone(),
            expected_revision: 1,
            source: DocumentSource {
                path: moved.clone(),
                revision: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
                bytes: 3,
                kind: DocumentKind::Text,
            },
        };
        let state = DocumentDraftState {
            source: DocumentSource {
                path: original,
                ..request.source.clone()
            },
            edits: vec![DocumentEdit::Text {
                text: "Retained full replacement".into(),
            }],
            export_name: "Recovered notes".into(),
            sheet: None,
            selected: None,
            pending_copy: None,
            pending_relink: Some(Box::new(request.clone())),
        };
        host.store
            .save_document_draft(DocumentDraftWrite {
                key: key.clone(),
                expected_revision: 0,
                mutation_id: Uuid::new_v4(),
                state: Some(state),
            })
            .unwrap();
        let cancelled_id = Uuid::new_v4();
        let command = HostCommand::Document(DocumentCommand::Relink(
            cancelled_id,
            Box::new(request.clone()),
        ));
        assert!(host.prepare_command(&command).await.unwrap());
        host.capability_requests.lock().await[&cancelled_id].cancel();
        host.handle(command).await.unwrap();
        assert!(
            matches!(events.recv().await.unwrap(),HostEvent::DocumentDraftRelinked{request_id,result} if request_id==cancelled_id && result.is_err())
        );
        assert!(
            host.store
                .document_draft(key.clone())
                .unwrap()
                .state
                .is_some()
        );
        let id = Uuid::new_v4();
        let command = HostCommand::Document(DocumentCommand::Relink(id, Box::new(request.clone())));
        assert!(host.prepare_command(&command).await.unwrap());
        host.handle(command).await.unwrap();
        let receipt = match events.recv().await.unwrap() {
            HostEvent::DocumentDraftRelinked { request_id, result } => {
                assert_eq!(id, request_id);
                (*result).unwrap()
            }
            _ => panic!("missing correlated location receipt"),
        };
        assert!(
            host.store
                .document_draft(key.clone())
                .unwrap()
                .state
                .is_none()
        );
        assert_eq!(std::fs::read(&moved).unwrap(), b"abc");
        std::fs::remove_file(&moved).unwrap();
        let replay = Uuid::new_v4();
        let command =
            HostCommand::Document(DocumentCommand::Relink(replay, Box::new(request.clone())));
        assert!(host.prepare_command(&command).await.unwrap());
        host.handle(command).await.unwrap();
        match events.recv().await.unwrap() {
            HostEvent::DocumentDraftRelinked { request_id, result } => {
                assert_eq!(request_id, replay);
                assert_eq!((*result).unwrap(), receipt);
            }
            _ => panic!("missing replay receipt"),
        }
        let mut changed = request;
        changed.source.path = workspace.join("changed.txt");
        let command =
            HostCommand::Document(DocumentCommand::Relink(Uuid::new_v4(), Box::new(changed)));
        assert!(host.prepare_command(&command).await.unwrap());
        host.handle(command).await.unwrap();
        assert!(
            matches!(events.recv().await.unwrap(),HostEvent::DocumentDraftRelinked{result,..} if result.is_err())
        );
        assert!(host.helpers.lock().await.is_empty());
    }
}
