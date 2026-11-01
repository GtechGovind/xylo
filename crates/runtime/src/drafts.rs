//! Typed, off-UI draft operations. A committed write always returns its receipt:
//! cancellation is checked before dispatch, not by abandoning an in-flight commit.
use anyhow::{Result, ensure};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;
use xylo_core::TurnRequest;
use xylo_services::Store;
pub use xylo_services::{
    DraftAdmission, DraftCursor, DraftKey, DraftPage, DraftSave, DraftSnapshot, DraftSubmission,
    DraftSummary, DraftWrite, MAX_DRAFT_ATTACHMENTS, MAX_DRAFT_BYTES,
};

pub async fn load(store: Store, key: DraftKey, cancel: CancellationToken) -> Result<DraftSnapshot> {
    ensure!(!cancel.is_cancelled(), "Draft load cancelled");
    tokio::task::spawn_blocking(move || store.draft(&key)).await?
}
pub async fn save(
    store: Store,
    request: DraftWrite,
    cancel: CancellationToken,
) -> Result<DraftSave> {
    ensure!(!cancel.is_cancelled(), "Draft save cancelled");
    tokio::task::spawn_blocking(move || store.save_draft(&request)).await?
}
pub async fn list(
    store: Store,
    workspace: Option<PathBuf>,
    before: Option<DraftCursor>,
    limit: usize,
    cancel: CancellationToken,
) -> Result<DraftPage> {
    ensure!(!cancel.is_cancelled(), "Draft listing cancelled");
    tokio::task::spawn_blocking(move || store.drafts_page(workspace, before, limit)).await?
}
/// Once dispatch capacity is reserved, this commit is not raced with cancellation.
/// The host must dispatch the admitted turn and emit this receipt after success,
/// including when cancellation arrived during the database commit.
pub async fn admit_turn(
    store: Store,
    turn: TurnRequest,
    submission: DraftSubmission,
) -> Result<DraftAdmission> {
    #[cfg(test)]
    let _observation = AdmissionObservation::new(turn.turn_id);
    tokio::task::spawn_blocking(move || store.admit_draft_turn(&turn, &submission)).await?
}

// Observation only: tests can synchronize at the noncancellable boundary
// without adding delays or controlling the transaction's production behavior.
#[cfg(test)]
static ADMISSIONS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<uuid::Uuid>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));
#[cfg(test)]
struct AdmissionObservation(uuid::Uuid);
#[cfg(test)]
impl AdmissionObservation {
    fn new(id: uuid::Uuid) -> Self {
        ADMISSIONS.lock().unwrap().insert(id);
        Self(id)
    }
}
#[cfg(test)]
impl Drop for AdmissionObservation {
    fn drop(&mut self) {
        ADMISSIONS.lock().unwrap().remove(&self.0);
    }
}
#[cfg(test)]
pub(crate) fn admission_in_progress(id: uuid::Uuid) -> bool {
    ADMISSIONS.lock().unwrap().contains(&id)
}

#[cfg(test)]
mod tests;

/// Make an independent Saved drafts entry, including independently owned images.
pub async fn preserve(
    store: Store,
    artifacts: PathBuf,
    mut write: DraftWrite,
    validate: Vec<xylo_core::ImageAttachment>,
) -> Result<DraftSave> {
    tokio::task::spawn_blocking(move || {
        let images = xylo_services::attachments::AttachmentStore::new(artifacts)?;
        for image in &validate {
            images.validate(image.session_id, std::slice::from_ref(image))?;
        }
        let cancel = CancellationToken::new();
        let mut copied = Vec::with_capacity(write.attachments.len());
        for image in &write.attachments {
            let bytes = images.read_image(image.session_id, image)?;
            copied.push(images.import_bytes(
                write.key.session_id,
                &image.name,
                &bytes,
                image.source,
                &cancel,
            )?);
        }
        write.attachments = copied;
        store.save_draft(&write)
    })
    .await?
}
