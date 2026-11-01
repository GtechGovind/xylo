//! End-to-end broker checkout isolation and approval regression tests.
use super::*;
use crate::tools::tests::fixture;
use std::{path::Path, process::Command};
use xylo_core::{EngineEvent, PermissionMode};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worktree_exit_retains_host_inspection_checkout_and_switches_back() {
    let (dir, broker, mut events) = fixture(PermissionMode::FullAccess);
    initialize(dir.path());
    let main = broker.workspace_root().canonicalize().unwrap();
    approve_integration(broker, &mut events, move |execution| {
        execution
            .execute(
                "enter-inspection",
                "worktree_enter",
                json!({"name":"host-inspection"}),
            )
            .unwrap();
        let root = execution.workspace_root();
        let _host_lease = worktree::retain_inspection_workspace(&root)
            .unwrap()
            .unwrap();
        assert!(!execution.connectors.may_have_local_processes());
        let exit: Value = serde_json::from_str(
            &execution
                .execute("exit-inspection", "worktree_exit", json!({"keep":false}))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(exit["kept"], true);
        assert!(root.exists());
        assert_eq!(execution.workspace_root(), main);
        assert_eq!(
            execution
                .store
                .conversation(execution.request.session_id)
                .unwrap()
                .unwrap()
                .workspace,
            main
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worktree_exit_retains_unknown_shell_cleanup_after_job_owner_is_dropped() {
    let (dir, broker, mut events) = fixture(PermissionMode::FullAccess);
    initialize(dir.path());
    let main = broker.workspace_root().canonicalize().unwrap();
    approve_integration(broker, &mut events, move |execution| {
        execution
            .execute(
                "enter-inspection",
                "worktree_enter",
                json!({"name":"host-inspection"}),
            )
            .unwrap();
        let root = execution.workspace_root();
        let ownership = worktree::retain_process_workspace(&root).unwrap().unwrap();
        let outcome: anyhow::Result<()> = Err(xylo_services::sandbox::CleanupUnknown(
            "injected missing acknowledgment".into(),
        )
        .into());
        ownership.release_after(&outcome).unwrap();
        assert!(!execution.connectors.may_have_local_processes());
        let exit: Value = serde_json::from_str(
            &execution
                .execute("exit-inspection", "worktree_exit", json!({"keep":false}))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(exit["kept"], true);
        assert!(root.exists());
        assert_eq!(execution.workspace_root(), main);
        assert_eq!(
            execution
                .store
                .conversation(execution.request.session_id)
                .unwrap()
                .unwrap()
                .workspace,
            main
        );
    })
    .await;
}

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

        execution
            .rules
            .lock()
            .insert("native-lsp:old-root-authority".into());
        let stale = execution.request.clone();
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
    // F runs pre-tool hooks before asking for the blocked action itself.
    let error = approve_integration(broker.clone(), &mut events, |execution| {
        execution.execute(
            "hook-denied-enter",
            "worktree_enter",
            json!({"name":"blocked"}),
        )
    })
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("Pre-tool hook prevented"),
        "{error:#}"
    );
    assert!(!dir.path().join(".xylo/worktrees/blocked").exists());
    assert_eq!(broker.workspace_root(), dir.path());
}

async fn approve_integration<T: Send + 'static>(
    broker: Arc<ToolBroker>,
    events: &mut tokio::sync::mpsc::Receiver<EngineEvent>,
    action: impl FnOnce(Arc<ToolBroker>) -> T + Send + 'static,
) -> T {
    let execution = broker.clone();
    let mut task = tokio::task::spawn_blocking(move || action(execution));
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            tokio::select! {
                result = &mut task => break result.unwrap(),
                event = events.recv() => if let Some(EngineEvent::ApprovalRequired { action_id, .. }) = event {
                    broker.session.approvals().decide_with(action_id, true, false).unwrap();
                }
            }
        }
    }).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn integrated_worktree_shell_job_retains_checkout_until_process_cleanup() {
    let (dir, broker, mut events) = fixture(PermissionMode::FullAccess);
    initialize(dir.path());
    let (kept, existed, root) = approve_integration(broker.clone(), &mut events, |execution| {
        execution
            .execute(
                "enter-job",
                "worktree_enter",
                json!({"name":"job-lifetime"}),
            )
            .unwrap();
        let root = execution.workspace_root();
        let started: Value = serde_json::from_str(
            &execution
                .execute(
                    "job",
                    "shell_command",
                    json!({"command":"printf ready; sleep 60","run_in_background":true}),
                )
                .unwrap(),
        )
        .unwrap();
        let id = uuid::Uuid::parse_str(started["summary"]["job_id"].as_str().unwrap()).unwrap();
        let exited: Value = serde_json::from_str(
            &execution
                .execute("exit-job", "worktree_exit", json!({"keep":false}))
                .unwrap(),
        )
        .unwrap();
        let existed = root.exists();
        execution
            .session
            .kill_job(execution.request.session_id, id)
            .unwrap();
        (exited["kept"].as_bool().unwrap(), existed, root)
    })
    .await;
    broker.session.shell_jobs.shutdown().await;
    assert!(kept && existed, "A live process must keep its checkout");
    assert!(
        !broker
            .session
            .workspace_in_use(broker.request.session_id, &root),
        "A finished job buffer must not retain the process lease"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn integrated_worktree_refresh_preserves_explicit_sdk_prompt() {
    let (dir, mut broker, mut events) = fixture(PermissionMode::FullAccess);
    initialize(dir.path());
    let mut settings = broker.store.settings().unwrap();
    settings.output_style = "learning".into();
    broker.store.save_settings(&settings).unwrap();
    Arc::get_mut(&mut broker)
        .unwrap()
        .configure_run(&crate::RunOptions {
            append_system_prompt: "Keep the SDK output contract".into(),
            ..Default::default()
        })
        .unwrap();
    let prompt = approve_integration(broker, &mut events, |execution| {
        execution
            .execute("enter-sdk", "worktree_enter", json!({"name":"sdk-prompt"}))
            .unwrap();
        execution.refreshed_system_prompt().unwrap()
    })
    .await;
    assert!(prompt.contains("Keep the SDK output contract"));
    assert!(prompt.contains(include_str!("../../../prompts/output-style-learning.md").trim()));
}

struct IntegrationProvider(&'static str);
impl crate::model::ModelProvider for IntegrationProvider {
    fn stream(
        &self,
        _: crate::model::ModelRequest,
        _: tokio_util::sync::CancellationToken,
    ) -> crate::model::EventStream {
        Box::pin(futures_util::stream::iter([
            Ok(crate::model::StreamEvent::Text(self.0.into())),
            Ok(crate::model::StreamEvent::Finished(
                crate::model::FinishReason::Stop,
            )),
        ]))
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn integrated_worktree_web_utility_tracks_provider_changes_after_rebinding() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (dir, broker, mut events) = fixture(PermissionMode::FullAccess);
    initialize(dir.path());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/fixture", listener.local_addr().unwrap());
    broker.session.register_web_fixture(&url).unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = [0; 8192];
        let _ = stream.read(&mut bytes).await.unwrap();
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 12\r\nConnection: close\r\n\r\n<h1>Doc</h1>").await.unwrap();
    });
    broker.set_web_provider(Arc::new(IntegrationProvider("First provider")));
    approve_integration(broker, &mut events, move |execution| {
        execution
            .execute(
                "enter-web",
                "worktree_enter",
                json!({"name":"web-provider"}),
            )
            .unwrap();
        let first = execution
            .execute(
                "fetch-first",
                "web_fetch",
                json!({"url":url,"prompt":"Extract"}),
            )
            .unwrap();
        assert!(first.contains("First provider"));
        execution.set_web_provider(Arc::new(IntegrationProvider("Updated provider")));
        let second = execution
            .execute(
                "fetch-next",
                "web_fetch",
                json!({"url":url,"prompt":"Extract"}),
            )
            .unwrap();
        assert!(second.contains("Updated provider"));
    })
    .await;
    server.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn integrated_settings_hooks_follow_worktrees_and_keep_terminal_controls() {
    let (dir, broker, mut events) = fixture(PermissionMode::FullAccess);
    initialize(dir.path());
    approve_integration(broker.clone(), &mut events, |execution| {
        execution.execute("enter-settings", "worktree_enter", json!({"name":"settings"})).unwrap();
        let root = execution.workspace_root();
        execution.execute("write-in-checkout", "shell_command", json!({"command":"printf checked > shell-result.txt"})).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("shell-result.txt")).unwrap(), "checked");
        std::fs::create_dir_all(root.join(".xylo")).unwrap();
        std::fs::write(root.join(".xylo/settings.json"), json!({"hooks":{
            "Stop":[{"hooks":[{"command":"printf '%s' '{\"additionalContext\":\"current checkout context\"}'"}]}],
            "PreToolUse":[{"matcher":"^Write$","hooks":[{"command":"printf '%s' '{\"continue\":false,\"stopReason\":\"checkout stop\",\"additionalContext\":\"checkout hook note\"}'"}]}]
        }}).to_string()).unwrap();
        let context = execution.lifecycle(None, "Stop", json!({})).unwrap();
        assert!(context.context.iter().any(|value| value.contains("current checkout context")));
        let error = execution.execute("stopped-write", "file_write", json!({"path":"never-written.txt","content":"not allowed"})).unwrap_err();
        assert!(error.to_string().contains("Pre-tool hook prevented"), "{error:#}");
        assert!(!root.join("never-written.txt").exists());
        assert_eq!(execution.hook_stop.lock().as_deref(), Some("checkout stop"));
        assert!(execution.take_hook_notes().iter().any(|(_, text)| text.contains("checkout hook note")));
    }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn integrated_worktree_instructions_persist_exposure_and_reach_parent_boundary() {
    let (dir, broker, mut events) = fixture(PermissionMode::FullAccess);
    initialize(dir.path());
    let home = std::env::var_os("HOME").unwrap();
    let outside = tempfile::Builder::new()
        .prefix("xylo-worktree-guide-")
        .tempdir_in(home)
        .unwrap();
    let guide = outside.path().join("guide.md");
    std::fs::write(&guide, "Imported checkout instruction marker").unwrap();
    std::fs::write(
        dir.path().join("CLAUDE.md"),
        format!("@{}\n", guide.display()),
    )
    .unwrap();
    git(dir.path(), &["add", "CLAUDE.md"]);
    git(dir.path(), &["commit", "-m", "checkout instructions"]);
    approve_integration(broker.clone(), &mut events, |execution| {
        let entered = execution
            .execute(
                "enter-provenance",
                "worktree_enter",
                json!({"name":"instruction-scope"}),
            )
            .unwrap();
        assert!(entered.contains("Imported checkout instruction marker"));
        assert_eq!(
            execution.exposure().unwrap(),
            xylo_core::MemoryOrigin::ToolOutput
        );
        let root = execution.workspace_root();
        std::fs::create_dir_all(root.join(".xylo/rules")).unwrap();
        std::fs::write(
            root.join(".xylo/rules/checkout.md"),
            "---\npaths: [file.txt]\n---\nLazy checkout rule marker\n",
        )
        .unwrap();
        execution
            .execute(
                "read-checkout-rule",
                "file_read",
                json!({"path":"file.txt"}),
            )
            .unwrap();
        assert!(
            execution
                .take_instruction_notes()
                .join("\n")
                .contains("Lazy checkout rule marker"),
            "rebound tools must deliver scoped notes to the original loop broker"
        );
        assert!(execution.take_instruction_notes().is_empty());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn integrated_headless_questions_remain_noninteractive_after_worktree_rebind() {
    let (dir, mut broker, mut events) = fixture(PermissionMode::FullAccess);
    initialize(dir.path());
    Arc::get_mut(&mut broker)
        .unwrap()
        .configure_run(&crate::RunOptions {
            interactive: false,
            ..Default::default()
        })
        .unwrap();
    approve_integration(broker.clone(), &mut events, |execution| {
        execution
            .execute(
                "enter-headless",
                "worktree_enter",
                json!({"name":"headless-scope"}),
            )
            .unwrap();
    })
    .await;
    let execution = broker.clone();
    let mut task = tokio::task::spawn_blocking(move || {
        execution.execute("headless-question", "ask_user", json!({"questions":[{"header":"Scope","question":"Which change?","options":[{"label":"Small","description":"One file"},{"label":"Broad","description":"All files"}],"multi_select":false}]}))
    });
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), &mut task).await;
    if result.is_err() {
        broker.cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), task).await;
    }
    let output = result
        .expect("Headless rebound must not wait for a user")
        .unwrap()
        .unwrap();
    assert!(output.contains("No interactive client"), "{output}");
}
