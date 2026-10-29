//! Correlated Memory operations. SQLite, embedding and exports never run on UI
//! or async event threads; accepted form saves are awaited through their commit.
use super::*;
use crate::{ExportSelection, FactCursor, FactDeleteKind, FactDeleteRequest, MemoryFormWrite};

pub(super) enum FactCommand {
    Versions(Uuid, String, Option<Uuid>, Option<FactCursor>),
    Preview(Uuid, String, FactDeleteKind),
    Delete(Uuid, FactDeleteRequest),
    Export(Uuid, String, ExportSelection, PathBuf),
    LoadForm(Uuid, String),
    SaveForm(Uuid, MemoryFormWrite),
}

impl FactCommand {
    pub(super) fn cancellable_id(&self) -> Option<Uuid> {
        match self {
            Self::Versions(id, ..)
            | Self::Preview(id, ..)
            | Self::Delete(id, ..)
            | Self::Export(id, ..) => Some(*id),
            Self::LoadForm(..) | Self::SaveForm(..) => None,
        }
    }
    pub(super) fn refused(&self, message: &str) -> HostEvent {
        let error = Err(message.to_owned());
        match self {
            Self::Versions(id, ..) => HostEvent::FactVersions {
                request_id: *id,
                result: error,
            },
            Self::Preview(id, ..) => HostEvent::FactDeletePreview {
                request_id: *id,
                result: Err(message.into()),
            },
            Self::Delete(id, ..) => HostEvent::FactDeleted {
                request_id: *id,
                result: Err(message.into()),
            },
            Self::Export(id, ..) => HostEvent::MemoryExport {
                request_id: *id,
                result: Err(message.into()),
            },
            Self::LoadForm(id, ..) => HostEvent::MemoryFormLoaded {
                request_id: *id,
                result: Err(message.into()),
            },
            Self::SaveForm(id, ..) => HostEvent::MemoryFormSaved {
                request_id: *id,
                result: Err(message.into()),
            },
        }
    }
}

impl HostClient {
    pub fn fact_versions(
        &self,
        id: Uuid,
        scope: String,
        anchor: Option<Uuid>,
        before: Option<FactCursor>,
    ) -> Result<()> {
        self.enqueue(HostCommand::Fact(FactCommand::Versions(
            id, scope, anchor, before,
        )))
    }
    pub fn fact_delete_preview(&self, id: Uuid, scope: String, kind: FactDeleteKind) -> Result<()> {
        self.enqueue(HostCommand::Fact(FactCommand::Preview(id, scope, kind)))
    }
    pub fn fact_delete(&self, id: Uuid, request: FactDeleteRequest) -> Result<()> {
        self.enqueue(HostCommand::Fact(FactCommand::Delete(id, request)))
    }
    pub fn fact_export(
        &self,
        id: Uuid,
        scope: String,
        selection: ExportSelection,
        destination: PathBuf,
    ) -> Result<()> {
        self.enqueue(HostCommand::Fact(FactCommand::Export(
            id,
            scope,
            selection,
            destination,
        )))
    }
    pub fn fact_cancel(&self, id: Uuid) -> Result<()> {
        self.cancel_capability(id)
    }
    pub fn memory_form_load(&self, id: Uuid, scope: String) -> Result<()> {
        self.enqueue(HostCommand::Fact(FactCommand::LoadForm(id, scope)))
    }
    pub fn memory_form_save(&self, id: Uuid, write: MemoryFormWrite) -> Result<()> {
        self.enqueue(HostCommand::Fact(FactCommand::SaveForm(id, write)))
    }
}

impl HostState {
    pub(super) async fn handle_fact(&self, command: FactCommand) -> Result<()> {
        let id = command.cancellable_id();
        let cancel = if let Some(id) = id {
            self.capability_requests
                .lock()
                .await
                .get(&id)
                .cloned()
                .context("Memory operation is no longer pending")?
        } else {
            CancellationToken::new()
        };
        let store = self.store.clone();
        let event = match command {
            FactCommand::Versions(request_id, scope, anchor, before) => {
                let result = tokio::task::spawn_blocking(move || {
                    store.fact_versions(&scope, anchor, before, 50, &cancel)
                })
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result)
                .map_err(|error| error.to_string());
                HostEvent::FactVersions { request_id, result }
            }
            FactCommand::Preview(request_id, scope, kind) => {
                let result = tokio::task::spawn_blocking(move || {
                    store.fact_delete_preview(&scope, kind, &cancel)
                })
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result)
                .map_err(|error| error.to_string());
                HostEvent::FactDeletePreview { request_id, result }
            }
            FactCommand::Delete(request_id, request) => {
                let memory = self.memory.clone();
                let result =
                    tokio::task::spawn_blocking(move || memory.delete_facts(request, &cancel))
                        .await
                        .map_err(anyhow::Error::from)
                        .and_then(|result| result)
                        .map_err(|error| error.to_string());
                HostEvent::FactDeleted { request_id, result }
            }
            FactCommand::Export(request_id, scope, selection, destination) => {
                let database = self.paths.database.clone();
                let result = tokio::task::spawn_blocking(move || {
                    xylo_services::export_fact_versions(
                        &database,
                        &scope,
                        &selection,
                        &destination,
                        &cancel,
                    )
                })
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result)
                .map_err(|error| error.to_string());
                HostEvent::MemoryExport { request_id, result }
            }
            FactCommand::LoadForm(request_id, scope) => {
                let result = tokio::task::spawn_blocking(move || store.memory_form(&scope))
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|result| result)
                    .map_err(|error| error.to_string());
                HostEvent::MemoryFormLoaded { request_id, result }
            }
            FactCommand::SaveForm(request_id, write) => {
                let result = tokio::task::spawn_blocking(move || store.save_memory_form(write))
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|result| result)
                    .map_err(|error| error.to_string());
                HostEvent::MemoryFormSaved { request_id, result }
            }
        };
        if let Some(id) = id {
            self.capability_requests.lock().await.remove(&id);
        }
        self.events.send(event).await?;
        Ok(())
    }
}
