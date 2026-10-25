use super::*;

#[tokio::test]
async fn damaged_embedding_configuration_keeps_the_host_available_for_repair() {
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::at(directory.path().to_owned()).unwrap();
    std::fs::write(paths.data.join("embedding.json"), b"not valid JSON").unwrap();
    let (host, events) = HostClient::spawn_at(paths).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut settings = false;
        let mut notice = false;
        while !(settings && notice) {
            match events.recv().await.unwrap() {
                HostEvent::Settings(_) => settings = true,
                HostEvent::Status(message) => {
                    notice |= message.contains("Semantic search needs setup")
                }
                HostEvent::Error(error) => {
                    panic!("Host failed instead of allowing repair: {error}")
                }
                _ => {}
            }
        }
        host.shutdown().unwrap();
        while !matches!(events.recv().await.unwrap(), HostEvent::Stopped) {}
    })
    .await
    .unwrap();
}

pub(super) fn state() -> (tempfile::TempDir, Arc<HostState>, Receiver<HostEvent>) {
    let dir = tempfile::tempdir().unwrap();
    let paths = AppPaths::at(dir.path().to_owned()).unwrap();
    let store = Store::open(&paths.database).unwrap();
    let vault = CredentialVault::new("org.agentworkbench.host-tests").unwrap();
    let accounts = workbench_services::providers::ChatGptSignInService::new(&paths.data).unwrap();
    let (events, receiver) = async_channel::bounded(256);
    let (editor_events, _editor_receiver) = async_channel::bounded(128);
    let lsp = crate::lsp::Service::new(paths.data.clone(), editor_events);
    let (workspace_events, _) = async_channel::bounded(16);
    let host = Arc::new(HostState {
        workspace: crate::workspace::WorkspaceService::new(workspace_events),
        search_requests: Mutex::new(HashMap::new()),
        image_import_gate: Mutex::new(()),
        updates: crate::updates::Service::new(paths.data.clone(), std::env::current_exe().unwrap())
            .unwrap(),
        operations: tokio::sync::RwLock::new(()),
        installing: Arc::new(AtomicBool::new(false)),
        install_request: Arc::new(parking_lot::Mutex::new(None)),
        notifications: crate::notifications::Service::new(
            paths.data.clone(),
            crate::notifications::AppIdentity {
                id: "org.agentworkbench.host-tests".into(),
                name: "Workbench tests".into(),
            },
        )
        .unwrap(),
        notices: async_channel::bounded(128).0,
        foreground: Arc::new(AtomicBool::new(false)),
        paths,
        memory: SqliteMemory::new(store.clone()),
        accounts,
        connectors: ConnectorBroker::new(store.clone(), vault.clone()),
        store,
        vault,
        settings: Mutex::new(AppSettings::default()),
        workers: Mutex::new(HashMap::new()),
        events,
        resources: ResourceManager::new(),
        queued: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        scheduled: Arc::new(Mutex::new(HashMap::new())),
        stopping: Arc::new(AtomicBool::new(false)),
        helpers: Mutex::new(HashMap::new()),
        capability_requests: Mutex::new(HashMap::new()),
        lsp,
    });
    (dir, host, receiver)
}

#[tokio::test]
async fn cancel_before_capability_task_runs_never_starts_a_helper() {
    let (_dir, host, events) = state();
    let id = Uuid::new_v4();
    let command = HostCommand::Capability(id, "document_export_docx".into(), serde_json::json!({}));
    assert!(host.prepare_command(&command).await.unwrap());
    assert!(
        !host
            .prepare_command(&HostCommand::CancelCapability(id))
            .await
            .unwrap()
    );
    host.handle(command).await.unwrap();
    assert!(
        matches!(events.recv().await.unwrap(),HostEvent::Capability{request_id,result:Err(_),..} if request_id==id)
    );
    assert!(host.helpers.lock().await.is_empty());
    assert!(host.capability_requests.lock().await.is_empty());
}

#[tokio::test]
async fn cancel_before_run_task_starts_does_not_record_or_dispatch_the_turn() {
    let (_dir, host, _events) = state();
    let session = Uuid::new_v4();
    let turn = TurnRequest {
        session_id: session,
        turn_id: Uuid::new_v4(),
        prompt: "Must not execute".into(),
        workspace: host.paths.data.clone(),
        provider: ProviderConfig::default(),
        permissions: Default::default(),
        attachments: Vec::new(),
    };
    let command = HostCommand::Worker(Some(session), WorkerCommand::Run { turn });
    assert!(host.prepare_command(&command).await.unwrap());
    host.worker_command(Some(session), WorkerCommand::Cancel)
        .await
        .unwrap();
    host.handle(command).await.unwrap();
    assert!(host.workers.lock().await.is_empty());
    assert!(host.queued.lock().is_empty());
    assert!(host.store.messages(session, 10).unwrap().is_empty());
}

