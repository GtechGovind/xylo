//! Human review controls. There is deliberately no agent-facing resolver tool.
use super::*;
use crate::{
    ActionResolutionRequest, ActionReviewCursor, ActionReviewFormWrite, ActionReviewScope,
};
pub(super) enum ActionCommand {
    List(Uuid, ActionReviewScope, Option<ActionReviewCursor>, usize),
    Detail(Uuid, ActionReviewScope, Uuid),
    Resolve(Uuid, Box<ActionResolutionRequest>),
    Receipt(Uuid, ActionReviewScope, Uuid),
    LoadForm(Uuid, ActionReviewScope),
    SaveForm(Uuid, Box<ActionReviewFormWrite>),
}
impl ActionCommand {
    pub(super) fn cancellable_id(&self) -> Option<Uuid> {
        match self {
            Self::List(id, ..)
            | Self::Detail(id, ..)
            | Self::Resolve(id, ..)
            | Self::Receipt(id, ..) => Some(*id),
            _ => None,
        }
    }
    pub(super) fn refused(&self, message: &str) -> HostEvent {
        match self {
            Self::List(id, ..) => HostEvent::ActionReviews {
                request_id: *id,
                result: Err(message.into()),
            },
            Self::Detail(id, ..) => HostEvent::ActionReview {
                request_id: *id,
                result: Box::new(Err(message.into())),
            },
            Self::Resolve(id, ..) => HostEvent::ActionResolved {
                request_id: *id,
                result: Err(message.into()),
            },
            Self::Receipt(id, ..) => HostEvent::ActionResolution {
                request_id: *id,
                result: Err(message.into()),
            },
            Self::LoadForm(id, ..) => HostEvent::ActionReviewForm {
                request_id: *id,
                result: Err(message.into()),
            },
            Self::SaveForm(id, ..) => HostEvent::ActionReviewFormSaved {
                request_id: *id,
                result: Err(message.into()),
            },
        }
    }
}
impl HostClient {
    pub fn action_reviews(
        &self,
        id: Uuid,
        scope: ActionReviewScope,
        cursor: Option<ActionReviewCursor>,
        limit: usize,
    ) -> Result<()> {
        self.enqueue(HostCommand::Action(ActionCommand::List(
            id, scope, cursor, limit,
        )))
    }
    pub fn action_review(&self, id: Uuid, scope: ActionReviewScope, action: Uuid) -> Result<()> {
        self.enqueue(HostCommand::Action(ActionCommand::Detail(
            id, scope, action,
        )))
    }
    pub fn resolve_action(&self, id: Uuid, request: ActionResolutionRequest) -> Result<()> {
        self.enqueue(HostCommand::Action(ActionCommand::Resolve(
            id,
            Box::new(request),
        )))
    }
    pub fn action_resolution(
        &self,
        id: Uuid,
        scope: ActionReviewScope,
        operation: Uuid,
    ) -> Result<()> {
        self.enqueue(HostCommand::Action(ActionCommand::Receipt(
            id, scope, operation,
        )))
    }
    pub fn action_review_form(&self, id: Uuid, scope: ActionReviewScope) -> Result<()> {
        self.enqueue(HostCommand::Action(ActionCommand::LoadForm(id, scope)))
    }
    pub fn save_action_review_form(&self, id: Uuid, write: ActionReviewFormWrite) -> Result<()> {
        self.enqueue(HostCommand::Action(ActionCommand::SaveForm(
            id,
            Box::new(write),
        )))
    }
}
impl HostState {
    pub(super) async fn handle_action(&self, command: ActionCommand) -> Result<()> {
        let id = command.cancellable_id();
        let cancel = if let Some(id) = id {
            self.capability_requests
                .lock()
                .await
                .get(&id)
                .cloned()
                .context("Review request is no longer pending")?
        } else {
            CancellationToken::new()
        };
        let store = self.store.clone();
        let event = match command {
            ActionCommand::List(request_id, scope, cursor, limit) => HostEvent::ActionReviews {
                request_id,
                result: tokio::task::spawn_blocking(move || {
                    store.action_reviews(scope, cursor, limit, &cancel)
                })
                .await
                .map_err(anyhow::Error::from)
                .and_then(|r| r)
                .map_err(|e| e.to_string()),
            },
            ActionCommand::Detail(request_id, scope, action) => HostEvent::ActionReview {
                request_id,
                result: Box::new(
                    tokio::task::spawn_blocking(move || {
                        ensure!(!cancel.is_cancelled(), "Review cancelled");
                        store.action_review(scope, action)
                    })
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|r| r)
                    .map_err(|e| e.to_string()),
                ),
            },
            ActionCommand::Resolve(request_id, request) => {
                let session = request.scope.session_id;
                let lookup = store.clone();
                let scope = request.scope.clone();
                let operation = request.operation_id;
                let recorded =
                    tokio::task::spawn_blocking(move || lookup.action_resolution(scope, operation))
                        .await
                        .map_err(anyhow::Error::from)
                        .and_then(|value| value);
                let queued = self.queued.lock().contains_key(&session);
                let busy = queued
                    || self
                        .workers
                        .lock()
                        .await
                        .get(&session)
                        .is_some_and(|worker| worker.active.load(Ordering::Acquire));
                let result = if let Err(error) = recorded {
                    Err(error.to_string())
                } else if busy && recorded.as_ref().is_ok_and(|value| value.is_none()) {
                    Err(
                        "Wait until this chat has stopped working before recording an outcome"
                            .into(),
                    )
                } else {
                    tokio::task::spawn_blocking(move || store.resolve_action(*request, &cancel))
                        .await
                        .map_err(anyhow::Error::from)
                        .and_then(|r| r)
                        .map_err(|e| e.to_string())
                };
                HostEvent::ActionResolved { request_id, result }
            }
            ActionCommand::Receipt(request_id, scope, operation) => HostEvent::ActionResolution {
                request_id,
                result: tokio::task::spawn_blocking(move || {
                    ensure!(!cancel.is_cancelled(), "Review cancelled");
                    store.action_resolution(scope, operation)
                })
                .await
                .map_err(anyhow::Error::from)
                .and_then(|r| r)
                .map_err(|e| e.to_string()),
            },
            ActionCommand::LoadForm(request_id, scope) => HostEvent::ActionReviewForm {
                request_id,
                result: tokio::task::spawn_blocking(move || store.action_review_form(scope))
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|r| r)
                    .map_err(|e| e.to_string()),
            },
            ActionCommand::SaveForm(request_id, write) => HostEvent::ActionReviewFormSaved {
                request_id,
                result: tokio::task::spawn_blocking(move || store.save_action_review_form(*write))
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|r| r)
                    .map_err(|e| e.to_string()),
            },
        };
        if let Some(id) = id {
            self.capability_requests.lock().await.remove(&id);
        }
        self.events.send(event).await?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "action_control/tests.rs"]
mod tests;
