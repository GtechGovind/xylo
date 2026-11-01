//! Native loop controls retain the chat, authority and stop state on disk.
use super::*;

#[tokio::test]
async fn interaction_host_loop_is_durable_and_can_be_stopped() {
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::at(directory.path().join("profile")).unwrap();
    let workspace = directory.path().canonicalize().unwrap();
    let (host, events) = HostClient::spawn_at(paths.clone()).unwrap();
    let session = Uuid::new_v4();
    host.start_loop(session, workspace.clone(), "/loop 1m check build".into())
        .unwrap();
    let task = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match events.recv().await.unwrap() {
                HostEvent::Schedules(tasks) => {
                    if let Some(task) = tasks.into_iter().find(|task| {
                        task.wakeup
                            .as_ref()
                            .is_some_and(|wakeup| wakeup.session_id == session)
                    }) {
                        break task;
                    }
                }
                HostEvent::Error(error) => panic!("{error}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(task.wakeup.as_ref().unwrap().interval_seconds, Some(60));
    assert_eq!(task.workspace, workspace);
    host.stop_wakeups(session).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let HostEvent::Schedules(tasks) = events.recv().await.unwrap()
                && tasks
                    .iter()
                    .any(|saved| saved.id == task.id && !saved.enabled)
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    host.shutdown().unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !matches!(events.recv().await.unwrap(), HostEvent::Stopped) {}
    })
    .await
    .unwrap();
    let reopened =
        crate::scheduler::Scheduler::new(paths.artifacts.join("schedules.json"), workspace)
            .unwrap();
    let saved = reopened.list().await.unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].wakeup, task.wakeup);
    assert!(!saved[0].enabled);
}

async fn due_wakeup(host: &HostState) -> crate::scheduler::ScheduledTask {
    let scheduler = host.default_scheduler().unwrap();
    let mut tasks = scheduler.list().await.unwrap();
    tasks[0].next_run -= std::time::Duration::from_secs(3601);
    std::fs::write(
        host.paths.artifacts.join("schedules.json"),
        serde_json::to_vec(&tasks).unwrap(),
    )
    .unwrap();
    scheduler.claim_due().await.unwrap().remove(0)
}

#[tokio::test]
async fn interaction_host_busy_chat_preserves_user_reservation_and_defers_wakeup() {
    let (root, host, _events) = state();
    let session = Uuid::new_v4();
    host.start_loop(session, root.path().to_owned(), "/loop 60s check".into())
        .await
        .unwrap();
    let task = due_wakeup(&host).await;
    let user_turn = Uuid::new_v4();
    host.queued
        .lock()
        .insert(session, (user_turn, CancellationToken::new()));
    host.dispatch_wakeup(task.clone()).await.unwrap();
    assert_eq!(host.queued.lock().get(&session).unwrap().0, user_turn);
    assert!(host.workers.lock().await.is_empty());
    assert!(host.scheduled.lock().await.is_empty());
    let saved = host.default_scheduler().unwrap().list().await.unwrap();
    assert!(saved[0].claim.is_none());
    assert!(saved[0].enabled);
}

