use super::*;
#[test]
fn extension_receipts_prevent_replay_and_recover_unknown_effects() {
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
    assert_eq!(store.recover_interrupted().unwrap(), 1);
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
}
