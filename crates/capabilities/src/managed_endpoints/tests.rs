use super::*;
#[test]
fn ownership_is_normalized_and_cleanup_preserves_other_servers() {
    let root = tempfile::tempdir().unwrap();
    // Registry entries are kept only for running processes.
    let mut server = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let other = server.id();
    let first = Registration::create(
        root.path(),
        "http://127.0.0.1:12121/v1/",
        std::process::id(),
    )
    .unwrap();
    let second = Registration::create(root.path(), "http://127.0.0.1:12122/v1", other).unwrap();
    let read = || {
        serde_json::from_slice::<serde_json::Value>(
            &std::fs::read(root.path().join("managed-endpoints.json")).unwrap(),
        )
        .unwrap()
    };
    assert_eq!(read()["http://127.0.0.1:12121/v1"]["single_slot"], true);
    drop(first);
    assert!(read().get("http://127.0.0.1:12121/v1").is_none());
    assert_eq!(read()["http://127.0.0.1:12122/v1"]["pid"], other);
    drop(second);
    assert_eq!(read(), serde_json::json!({}));
    let _ = server.kill();
    let _ = server.wait();
}

#[cfg(unix)]
#[test]
fn entries_of_exited_servers_are_pruned_so_unclean_exits_cannot_exhaust_the_registry() {
    let root = tempfile::tempdir().unwrap();
    let mut exited = std::process::Command::new("true").spawn().unwrap();
    let exited_pid = exited.id();
    exited.wait().unwrap();
    // 256 markers left behind by servers that never ran Drop (crash, kill -9),
    // one with a PID no system assigns.
    let mut stale = serde_json::Map::new();
    for port in 0..255u32 {
        stale.insert(
            format!("http://127.0.0.1:{}/v1", 20_000 + port),
            serde_json::json!({"single_slot":true,"pid":exited_pid}),
        );
    }
    stale.insert(
        "http://127.0.0.1:30000/v1".into(),
        serde_json::json!({"single_slot":true,"pid":i32::MAX}),
    );
    std::fs::write(
        root.path().join("managed-endpoints.json"),
        serde_json::to_vec(&stale).unwrap(),
    )
    .unwrap();
    let live =
        Registration::create(root.path(), "http://127.0.0.1:12123/v1", std::process::id()).unwrap();
    let entries: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join("managed-endpoints.json")).unwrap())
            .unwrap();
    assert_eq!(
        entries,
        serde_json::json!({"http://127.0.0.1:12123/v1":{"single_slot":true,"pid":std::process::id()}})
    );
    drop(live);
}
