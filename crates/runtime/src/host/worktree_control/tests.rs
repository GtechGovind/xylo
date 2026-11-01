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

fn git(root: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("/usr/bin/git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[cfg(unix)]
#[tokio::test]
async fn worktree_submodule_configuration_never_executes_during_status_or_startup_cleanup() {
    let (_profile, host, _events) = super::super::tests::state();
    let source = repo();
    let repo = repo();
    std::fs::write(source.path().join("tracked.txt"), "original\n").unwrap();
    std::fs::write(
        source.path().join(".gitattributes"),
        "*.txt filter=hostile\n",
    )
    .unwrap();
    git(source.path(), &["add", "."]);
    git(source.path(), &["commit", "-m", "submodule data"]);
    let oid = git(source.path(), &["rev-parse", "HEAD"]);
    git(
        repo.path(),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{},nested", oid.trim()),
        ],
    );
    git(repo.path(), &["commit", "-m", "gitlink"]);
    let id = Uuid::new_v4();
    host.create_chat_worktree(id, repo.path().into())
        .await
        .unwrap();
    let path = host.store.conversation(id).unwrap().unwrap().workspace;
    git(&path, &["clone", source.path().to_str().unwrap(), "nested"]);
    let marker_dir = tempfile::tempdir().unwrap();
    let marker = marker_dir.path().join("EXECUTED");
    let command = format!("touch '{}'; cat", marker.display());
    git(
        &path.join("nested"),
        &["config", "filter.hostile.clean", &command],
    );
    std::fs::write(path.join("nested/tracked.txt"), "modified\n").unwrap();
    std::fs::File::options()
        .write(true)
        .open(path.join("nested/tracked.txt"))
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1))
        .unwrap();
    assert!(!marker.exists(), "Fixture must not run the filter");
    let result = worktree::status(&path).await;
    assert!(
        !marker.exists(),
        "Native status executed submodule configuration"
    );
    assert!(
        result.is_err(),
        "Native status must fail closed on submodules"
    );
    host.store.set_conversation_archived(id, true).unwrap();
    host.cleanup_archived_worktrees().await.unwrap();
    assert!(
        !marker.exists(),
        "Startup cleanup executed submodule configuration"
    );
    assert!(path.exists());
    assert!(worktree::remove(&path, false).await.is_err());
    assert!(worktree::remove(&path, true).await.is_err());
    assert!(!marker.exists());
}

#[tokio::test]
async fn worktree_removal_retains_worker_ownership_across_shutdown_timeout_and_retry() {
    let (_profile, host, _events) = super::super::tests::state();
    let repo = repo();
    let id = Uuid::new_v4();
    host.create_chat_worktree(id, repo.path().into())
        .await
        .unwrap();
    let path = host.store.conversation(id).unwrap().unwrap().workspace;
    let (commands, mut incoming) = mpsc::channel(8);
    let (exit, exited) = tokio::sync::watch::channel(false);
    let worker = WorkerHandle::new(commands, exited);
    host.workers.lock().await.insert(id, worker.clone());
    for _ in 0..2 {
        let error = host.remove_worktree(path.clone(), false).await.unwrap_err();
        assert!(error.to_string().contains("background jobs"), "{error:#}");
        assert!(path.exists());
        assert!(
            host.workers
                .lock()
                .await
                .get(&id)
                .is_some_and(|owned| owned.exited.same_channel(&worker.exited))
        );
        assert!(matches!(
            incoming.recv().await.unwrap().payload,
            WorkerCommand::Shutdown
        ));
    }
    assert!(
        host.ready_worker(id).await.is_err(),
        "A retiring worker must fence admission"
    );
    assert!(
        !worker.stop.is_cancelled(),
        "Admission must not kill a graceful shutdown"
    );
    exit.send(true).unwrap();
    host.remove_worktree(path.clone(), false).await.unwrap();
    assert!(!path.exists());
    assert!(!host.workers.lock().await.contains_key(&id));
}

#[tokio::test]
async fn worktree_removal_retains_worker_after_failed_shutdown_send() {
    let (_profile, host, _events) = super::super::tests::state();
    let repo = repo();
    let id = Uuid::new_v4();
    host.create_chat_worktree(id, repo.path().into())
        .await
        .unwrap();
    let path = host.store.conversation(id).unwrap().unwrap().workspace;
    let (commands, incoming) = mpsc::channel(1);
    let (exit, exited) = tokio::sync::watch::channel(false);
    let worker = WorkerHandle::new(commands, exited);
    drop(incoming);
    host.workers.lock().await.insert(id, worker.clone());
    for _ in 0..2 {
        assert!(host.remove_worktree(path.clone(), false).await.is_err());
        assert!(path.exists());
        assert!(host.workers.lock().await.contains_key(&id));
        assert!(host.ready_worker(id).await.is_err());
        assert!(!worker.stop.is_cancelled());
    }
    exit.send(true).unwrap();
    host.remove_worktree(path.clone(), false).await.unwrap();
    assert!(!path.exists());
    assert!(!host.workers.lock().await.contains_key(&id));
}