#[tokio::test]
async fn interaction_host_archived_or_missing_project_never_admits_wakeup() {
    let (root, host, _events) = state();
    let workspace = root.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    let session = Uuid::new_v4();
    host.start_loop(session, workspace.clone(), "/loop check".into())
        .await
        .unwrap();
    let task = due_wakeup(&host).await;
    host.store.set_conversation_archived(session, true).unwrap();
    assert!(
        host.dispatch_wakeup(task.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("archived")
    );
    assert!(
        host.start_loop(session, workspace.clone(), "/loop check".into())
            .await
            .is_err()
    );
    host.store
        .set_conversation_archived(session, false)
        .unwrap();
    std::fs::remove_dir(&workspace).unwrap();
    assert!(host.dispatch_wakeup(task).await.is_err());
    assert!(host.queued.lock().is_empty());
    assert!(host.scheduled.lock().await.is_empty());
    assert!(host.workers.lock().await.is_empty());
}

#[tokio::test]
async fn interaction_host_original_permission_is_a_cap_after_setting_changes() {
    let (root, host, _events) = state();
    let session = Uuid::new_v4();
    host.settings.lock().await.permissions = PermissionMode::AskForApproval;
    host.start_loop(session, root.path().to_owned(), "/loop check".into())
        .await
        .unwrap();
    let task = due_wakeup(&host).await;
    host.scheduled.lock().await.insert(session, task.id);
    assert_eq!(
        host.wakeup_permission(session, PermissionMode::FullAccess)
            .await
            .unwrap(),
        PermissionMode::AskForApproval
    );
    assert_eq!(
        host.wakeup_permission(session, PermissionMode::Plan)
            .await
            .unwrap(),
        PermissionMode::Plan
    );
}

#[tokio::test]
async fn interaction_host_failed_wakeup_releases_ownership_before_manual_followup() {
    let (root, host, _events) = state();
    let session = Uuid::new_v4();
    host.start_loop(session, root.path().to_owned(), "/loop check".into())
        .await
        .unwrap();
    let task = due_wakeup(&host).await;
    host.scheduled.lock().await.insert(session, task.id);
    interaction_control::pause_schedule(
        &host.default_scheduler().unwrap(),
        &host.scheduled,
        session,
    )
    .await
    .unwrap();
    assert!(host.scheduled.lock().await.is_empty());
    assert!(!host.default_scheduler().unwrap().list().await.unwrap()[0].enabled);
    assert_eq!(
        host.wakeup_permission(session, PermissionMode::FullAccess)
            .await
            .unwrap(),
        PermissionMode::FullAccess
    );
}

#[tokio::test]
async fn interaction_host_stop_cancels_only_the_reserved_wakeup() {
    let (root, host, _events) = state();
    let session = Uuid::new_v4();
    host.start_loop(session, root.path().to_owned(), "/loop check".into())
        .await
        .unwrap();
    let task = due_wakeup(&host).await;
    let cancel = CancellationToken::new();
    host.queued
        .lock()
        .insert(session, (task.claim.unwrap().occurrence, cancel.clone()));
    host.scheduled.lock().await.insert(session, task.id);
    host.stop_wakeups(session).await.unwrap();
    assert!(cancel.is_cancelled());
    assert!(
        host.wakeup_permission(session, PermissionMode::FullAccess)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn interaction_host_plan_answer_obeys_captured_and_current_authority() {
    use xylo_core::InteractionAnswer;
    let (root, host, _events) = state();
    let session = Uuid::new_v4();
    host.settings.lock().await.permissions = PermissionMode::AskForApproval;
    host.start_loop(session, root.path().to_owned(), "/loop check".into())
        .await
        .unwrap();
    let task = due_wakeup(&host).await;
    host.scheduled.lock().await.insert(session, task.id);
    let (mut incoming, _exit) = draft::mock_worker(&host, session, 4).await;
    for (current, expected) in [
        (PermissionMode::FullAccess, PermissionMode::AskForApproval),
        (PermissionMode::ReadOnly, PermissionMode::ReadOnly),
    ] {
        host.settings.lock().await.permissions = current;
        host.control_worker(
            Some(session),
            WorkerCommand::Answer {
                action_id: Uuid::new_v4(),
                answer: InteractionAnswer::ApprovePlan {
                    mode: PermissionMode::FullAccess,
                },
            },
        )
        .await
        .unwrap();
        let command = incoming.recv().await.unwrap();
        assert!(
            matches!(command.payload, WorkerCommand::Answer {answer: InteractionAnswer::ApprovePlan {mode}, ..} if mode == expected),
            "{command:?}"
        );
    }
}

#[tokio::test]
async fn interaction_host_delivery_failure_is_correlated() {
    let (_root, host, events) = state();
    let session = Uuid::new_v4();
    let id = Uuid::new_v4();
    host.control_worker(
        Some(session),
        WorkerCommand::Answer {
            action_id: id,
            answer: xylo_core::InteractionAnswer::Reject,
        },
    )
    .await
    .unwrap();
    assert!(
        matches!(events.recv().await.unwrap(), HostEvent::InteractionReply { session:s, action_id, result:Err(_) } if s == session && action_id == id)
    );
}

#[tokio::test]
async fn interaction_plan_answer_and_permission_updates_share_admission_order() {
    let (root, host, _events) = state();
    let session = Uuid::new_v4();
    host.settings.lock().await.permissions = PermissionMode::AskForApproval;
    host.start_loop(session, root.path().to_owned(), "/loop check".into())
        .await
        .unwrap();
    let task = due_wakeup(&host).await;
    host.scheduled.lock().await.insert(session, task.id);
    let (mut incoming, _exit) = draft::mock_worker(&host, session, 4).await;
    let worker = host.workers.lock().await[&session].clone();
    let guard = worker.permission_order.lock().await;
    let mut answer = Box::pin(host.control_worker(
        Some(session),
        WorkerCommand::Answer {
            action_id: Uuid::new_v4(),
            answer: xylo_core::InteractionAnswer::ApprovePlan {
                mode: PermissionMode::FullAccess,
            },
        },
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut answer)
            .await
            .is_err()
    );
    assert!(incoming.try_recv().is_err());
    host.settings.lock().await.permissions = PermissionMode::ReadOnly;
    let mut update = Box::pin(host.forward_permissions(PermissionMode::ReadOnly));
    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut update)
            .await
            .is_err()
    );
    drop(guard);
    let (answered, ()) = tokio::join!(answer, update);
    answered.unwrap();
    assert!(matches!(
        incoming.recv().await.unwrap().payload,
        WorkerCommand::Answer {
            answer: xylo_core::InteractionAnswer::ApprovePlan {
                mode: PermissionMode::ReadOnly
            },
            ..
        }
    ));
    assert!(matches!(
        incoming.recv().await.unwrap().payload,
        WorkerCommand::SetPermissions {
            mode: PermissionMode::ReadOnly
        }
    ));
}

#[tokio::test]
async fn interaction_plan_selection_is_forwarded_after_continuation() {
    let (_root, host, _events) = state();
    let session = Uuid::new_v4();
    host.settings.lock().await.permissions = PermissionMode::Plan;
    let (mut incoming, _exit) = draft::mock_worker(&host, session, 4).await;
    host.control_worker(
        Some(session),
        WorkerCommand::SetPermissions {
            mode: PermissionMode::Plan,
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        incoming.recv().await.unwrap().payload,
        WorkerCommand::SetPermissions {
            mode: PermissionMode::Plan
        }
    ));
    assert_eq!(
        *host.workers.lock().await[&session].mode.lock(),
        PermissionMode::Plan
    );
}
