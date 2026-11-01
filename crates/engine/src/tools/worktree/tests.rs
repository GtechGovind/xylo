//! End-to-end broker checkout isolation and approval regression tests.
use super::*;
use crate::tools::tests::fixture;
use std::{path::Path, process::Command};
use xylo_core::{EngineEvent, PermissionMode};

fn git(root: &Path, args: &[&str]) {
    let result = Command::new("/usr/bin/git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
fn initialize(root: &Path) {
    git(root, &["init", "-b", "main"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "user.email", "test@example.invalid"]);
    std::fs::write(root.join("file.txt"), "base\n").unwrap();
    git(root, &["add", "file.txt"]);
    git(root, &["commit", "-m", "base"]);
    std::fs::write(root.join("file.txt"), "main checkout dirty\n").unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worktree_switch_routes_edits_and_refreshes_root_authority() {
    let (dir, broker, mut events) = fixture(PermissionMode::FullAccess);
    initialize(dir.path());
    let execution = broker.clone();
    let job = tokio::task::spawn_blocking(move || {
        let entered: Value = serde_json::from_str(
            &execution
                .execute("enter", "worktree_enter", json!({"name":"isolated"}))
                .unwrap(),
        )
        .unwrap();
        let path = PathBuf::from(entered["workspace"].as_str().unwrap());
        assert_eq!(execution.workspace_root(), path);
        let child = execution.scoped_child(uuid::Uuid::new_v4()).unwrap();
        assert_eq!(child.workspace_root(), path);

        let mut stale = execution.request.clone();
        stale.approved_rules = vec!["old-root-authority".into()];
        let rebuilt = ToolBroker::new(
            stale,
            execution.store.clone(),
            execution.capability_config.clone(),
            execution.session.clone(),
            execution.events.clone(),
            execution.cancel.clone(),
        )
        .unwrap();
        assert!(
            rebuilt.rules.lock().is_empty(),
            "An old-root request must not transplant its authority"
        );

        assert!(
            execution
                .refreshed_system_prompt()
                .unwrap()
                .contains(path.to_str().unwrap())
        );
        let read = execution
            .execute("read", "file_read", json!({"path":"file.txt"}))
            .unwrap();
        assert!(read.contains("base") && !read.contains("main checkout dirty"));
        execution
            .execute(
                "write",
                "file_write",
                json!({"path":"file.txt","content":"worktree edit\n"}),
            )
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(path.join("file.txt")).unwrap(),
            "worktree edit\n"
        );
        let exit: Value = serde_json::from_str(
            &execution
                .execute("exit", "worktree_exit", json!({"keep":false}))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(exit["kept"], true);
        assert!(path.exists());
        assert_eq!(
            execution.workspace_root(),
            dir.path().canonicalize().unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("file.txt")).unwrap(),
            "main checkout dirty\n"
        );
        execution
            .execute("enter-clean", "worktree_enter", json!({"name":"clean"}))
            .unwrap();
        let clean = execution.workspace_root();
        execution
            .execute("exit-clean", "worktree_exit", json!({"keep":false}))
            .unwrap();
        assert!(!clean.exists());
        execution
            .execute("enter-busy", "worktree_enter", json!({"name":"busy"}))
            .unwrap();
        let busy = execution.workspace_root();
        let lease = execution
            .session
            .retain_workspace(execution.request.session_id, &busy);
        let exited: Value = serde_json::from_str(
            &execution
                .execute("exit-busy", "worktree_exit", json!({"keep":false}))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(exited["kept"], true);
        assert!(busy.exists());
        assert!(
            execution
                .session
                .workspace_in_use(execution.request.session_id, &busy)
        );
        drop(lease);
        assert!(
            !execution
                .session
                .workspace_in_use(execution.request.session_id, &busy)
        );

        assert_eq!(
            execution
                .store
                .conversation(execution.request.session_id)
                .unwrap()
                .unwrap()
                .workspace,
            execution.workspace_root()
        );
    });
    let mut approvals = 0;
    loop {
        tokio::select! {
            event = events.recv() => if let Some(EngineEvent::ApprovalRequired { action_id, name, rule, .. }) = event {
                assert!(matches!(name.as_str(), "worktree_enter" | "worktree_exit"));
                assert!(rule.is_none());
                approvals += 1;
                broker.session.approvals().decide_with(action_id, true, false).unwrap();
            },
            _ = tokio::time::sleep(std::time::Duration::from_millis(25)) => if job.is_finished() { break; },
        }
    }
    job.await.unwrap();
    assert_eq!(approvals, 6, "Worktree transitions ask even in Full access");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn denied_worktree_switch_does_not_create_or_redirect() {
    let (dir, broker, mut events) = fixture(PermissionMode::AskForApproval);
    initialize(dir.path());
    let root = broker.workspace_root();
    let execution = broker.clone();
    let job = tokio::task::spawn_blocking(move || {
        execution.execute("denied", "worktree_enter", json!({"name":"denied"}))
    });
    while let Some(event) = events.recv().await {
        if let EngineEvent::ApprovalRequired { action_id, .. } = event {
            broker
                .session
                .approvals()
                .decide_with(action_id, false, false)
                .unwrap();
            break;
        }
    }
    assert!(job.await.unwrap().is_err());
    assert_eq!(broker.workspace_root(), root);
    assert!(!root.join(".xylo/worktrees/denied").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worktree_switch_waits_for_a_previously_authorized_action_root() {
    let (dir, broker, mut events) = fixture(PermissionMode::AskForApproval);
    initialize(dir.path());
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("approved.txt");
    let writing = broker.clone();
    let target_arg = target.clone();
    let write = tokio::task::spawn_blocking(move || {
        writing.execute(
            "outside-write",
            "file_write",
            json!({"path":target_arg,"content":"approved"}),
        )
    });
    let approval = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Some(EngineEvent::ApprovalRequired { action_id, .. }) = events.recv().await {
                break action_id;
            }
        }
    })
    .await
    .unwrap();
    let original = broker.workspace_root();
    let entering = broker.clone();
    let enter = tokio::task::spawn_blocking(move || {
        entering.execute(
            "concurrent-enter",
            "worktree_enter",
            json!({"name":"concurrent"}),
        )
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(broker.workspace_root(), original);
    assert!(!enter.is_finished());
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(event, EngineEvent::ToolStarted { ref name, .. } if name == "worktree_enter")
        );
    }
    broker
        .session
        .approvals()
        .decide_with(approval, true, false)
        .unwrap();
    write.await.unwrap().unwrap();
    let approval = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Some(EngineEvent::ApprovalRequired {
                action_id, name, ..
            }) = events.recv().await
            {
                assert_eq!(name, "worktree_enter");
                break action_id;
            }
        }
    })
    .await
    .unwrap();
    broker
        .session
        .approvals()
        .decide_with(approval, true, false)
        .unwrap();
    enter.await.unwrap().unwrap();
    assert_ne!(broker.workspace_root(), original);
    assert_eq!(std::fs::read_to_string(target).unwrap(), "approved");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worktree_transition_respects_an_installed_pre_tool_hook() {
    let (dir, broker, mut events) = fixture(PermissionMode::AskForApproval);
    initialize(dir.path());
    let source = tempfile::tempdir().unwrap();
    std::fs::create_dir(source.path().join("hooks")).unwrap();
    std::fs::write(source.path().join("hooks/hooks.json"), json!({"hooks":{"PreToolUse":[{"matcher":"worktree_enter","hooks":[{"type":"command","command":"printf '{\"hookSpecificOutput\":{\"permissionDecision\":\"deny\"}}'"}]}]}}).to_string()).unwrap();
    let service = crate::tools::portable::service(&broker);
    let package = service.install_local(source.path()).unwrap();
    service.set_enabled(&package.id, true).unwrap();
    service
        .component_setup(
            &package.id,
            "hooks",
            xylo_services::extensions::ComponentSetup {
                workspace: broker.request.workspace.clone(),
                enabled_events: vec!["PreToolUse".into()],
                timeout_seconds: 5,
                ..Default::default()
            },
        )
        .unwrap();
    let execution = broker.clone();
    let job = tokio::task::spawn_blocking(move || {
        execution.execute(
            "hook-denied-enter",
            "worktree_enter",
            json!({"name":"blocked"}),
        )
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Some(EngineEvent::ApprovalRequired { action_id, .. }) = events.recv().await {
                broker
                    .session
                    .approvals()
                    .decide_with(action_id, true, false)
                    .unwrap();
                break;
            }
        }
    })
    .await
    .unwrap();
    let error = tokio::time::timeout(std::time::Duration::from_secs(20), job)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(
        error.to_string().contains("Pre-tool hook prevented"),
        "{error:#}"
    );
    assert!(!dir.path().join(".xylo/worktrees/blocked").exists());
    assert_eq!(broker.workspace_root(), dir.path());
}
