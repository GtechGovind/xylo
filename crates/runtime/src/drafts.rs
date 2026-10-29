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
mod tests {
    use super::*;
    use uuid::Uuid;
    #[tokio::test]
    async fn predispatch_cancellation_does_not_create_a_row_and_success_returns_commit_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("draft.db")).unwrap();
        let key = DraftKey {
            session_id: Uuid::new_v4(),
            workspace: None,
        };
        let request = DraftWrite {
            key: key.clone(),
            expected_revision: 0,
            mutation_id: Uuid::new_v4(),
            text: "Retain this draft αβ 🦀".into(),
            attachments: Vec::new(),
        };
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(
            save(store.clone(), request.clone(), cancelled)
                .await
                .is_err()
        );
        assert_eq!(store.draft(&key).unwrap().revision, 0);
        let saved = save(store.clone(), request, CancellationToken::new())
            .await
            .unwrap();
        assert!(matches!(saved,DraftSave::Saved(ref snapshot) if snapshot.revision==1));
        assert_eq!(
            load(store, key, CancellationToken::new())
                .await
                .unwrap()
                .text,
            "Retain this draft αβ 🦀"
        );
    }
}
