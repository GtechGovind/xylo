//! Typed guidance persistence off the UI. Commits are awaited to their receipt;
//! abandoning a commit on cancellation could misreport application to a task.
use anyhow::Result;
use uuid::Uuid;
use workbench_core::SteeringRequest;
use workbench_services::{DraftSubmission, Store};
pub use workbench_services::{SteeringApplication, SteeringOutcome};
pub async fn queue(
    store: Store,
    request: SteeringRequest,
    submission: DraftSubmission,
) -> Result<SteeringOutcome> {
    tokio::task::spawn_blocking(move || store.queue_steering(&request, &submission)).await?
}
pub async fn lookup(store: Store, id: Uuid) -> Result<Option<SteeringOutcome>> {
    tokio::task::spawn_blocking(move || store.steering(id)).await?
}
pub async fn finish(
    store: Store,
    session: Uuid,
    turn: Option<Uuid>,
    reason: String,
) -> Result<Vec<SteeringOutcome>> {
    tokio::task::spawn_blocking(move || store.finish_pending_steering(session, turn, &reason))
        .await?
}
