use super::*;
struct Fixture {
    _directory: tempfile::TempDir,
    store: Store,
    artifacts: PathBuf,
    request: DocumentDraftRelinkRequest,
    edits: Vec<DocumentEdit>,
}
fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let artifacts = root.join("artifacts");
    std::fs::create_dir(&artifacts).unwrap();
    let old = root.join("original.docx");
    let moved = root.join("moved.docx");
    let bytes = b"opaque original Office package; never reconstructed";
    std::fs::write(&old, bytes).unwrap();
    std::fs::rename(&old, &moved).unwrap();
    let store = Store::open(root.join("state.db")).unwrap();
    let key = DocumentDraftKey {
        workspace: root.clone(),
        source: old.clone(),
    };
    let source = DocumentSource {
        path: old,
        revision: hex::encode(Sha256::digest(bytes)),
        bytes: bytes.len() as u64,
        kind: DocumentKind::Docx,
    };
    let edits = vec![DocumentEdit::Paragraph {
        index: 7,
        text: "Retained paragraph edit\nUnicode α".into(),
    }];
    let mut state = DocumentDraftState {
        source: source.clone(),
        edits: edits.clone(),
        export_name: "My unchanged copy name".into(),
        sheet: None,
        selected: None,
        pending_copy: None,
        pending_relink: None,
    };
    let request = DocumentDraftRelinkRequest {
        operation_id: Uuid::new_v4(),
        key: key.clone(),
        expected_revision: 2,
        source: DocumentSource {
            path: moved,
            ..source
        },
    };
    let save = |revision, state| {
        store
            .save_document_draft(DocumentDraftWrite {
                key: key.clone(),
                expected_revision: revision,
                mutation_id: Uuid::new_v4(),
                state: Some(state),
            })
            .unwrap()
    };
    assert!(matches!(
        save(0, state.clone()),
        DocumentDraftSave::Saved(_)
    ));
    state.pending_relink = Some(Box::new(request.clone()));
    assert!(matches!(save(1, state), DocumentDraftSave::Saved(_)));
    Fixture {
        _directory: directory,
        store,
        artifacts,
        request,
        edits,
    }
}
#[test]
fn atomic_relink_preserves_proposal_and_replays_after_restart_without_original() {
    let f = fixture();
    verify_document_relink_source(
        &f.request,
        &f.artifacts,
        &f.artifacts.parent().unwrap().join("native-management"),
        &CancellationToken::new(),
    )
    .unwrap();
    let receipt = f.store.relink_document_draft(f.request.clone()).unwrap();
    assert!(
        f.store
            .document_draft(f.request.key.clone())
            .unwrap()
            .state
            .is_none()
    );
    let saved = f
        .store
        .document_draft(receipt.key.clone())
        .unwrap()
        .state
        .unwrap();
    assert_eq!(saved.edits, f.edits);
    assert_eq!(saved.export_name, "My unchanged copy name");
    assert!(saved.pending_relink.is_none());
    assert_eq!(saved.source, f.request.source);
    std::fs::remove_file(&f.request.source.path).unwrap();
    let reopened = Store::open(f._directory.path().join("state.db")).unwrap();
    assert_eq!(
        reopened
            .document_draft_relink_status(f.request.key.clone(), f.request.operation_id)
            .unwrap(),
        Some(receipt.clone())
    );
    assert_eq!(
        reopened.relink_document_draft(f.request.clone()).unwrap(),
        receipt
    );
}
#[test]
fn stale_proposal_and_destination_edits_are_never_clobbered() {
    let f = fixture();
    let mut old = f.store.document_draft(f.request.key.clone()).unwrap();
    old.state
        .as_mut()
        .unwrap()
        .edits
        .push(DocumentEdit::Paragraph {
            index: 8,
            text: "Concurrent typing".into(),
        });
    assert!(matches!(
        f.store
            .save_document_draft(DocumentDraftWrite {
                key: old.key.clone(),
                expected_revision: old.revision,
                mutation_id: Uuid::new_v4(),
                state: old.state.clone()
            })
            .unwrap(),
        DocumentDraftSave::Saved(_)
    ));
    assert!(f.store.relink_document_draft(f.request.clone()).is_err());
    assert_eq!(
        f.store
            .document_draft(old.key)
            .unwrap()
            .state
            .unwrap()
            .edits
            .len(),
        2
    );
    let f = fixture();
    let destination = DocumentDraftKey {
        workspace: f.request.key.workspace.clone(),
        source: f.request.source.path.clone(),
    };
    let mut state = f
        .store
        .document_draft(f.request.key.clone())
        .unwrap()
        .state
        .unwrap();
    state.source = f.request.source.clone();
    state.pending_relink = None;
    state.edits = vec![DocumentEdit::Paragraph {
        index: 0,
        text: "Destination's own edit".into(),
    }];
    f.store
        .save_document_draft(DocumentDraftWrite {
            key: destination.clone(),
            expected_revision: 0,
            mutation_id: Uuid::new_v4(),
            state: Some(state),
        })
        .unwrap();
    assert!(f.store.relink_document_draft(f.request.clone()).is_err());
    assert_eq!(
        f.store
            .document_draft(f.request.key.clone())
            .unwrap()
            .state
            .unwrap()
            .edits,
        f.edits
    );
    assert_eq!(
        f.store
            .document_draft(destination)
            .unwrap()
            .state
            .unwrap()
            .edits
            .len(),
        1
    );
}
#[test]
fn changed_identity_scope_kind_hash_bytes_and_modified_file_fail_closed() {
    let f = fixture();
    let mut changed = f.request.clone();
    changed.source.kind = DocumentKind::Text;
    assert!(
        verify_document_relink_source(
            &changed,
            &f.artifacts,
            &f.artifacts.parent().unwrap().join("native-management"),
            &CancellationToken::new()
        )
        .is_err()
    );
    assert!(f.store.relink_document_draft(changed).is_err());
    let mut changed = f.request.clone();
    changed.source.bytes += 1;
    assert!(
        verify_document_relink_source(
            &changed,
            &f.artifacts,
            &f.artifacts.parent().unwrap().join("native-management"),
            &CancellationToken::new()
        )
        .is_err()
    );
    let mut changed = f.request.clone();
    changed.source.revision = "b".repeat(64);
    assert!(
        verify_document_relink_source(
            &changed,
            &f.artifacts,
            &f.artifacts.parent().unwrap().join("native-management"),
            &CancellationToken::new()
        )
        .is_err()
    );
    std::fs::write(&f.request.source.path, b"changed original").unwrap();
    assert!(
        verify_document_relink_source(
            &f.request,
            &f.artifacts,
            &f.artifacts.parent().unwrap().join("native-management"),
            &CancellationToken::new()
        )
        .is_err()
    );
    let mut other = f.request.key.clone();
    other.workspace = other.workspace.join("different-project");
    assert!(
        f.store
            .document_draft_relink_status(other, f.request.operation_id)
            .unwrap()
            .is_none()
    );
    assert!(
        f.store
            .document_draft(f.request.key.clone())
            .unwrap()
            .state
            .unwrap()
            .pending_relink
            .is_some()
    );
}
#[test]
fn same_operation_changed_arguments_and_wrong_scope_receipt_are_rejected() {
    let f = fixture();
    f.store.relink_document_draft(f.request.clone()).unwrap();
    let mut changed = f.request.clone();
    changed.source.path = changed.source.path.with_file_name("another.docx");
    assert!(f.store.relink_document_draft(changed).is_err());
    let mut other = f.request.key.clone();
    other.workspace = other.workspace.join("other");
    assert!(
        f.store
            .document_draft_relink_status(other, f.request.operation_id)
            .is_err()
    );
}
#[test]
fn clearing_pending_location_fences_late_relink_without_losing_edits() {
    let f = fixture();
    let snapshot = f.store.document_draft(f.request.key.clone()).unwrap();
    let mut state = snapshot.state.unwrap();
    state.pending_relink = None;
    f.store
        .save_document_draft(DocumentDraftWrite {
            key: snapshot.key.clone(),
            expected_revision: snapshot.revision,
            mutation_id: Uuid::new_v4(),
            state: Some(state),
        })
        .unwrap();
    assert!(f.store.relink_document_draft(f.request.clone()).is_err());
    assert_eq!(
        f.store
            .document_draft(snapshot.key)
            .unwrap()
            .state
            .unwrap()
            .edits,
        f.edits
    );
}
#[test]
fn pending_saved_copy_cannot_be_retargeted_to_a_new_source() {
    let f = fixture();
    let mut snapshot = f.store.document_draft(f.request.key.clone()).unwrap();
    let state = snapshot.state.as_mut().unwrap();
    state.pending_copy = Some(DocumentCopyIntent {
        operation_id: Uuid::new_v4(),
        generation: 1,
        name: "original pending copy".into(),
        edits: state.edits.clone(),
    });
    assert!(
        f.store
            .save_document_draft(DocumentDraftWrite {
                key: snapshot.key,
                expected_revision: snapshot.revision,
                mutation_id: Uuid::new_v4(),
                state: snapshot.state
            })
            .is_err()
    );
    assert_eq!(
        f.store
            .document_draft(f.request.key.clone())
            .unwrap()
            .state
            .unwrap()
            .edits,
        f.edits
    );
}

