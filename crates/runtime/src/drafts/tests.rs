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