#[tokio::test]
async fn admission_failure_reports_the_correlated_capability_result() {
    let (_dir, host, events) = state();
    for _ in 0..64 {
        host.capability_requests
            .lock()
            .await
            .insert(Uuid::new_v4(), CancellationToken::new());
    }
    let id = Uuid::new_v4();
    assert!(
        !host
            .prepare_command(&HostCommand::Capability(
                id,
                "document_read".into(),
                serde_json::json!({})
            ))
            .await
            .unwrap()
    );
    assert!(
        matches!(events.recv().await.unwrap(),HostEvent::Capability{request_id,result:Err(_),..} if request_id==id)
    );
}

#[test]
fn stale_admission_guard_cannot_remove_a_newer_run() {
    let pending = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let session = Uuid::new_v4();
    let old = PendingRun {
        session,
        id: Uuid::new_v4(),
        cancel: CancellationToken::new(),
        pending: pending.clone(),
    };
    let next = Uuid::new_v4();
    pending
        .lock()
        .insert(session, (next, CancellationToken::new()));
    drop(old);
    assert_eq!(pending.lock()[&session].0, next);
}

#[tokio::test]
async fn managed_turn_activity_preserves_the_server_across_tool_intervals() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("local-inference-activity.lock");
    let lease = acquire_model_activity(path.clone(), CancellationToken::new())
        .await
        .unwrap();
    assert!(model_activity_busy(&path).unwrap());
    let another = acquire_model_activity(path.clone(), CancellationToken::new())
        .await
        .unwrap();
    drop(lease);
    assert!(model_activity_busy(&path).unwrap());
    drop(another);
    assert!(!model_activity_busy(&path).unwrap());
}

#[tokio::test]
async fn a_failed_schedule_acknowledgement_retains_occurrence_ownership() {
    let dir = tempfile::tempdir().unwrap();
    let scheduler =
        crate::scheduler::Scheduler::new(dir.path().join("schedule.json"), dir.path().to_owned())
            .unwrap();
    let session = Uuid::new_v4();
    let id = Uuid::new_v4();
    let scheduled = Mutex::new(HashMap::from([(session, id)]));
    assert!(
        acknowledge_schedule(&scheduler, &scheduled, session)
            .await
            .is_err()
    );
    assert_eq!(scheduled.lock().await.get(&session), Some(&id));
}

#[tokio::test]
async fn unconfigured_install_preserves_data_and_releases_admission() {
    let (_dir, host, events) = state();
    let id = Uuid::new_v4();
    let command = HostCommand::Capability(id, "updates_install".into(), serde_json::json!({}));
    assert!(host.prepare_command(&command).await.unwrap());
    host.handle(command).await.unwrap();
    assert!(
        matches!(events.recv().await.unwrap(),HostEvent::Capability{request_id,result:Err(_),..} if request_id==id)
    );
    assert!(!host.installing.load(Ordering::Acquire));
    assert!(host.capability_requests.lock().await.is_empty());
    assert!(host.helpers.lock().await.is_empty());
    assert_eq!(host.store.settings().unwrap().provider.model, "");
}
#[tokio::test]
async fn install_admission_rejects_tools_with_a_correlated_result() {
    let (_dir, host, events) = state();
    host.installing.store(true, Ordering::Release);
    let id = Uuid::new_v4();
    let command = HostCommand::Capability(id, "document_export_docx".into(), serde_json::json!({}));
    assert!(!host.prepare_command(&command).await.unwrap());
    assert!(
        matches!(events.recv().await.unwrap(),HostEvent::Capability{request_id,result:Err(_),..} if request_id==id)
    );
    assert!(host.capability_requests.lock().await.is_empty());
    assert!(host.helpers.lock().await.is_empty());
}

#[tokio::test]
async fn retiring_workers_remain_supervised_until_reaped_and_active_turns_survive() {
    fn worker(active: bool) -> (WorkerHandle, tokio::sync::watch::Sender<bool>) {
        let (commands, _incoming) = mpsc::channel(2);
        let (exited_tx, exited) = tokio::sync::watch::channel(false);
        (
            WorkerHandle {
                commands,
                active: Arc::new(AtomicBool::new(active)),
                lease: Arc::new(Mutex::new(None)),
                stop: CancellationToken::new(),
                exited,
                run_request: Arc::new(parking_lot::Mutex::new(None)),
                run_turn: Arc::new(parking_lot::Mutex::new(None)),
                model_activity: Arc::new(Mutex::new(None)),
            },
            exited_tx,
        )
    }
    let current = Uuid::new_v4();
    let idle = Uuid::new_v4();
    let active = Uuid::new_v4();
    let (idle_worker, idle_exit) = worker(false);
    let (active_worker, _active_exit) = worker(true);
    let idle_stop = idle_worker.stop.clone();
    let active_stop = active_worker.stop.clone();
    let mut workers = HashMap::from([(idle, idle_worker), (active, active_worker)]);
    retire_idle_workers(&mut workers, current);
    assert!(idle_stop.is_cancelled());
    assert!(
        workers.contains_key(&idle),
        "A still-closing process must remain visible to quiescence"
    );
    assert!(
        !active_stop.is_cancelled(),
        "Opening a chat must preserve other active turns"
    );
    idle_exit.send(true).unwrap();
    retire_idle_workers(&mut workers, current);
    assert!(!workers.contains_key(&idle));
    assert!(workers.contains_key(&active));
}