#[test]
fn cancellation_and_management_scope_never_move_a_proposal() {
    let f = fixture();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let management = f.artifacts.parent().unwrap().join("native-management");
    assert!(verify_document_relink_source(&f.request, &f.artifacts, &management, &cancel).is_err());
    assert!(
        f.store
            .relink_document_draft_cancellable(f.request.clone(), &cancel)
            .is_err()
    );
    std::fs::create_dir(&management).unwrap();
    let private = management.join("original.docx");
    std::fs::copy(&f.request.source.path, &private).unwrap();
    let mut request = f.request.clone();
    request.source.path = private;
    assert!(
        verify_document_relink_source(
            &request,
            &f.artifacts,
            &management,
            &CancellationToken::new()
        )
        .is_err()
    );
    assert_eq!(
        f.store
            .document_draft(f.request.key.clone())
            .unwrap()
            .state
            .unwrap()
            .edits,
        f.edits
    );
}
#[test]
fn replay_does_not_replace_newer_destination_edits() {
    let f = fixture();
    let receipt = f.store.relink_document_draft(f.request.clone()).unwrap();
    let mut snapshot = f.store.document_draft(receipt.key.clone()).unwrap();
    let edits = &mut snapshot.state.as_mut().unwrap().edits;
    edits.push(DocumentEdit::Paragraph {
        index: 1,
        text: "Newer destination typing".into(),
    });
    let expected = edits.clone();
    f.store
        .save_document_draft(DocumentDraftWrite {
            key: snapshot.key.clone(),
            expected_revision: snapshot.revision,
            mutation_id: Uuid::new_v4(),
            state: snapshot.state,
        })
        .unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        f.store
            .relink_document_draft_cancellable(f.request.clone(), &cancel)
            .unwrap(),
        receipt
    );
    assert_eq!(
        f.store
            .document_draft(snapshot.key)
            .unwrap()
            .state
            .unwrap()
            .edits,
        expected
    );
}
