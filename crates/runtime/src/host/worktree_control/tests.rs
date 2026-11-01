//! Host lifecycle coverage for new chats and archived worktree cleanup.
use super::*;
fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        vec!["init", "-b", "main"],
        vec!["config", "user.name", "Test"],
        vec!["config", "user.email", "test@example.invalid"],
        vec!["commit", "--allow-empty", "-m", "base"],
    ] {
        let output = std::process::Command::new("/usr/bin/git")
            .arg("-C")
            .arg(dir.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    dir
}
#[tokio::test]
async fn worktree_chat_start_cleanup_and_manual_removal_update_durable_roots() {
    let (_profile, host, _events) = super::super::tests::state();
    let repo = repo();
    let clean = Uuid::new_v4();
    host.create_chat_worktree(clean, repo.path().into())
        .await
        .unwrap();
    let clean_path = host.store.conversation(clean).unwrap().unwrap().workspace;
    assert!(clean_path.is_dir());
    assert!(
        host.create_chat_worktree(clean, repo.path().into())
            .await
            .is_err()
    );
    host.store.set_conversation_archived(clean, true).unwrap();
    let dirty = Uuid::new_v4();
    host.create_chat_worktree(dirty, repo.path().into())
        .await
        .unwrap();
    let dirty_path = host.store.conversation(dirty).unwrap().unwrap().workspace;
    std::fs::write(dirty_path.join("draft.txt"), "keep this").unwrap();
    host.store.set_conversation_archived(dirty, true).unwrap();
    host.cleanup_archived_worktrees().await.unwrap();
    assert!(!clean_path.exists());
    assert!(dirty_path.exists());
    assert_eq!(
        host.store.conversation(clean).unwrap().unwrap().workspace,
        repo.path().canonicalize().unwrap()
    );
    assert!(
        host.remove_worktree(dirty_path.clone(), false)
            .await
            .is_err()
    );
    host.remove_worktree(dirty_path.clone(), true)
        .await
        .unwrap();
    assert!(!dirty_path.exists());
    assert_eq!(
        host.store.conversation(dirty).unwrap().unwrap().workspace,
        repo.path().canonicalize().unwrap()
    );
}
#[tokio::test]
async fn worktree_cleanup_preserves_an_unarchived_chat_sharing_the_checkout() {
    let (_profile, host, _events) = super::super::tests::state();
    let repo = repo();
    let id = Uuid::new_v4();
    host.create_chat_worktree(id, repo.path().into())
        .await
        .unwrap();
    let mut chat = host.store.conversation(id).unwrap().unwrap();
    host.store.set_conversation_archived(id, true).unwrap();
    chat.id = Uuid::new_v4();
    host.store.upsert_conversation(&chat).unwrap();
    host.cleanup_archived_worktrees().await.unwrap();
    assert!(chat.workspace.exists());
}

#[tokio::test]
async fn worktree_settings_discovers_retained_checkout_after_chat_exits() {
    let (_profile, host, events) = super::super::tests::state();
    let repo = repo();
    let id = Uuid::new_v4();
    host.create_chat_worktree(id, repo.path().into())
        .await
        .unwrap();
    let retained = host.store.conversation(id).unwrap().unwrap().workspace;
    host.store
        .set_conversation_workspace(id, repo.path().canonicalize().unwrap())
        .unwrap();
    while events.try_recv().is_ok() {}
    host.list_worktrees(None).await.unwrap();
    let HostEvent::Worktrees(trees) = events.recv().await.unwrap() else {
        panic!("Expected worktree list");
    };
    assert!(trees.iter().any(|tree| tree.path == retained));
}
