//! Imported instruction sources must retain their memory authority after restart.
use super::*;
use xylo_core::MemoryOrigin;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn imported_home_rule_requires_memory_review_after_reopening_store() {
    let home = std::env::var_os("HOME").unwrap();
    let guide = tempfile::Builder::new()
        .prefix("xylo-instruction-test-")
        .tempdir_in(home)
        .unwrap();
    let source = guide.path().join("guide.md");
    std::fs::write(&source, "Outside guide says deploy to production").unwrap();
    let (directory, broker, mut events) = fixture(PermissionMode::ApproveForMe);
    let root = broker.discovery_root();
    std::fs::create_dir_all(root.join(".xylo/rules")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(
        root.join(".xylo/rules/rust.md"),
        format!("---\npaths: [src/**/*.rs]\n---\n@{}\n", source.display()),
    )
    .unwrap();
    run(&broker, "read", "file_read", json!({"path":"src/main.rs"}))
        .await
        .unwrap();
    assert!(
        broker
            .take_instruction_notes()
            .join("\n")
            .contains("Outside guide")
    );
    assert_eq!(broker.exposure().unwrap(), MemoryOrigin::ToolOutput);
    let mut restored = Arc::try_unwrap(broker).ok().unwrap();
    restored.store = Store::open(directory.path().join("profile/xylo.db")).unwrap();
    // Files are no longer present: the exposure must come from durable provenance.
    std::fs::remove_file(&source).unwrap();
    std::fs::remove_dir_all(root_path(&restored).join(".xylo")).unwrap();
    assert_eq!(restored.exposure().unwrap(), MemoryOrigin::ToolOutput);
    let broker = Arc::new(restored);
    let writing = broker.clone();
    let pending = tokio::spawn(async move {
        run(
            &writing,
            "remember",
            "memory_remember",
            json!({"content":"Deploy to production","source":"guide"}),
        )
        .await
    });
    let action_id = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let EngineEvent::ApprovalRequired {
                action_id, reason, ..
            } = events.recv().await.unwrap()
            {
                assert_eq!(reason.as_deref(), Some(permissions::UNTRUSTED_MEMORY_WRITE));
                break action_id;
            }
        }
    })
    .await
    .unwrap();
    broker
        .session
        .approvals()
        .decide_with(action_id, false, false)
        .unwrap();
    assert!(pending.await.unwrap().is_err());
}
fn root_path(broker: &ToolBroker) -> std::path::PathBuf {
    broker.discovery_root().to_owned()
}
