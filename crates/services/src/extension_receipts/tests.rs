use super::*;
#[test]
fn extension_attempts_are_live_and_disappear_on_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db")).unwrap();
    let id = Uuid::new_v4();
    let intent =
        json!({"package":"fixture","component":"hooks","digest":"version1","operation":"verify"});
    assert!(
        store
            .begin_extension_execution(id, &intent)
            .unwrap()
            .is_none()
    );
    assert!(store.begin_extension_execution(id, &intent).is_err());
    store
        .mark_extension_execution_unknown(id, "Disconnected")
        .unwrap();
    assert!(store.begin_extension_execution(id, &intent).is_err());
    assert_eq!(
        store.extension_execution_receipts(5).unwrap()[0]["state"],
        "outcome_unknown"
    );
    let next = Uuid::new_v4();
    store.begin_extension_execution(next, &intent).unwrap();
    store
        .finish_extension_execution(next, &json!({"executed":true}))
        .unwrap();
    assert_eq!(
        store.begin_extension_execution(next, &intent).unwrap(),
        Some(json!({"executed":true}))
    );
    let reopened = Store::open(dir.path().join("db")).unwrap();
    assert!(
        reopened
            .extension_execution_receipts(10)
            .unwrap()
            .is_empty()
    );
    assert!(
        reopened
            .begin_extension_execution(id, &intent)
            .unwrap()
            .is_none()
    );
}

#[test]
fn large_extension_input_is_fingerprinted_without_blocking_execution() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db")).unwrap();
    let id = Uuid::new_v4();
    let intent = json!({"input":"a".repeat(64 * 1024)});
    assert!(
        store
            .begin_extension_execution(id, &intent)
            .unwrap()
            .is_none()
    );
    assert!(store.begin_extension_execution(id, &intent).is_err());
    store
        .finish_extension_execution(id, &json!({"ok":true}))
        .unwrap();
    assert_eq!(
        store.begin_extension_execution(id, &intent).unwrap(),
        Some(json!({"ok":true}))
    );
    assert!(
        store
            .begin_extension_execution(id, &json!({"input":"changed"}))
            .is_err()
    );
    assert!(store.extension_execution_receipts(1).unwrap()[0]["intent"].is_null());
}
