use super::*;
#[test]
fn ownership_is_normalized_and_cleanup_preserves_other_servers() {
    let root = tempfile::tempdir().unwrap();
    let first = Registration::create(root.path(), "http://127.0.0.1:12121/v1/", 42).unwrap();
    let second = Registration::create(root.path(), "http://127.0.0.1:12122/v1", 43).unwrap();
    let read = || {
        serde_json::from_slice::<serde_json::Value>(
            &std::fs::read(root.path().join("managed-endpoints.json")).unwrap(),
        )
        .unwrap()
    };
    assert_eq!(read()["http://127.0.0.1:12121/v1"]["single_slot"], true);
    drop(first);
    assert!(read().get("http://127.0.0.1:12121/v1").is_none());
    assert_eq!(read()["http://127.0.0.1:12122/v1"]["pid"], 43);
    drop(second);
    assert_eq!(read(), serde_json::json!({}));
}
