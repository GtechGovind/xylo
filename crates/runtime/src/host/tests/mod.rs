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

#[tokio::test]
async fn an_unreadable_vault_is_reported_once_at_startup() {
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::at(directory.path().to_owned()).unwrap();
    {
        let vault = CredentialVault::open(&paths.data, SecretStorage::LocalFile).unwrap();
        vault.set("provider", "secret").unwrap();
    }
    std::fs::write(paths.data.join("secrets.vault"), b"XYLOVLT1 tampered").unwrap();
    let start = |paths: AppPaths| async move {
        // The previous host releases the profile lock as its thread exits.
        let (host, events) = loop {
            match HostClient::spawn_at(paths.clone()) {
                Ok(started) => break started,
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        };
        let mut notices = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), async {
            host.shutdown().unwrap();
            loop {
                match events.recv().await.unwrap() {
                    HostEvent::Status(text) => notices.push(text),
                    HostEvent::Stopped => break,
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        notices
    };
    let notices = start(paths.clone()).await;
    let vault_notices = notices
        .iter()
        .filter(|text| text.contains("Saved keys could not be read"))
        .count();
    assert_eq!(vault_notices, 1, "{notices:?}");
    let notices = start(paths).await;
    assert!(
        !notices
            .iter()
            .any(|text| text.contains("Saved keys could not be read")),
        "the replaced vault opens cleanly next time: {notices:?}"
    );
}

pub(super) fn state() -> (tempfile::TempDir, Arc<HostState>, Receiver<HostEvent>) {
    let dir = tempfile::tempdir().unwrap();
    let paths = AppPaths::at(dir.path().to_owned()).unwrap();
    let store = Store::open(&paths.database).unwrap();
    let secrets = open_secrets(&paths.data, SecretStorage::LocalFile, &store).unwrap();
    let accounts = xylo_services::providers::ChatGptSignInService::new(&paths.data).unwrap();
    let (events, receiver) = async_channel::bounded(256);
    let (editor_events, _editor_receiver) = async_channel::bounded(128);
    let lsp = crate::lsp::Service::new(paths.data.clone(), editor_events);
    let (workspace_events, _) = async_channel::bounded(16);
    let host = Arc::new(HostState {
        discovery: Arc::new(provider_control::DiscoveryControl::default()),
        provider_save_gate: Mutex::new(()),
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
                id: "com.qurkos.xylo.host-tests".into(),
                name: "Xylo tests".into(),
            },
        )
        .unwrap(),
        notices: async_channel::bounded(128).0,
        foreground: Arc::new(AtomicBool::new(false)),
        paths,
        memory: SqliteMemory::new(store.clone()),
        accounts,
        secrets: parking_lot::RwLock::new(Ok(secrets)),
        store,
        settings: Mutex::new(AppSettings::default()),
        latest_project_open: std::sync::atomic::AtomicU64::new(0),
        workers: Mutex::new(HashMap::new()),
        sequences: SessionSequences::default(),
        events,
        resources: ResourceManager::new(),
        queued: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        turn_queue: turn_queue_control::TurnQueueControl::default(),
        scheduled: Arc::new(Mutex::new(HashMap::new())),
        stopping: Arc::new(AtomicBool::new(false)),
        helpers: Mutex::new(HashMap::new()),
        capability_requests: Mutex::new(HashMap::new()),
        lsp,
    });
    (dir, host, receiver)
}

/// Delivers a UI Stop for `session` through the intake path.
pub(super) async fn stop(host: &Arc<HostState>, session: Uuid) {
    let stop = host.capture_stop(HostCommand::Worker(Some(session), WorkerCommand::Cancel));
    assert!(host.prepare_command(&stop).await.unwrap());
    host.handle(stop).await.unwrap();
}

/// The next event that is not a turn phase change.
pub(super) async fn next_event(events: &Receiver<HostEvent>) -> HostEvent {
    loop {
        match events.recv().await.unwrap() {
            HostEvent::Engine(Envelope {
                payload: EngineEvent::Phase { .. },
                ..
            }) => {}
            event => return event,
        }
    }
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
        approved_rules: Vec::new(),
    };
    let command = HostCommand::Worker(Some(session), WorkerCommand::Run { turn });
    assert!(host.prepare_command(&command).await.unwrap());
    stop(&host, session).await;
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
async fn idle_workers_stay_warm_until_aged_or_over_capacity_and_active_turns_survive() {
    type Ends = (
        mpsc::Receiver<Envelope<WorkerCommand>>,
        tokio::sync::watch::Sender<bool>,
    );
    fn worker(active: bool, idle_for: Duration) -> (WorkerHandle, Ends) {
        let (commands, incoming) = mpsc::channel(2);
        let (exited_tx, exited) = tokio::sync::watch::channel(false);
        let worker = WorkerHandle::new(commands, exited);
        worker.active.store(active, Ordering::Release);
        *worker.last_used.lock() = std::time::Instant::now() - idle_for;
        (worker, (incoming, exited_tx))
    }
    let now = std::time::Instant::now();
    let current = Uuid::new_v4();
    let active = Uuid::new_v4();
    let (active_worker, _active_exit) = worker(true, worker_control::IDLE_WORKER_TTL * 2);
    let mut workers = HashMap::from([(active, active_worker)]);
    let mut exits = Vec::new();
    let mut recent = Vec::new();
    for minute in 0..worker_control::MAX_IDLE_WORKERS + 1 {
        let session = Uuid::new_v4();
        let (handle, exit) = worker(false, Duration::from_secs(60 * minute as u64));
        workers.insert(session, handle);
        exits.push((session, exit));
        recent.push(session);
    }
    let aged = Uuid::new_v4();
    let (aged_worker, (_aged_commands, aged_exit)) = worker(
        false,
        worker_control::IDLE_WORKER_TTL + Duration::from_secs(1),
    );
    workers.insert(aged, aged_worker);
    retire_idle_workers(&mut workers, Some(current), now);
    let stopped = |workers: &HashMap<Uuid, WorkerHandle>, session: &Uuid| {
        workers[session].stop.is_cancelled()
    };
    assert!(
        stopped(&workers, &aged),
        "idle beyond the time limit is retired"
    );
    let oldest = *recent.last().unwrap();
    assert!(
        stopped(&workers, &oldest),
        "the least recently used beyond the cap is retired"
    );
    for session in &recent[..worker_control::MAX_IDLE_WORKERS] {
        assert!(!stopped(&workers, session), "recent idle workers stay warm");
    }
    assert!(
        !stopped(&workers, &active),
        "a running turn is never retired"
    );
    assert!(
        workers.contains_key(&aged),
        "A still-closing process must remain visible to quiescence"
    );
    aged_exit.send(true).unwrap();
    retire_idle_workers(&mut workers, Some(current), now);
    assert!(!workers.contains_key(&aged));
    assert!(workers.contains_key(&active));
    drop(exits);
}

#[tokio::test]
async fn replacing_provider_discovery_discards_an_older_catalog_response() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (_directory, host, events) = state();
    let (started, ready) = tokio::sync::oneshot::channel();
    let (release, held) = tokio::sync::oneshot::channel();
    let older = Arc::new(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap());
    let old_endpoint = format!("http://{}/v1", older.local_addr().unwrap());
    let newer = older.clone();
    let new_endpoint = old_endpoint.clone();
    let old_server = tokio::spawn(async move {
        let (socket, _) = older.accept().await.unwrap();
        let mut socket = BufReader::new(socket);
        loop {
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            if line == "\r\n" {
                break;
            }
        }
        started.send(()).unwrap();
        let _ = held.await;
        let body = r#"{"data":[{"id":"old-auth-catalog"}]}"#;
        let _ = socket.get_mut().write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
    });
    let old_host = host.clone();
    let old_generation = host.discovery.begin();
    let old_run = tokio::spawn(async move {
        old_host
            .discover_models(
                old_generation,
                ProviderConfig {
                    endpoint: old_endpoint,
                    ..Default::default()
                },
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), ready)
        .await
        .unwrap()
        .unwrap();
    let new_server = tokio::spawn(async move {
        let (socket, _) = newer.accept().await.unwrap();
        let mut socket = BufReader::new(socket);
        loop {
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            if line == "\r\n" {
                break;
            }
        }
        let body = r#"{"data":[{"id":"new-auth-catalog"}]}"#;
        socket.get_mut().write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    });
    host.discover_models(
        host.discovery.begin(),
        ProviderConfig {
            endpoint: new_endpoint,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let _ = release.send(());
    old_run.await.unwrap().unwrap();
    old_server.await.unwrap();
    new_server.await.unwrap();
    let mut catalogs = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let HostEvent::Models { models, .. } = event {
            catalogs.push(models);
        }
    }
    assert_eq!(catalogs, vec![vec!["new-auth-catalog".to_owned()]]);
}

#[tokio::test]
async fn discovery_auth_failure_is_correlated_to_the_selected_provider() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (_directory, host, events) = state();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = BufReader::new(socket);
        loop {
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            if line == "\r\n" {
                break;
            }
        }
        let body = r#"{"error":{"code":"catalog_denied","message":"reflected fixture-secret must not appear"}}"#;
        socket.get_mut().write_all(format!("HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    });
    let generation = host.discovery.begin();
    host.discover_models(
        generation,
        ProviderConfig {
            endpoint: endpoint.clone(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let HostEvent::ModelsFailed {
        generation: received,
        provider,
        message,
    } = events.recv().await.unwrap()
    else {
        panic!("Missing correlated discovery error");
    };
    assert_eq!(received, generation);
    assert_eq!(provider.endpoint, endpoint);
    assert!(message.contains("401") && message.contains("catalog_denied"));
    assert!(!message.contains("fixture-secret"));
    server.await.unwrap();
}

#[test]
fn visible_discovery_cancel_invalidates_the_same_generation_read_by_the_ui() {
    let (commands, _incoming) = async_channel::bounded(1);
    let discovery = Arc::new(provider_control::DiscoveryControl::default());
    let client = HostClient {
        commands,
        discovery: discovery.clone(),
    };
    let old = discovery.begin();
    let token = discovery.token(old).unwrap();
    client.cancel_model_discovery().unwrap();
    assert!(token.is_cancelled());
    assert_ne!(client.provider_discovery_generation(), old);
}

#[tokio::test]
async fn saving_a_manual_deployment_without_catalog_has_a_correlated_terminal_result() {
    let (_directory, host, events) = state();
    let generation = host.discovery.begin();
    let settings = AppSettings {
        provider: ProviderConfig { family:ApiFamily::Azure, endpoint:"https://resource.openai.azure.com/openai/deployments/explicit-deployment?api-version=2024-10-21".into(), model:"explicit-deployment".into(), ..Default::default() },
        ..Default::default()
    };
    host.handle(HostCommand::SaveProvider(
        generation,
        Box::new(settings.clone()),
        settings.provider,
        None,
    ))
    .await
    .unwrap();
    let mut catalogs = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let HostEvent::Models {
            generation,
            provider,
            models,
            ..
        } = event
        {
            catalogs.push((generation, provider.model, models));
        }
    }
    assert_eq!(
        catalogs,
        vec![(
            generation,
            "explicit-deployment".to_owned(),
            Vec::<String>::new()
        )]
    );
}

#[tokio::test]
async fn saving_a_separate_connection_keeps_active_model_until_catalog_selection() {
    let (_directory, host, events) = state();
    let generation = host.discovery.begin();
    let mut settings = AppSettings::default();
    settings.provider.model = "existing-active-model".into();
    let target = ProviderConfig { family:ApiFamily::Azure, endpoint:"https://resource.openai.azure.com/openai/deployments/explicit-deployment?api-version=2024-10-21".into(), model:"".into(), ..Default::default() };
    settings.providers.push(target.clone());
    host.handle(HostCommand::SaveProvider(
        generation,
        Box::new(settings),
        target,
        None,
    ))
    .await
    .unwrap();
    assert_eq!(
        host.settings.lock().await.provider.model,
        "existing-active-model"
    );
    let persisted = host.store.settings().unwrap();
    assert_eq!(persisted.provider.model, "existing-active-model");
    let mut terminal = 0;
    while let Ok(event) = events.try_recv() {
        if let HostEvent::Models {
            generation: observed,
            provider,
            models,
            ..
        } = event
        {
            assert_eq!(observed, generation);
            assert!(provider.model.is_empty());
            assert!(models.is_empty());
            terminal += 1;
        }
    }
    assert_eq!(terminal, 1);
}

#[tokio::test]
async fn separate_connection_catalog_results_preserve_active_and_carry_model_metadata() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    for denied in [false, true] {
        let (_directory, host, events) = state();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            loop {
                let mut line = String::new();
                socket.read_line(&mut line).await.unwrap();
                if line == "\r\n" {
                    break;
                }
            }
            let (status, body) = if denied {
                ("401 Unauthorized", r#"{"error":{"code":"denied"}}"#)
            } else {
                (
                    "200 OK",
                    r#"{"data":[{"id":"future-model","supported_reasoning_levels":[{"effort":"low"},{"effort":"high"}],"default_reasoning_level":"high"}]}"#,
                )
            };
            socket.get_mut().write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        });
        let mut settings = AppSettings::default();
        settings.provider.model = "existing-active-model".into();
        let target = ProviderConfig {
            endpoint,
            ..Default::default()
        };
        settings.providers.push(target.clone());
        let generation = host.discovery.begin();
        host.handle(HostCommand::SaveProvider(
            generation,
            Box::new(settings),
            target,
            None,
        ))
        .await
        .unwrap();
        assert_eq!(
            host.store.settings().unwrap().provider.model,
            "existing-active-model"
        );
        let mut terminal = 0;
        loop {
            let event = tokio::time::timeout(Duration::from_secs(3), events.recv())
                .await
                .unwrap()
                .unwrap();
            match event {
                HostEvent::Models {
                    generation: observed,
                    models,
                    reasoning_options,
                    default_reasoning_efforts,
                    ..
                } => {
                    assert!(!denied);
                    assert_eq!(observed, generation);
                    assert_eq!(models, vec!["future-model"]);
                    assert_eq!(
                        reasoning_options.get("future-model"),
                        Some(&vec!["low".to_owned(), "high".to_owned()])
                    );
                    assert_eq!(
                        default_reasoning_efforts
                            .get("future-model")
                            .map(String::as_str),
                        Some("high")
                    );
                    terminal += 1;
                }
                HostEvent::ModelsFailed {
                    generation: observed,
                    message,
                    ..
                } => {
                    assert!(denied);
                    assert_eq!(observed, generation);
                    assert!(message.contains("401"));
                    terminal += 1;
                }
                _ => {}
            }
            if terminal > 0 {
                break;
            }
        }
        assert_eq!(terminal, 1);
        server.await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receipt_ordered_settings_keep_newer_snapshot_after_a_delayed_first_commit() {
    let (_directory, host, events) = state();
    let order = settings_control::CommitOrder::default();
    let first = order.reserve();
    let second = order.reserve();
    let (started, ready) = tokio::sync::oneshot::channel();
    let (release, resume) = tokio::sync::oneshot::channel();
    let old_host = host.clone();
    let older = tokio::spawn(async move {
        let _commit = first.enter().await;
        let _ = started.send(());
        let _ = resume.await;
        let settings = AppSettings {
            theme: "older".into(),
            ..Default::default()
        };
        old_host
            .handle(HostCommand::SaveSettings(settings))
            .await
            .unwrap();
    });
    ready.await.unwrap();
    let new_host = host.clone();
    let newer = tokio::spawn(async move {
        let _commit = second.enter().await;
        let settings = AppSettings {
            theme: "newer".into(),
            ..Default::default()
        };
        new_host
            .handle(HostCommand::SaveSettings(settings))
            .await
            .unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    release.send(()).unwrap();
    older.await.unwrap();
    newer.await.unwrap();
    assert_eq!(host.store.settings().unwrap().theme, "newer");
    assert_eq!(host.settings.lock().await.theme, "newer");
    let mut observed = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let HostEvent::Settings(settings) = event {
            observed.push(settings.theme);
        }
    }
    assert_eq!(observed, vec!["older", "newer"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_earlier_settings_writer_releases_newer_commit_in_receipt_order() {
    let (_directory, host, events) = state();
    let order = settings_control::CommitOrder::default();
    let first = order.reserve();
    let second = order.reserve();
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, resume) = tokio::sync::oneshot::channel();
    let old = host.clone();
    let older = tokio::spawn(async move {
        let _commit = first.enter().await;
        entered.send(()).unwrap();
        resume.await.unwrap();
        let bad = ProviderConfig {
            context_tokens: 1,
            ..Default::default()
        };
        old.handle(HostCommand::SaveProvider(
            old.discovery.begin(),
            Box::default(),
            bad,
            None,
        ))
        .await
        .unwrap_err();
    });
    started.await.unwrap();
    let new = host.clone();
    let newer = tokio::spawn(async move {
        let _commit = second.enter().await;
        let settings = AppSettings {
            theme: "newer-after-error".into(),
            ..Default::default()
        };
        new.handle(HostCommand::SaveSettings(settings))
            .await
            .unwrap();
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!newer.is_finished());
    release.send(()).unwrap();
    older.await.unwrap();
    newer.await.unwrap();
    assert_eq!(host.store.settings().unwrap().theme, "newer-after-error");
    assert_eq!(host.settings.lock().await.theme, "newer-after-error");
    let mut observed = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let HostEvent::Settings(settings) = event {
            observed.push(settings.theme);
        }
    }
    assert_eq!(observed, vec!["newer-after-error"]);
}

#[tokio::test]
async fn dropping_a_queued_settings_ticket_does_not_overtake_the_active_writer() {
    let order = settings_control::CommitOrder::default();
    let first = order.reserve().enter().await;
    let abandoned = order.reserve();
    let last = order.reserve();
    let pending = tokio::spawn(async move {
        let _commit = abandoned.enter().await;
    });
    tokio::task::yield_now().await;
    pending.abort();
    let _ = pending.await;
    let following = tokio::spawn(async move {
        let _commit = last.enter().await;
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!following.is_finished());
    drop(first);
    tokio::time::timeout(Duration::from_secs(1), following)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_catalog_network_does_not_hold_settings_commit_admission() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (_directory, host, events) = state();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, resume) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = BufReader::new(socket);
        loop {
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            if line == "\r\n" {
                break;
            }
        }
        entered.send(()).unwrap();
        resume.await.unwrap();
        let body = r#"{"data":[{"id":"catalog-model"}]}"#;
        socket.get_mut().write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
    });
    let order = settings_control::CommitOrder::default();
    {
        let _commit = order.reserve().enter().await;
        let target = ProviderConfig {
            endpoint,
            ..Default::default()
        };
        host.handle(HostCommand::SaveProvider(
            host.discovery.begin(),
            Box::default(),
            target,
            None,
        ))
        .await
        .unwrap();
    }
    tokio::time::timeout(Duration::from_secs(2), started)
        .await
        .unwrap()
        .unwrap();
    {
        let _commit = order.reserve().enter().await;
        let settings = AppSettings {
            theme: "committed-while-catalog-waits".into(),
            ..Default::default()
        };
        tokio::time::timeout(
            Duration::from_secs(1),
            host.handle(HostCommand::SaveSettings(settings)),
        )
        .await
        .unwrap()
        .unwrap();
    }
    assert_eq!(
        host.store.settings().unwrap().theme,
        "committed-while-catalog-waits"
    );
    release.send(()).unwrap();
    server.await.unwrap();
    loop {
        if matches!(
            tokio::time::timeout(Duration::from_secs(2), events.recv())
                .await
                .unwrap()
                .unwrap(),
            HostEvent::Models { .. }
        ) {
            break;
        }
    }
}

#[tokio::test]
async fn connector_mutation_ack_is_durable_when_catalog_read_is_corrupt() {
    let (_directory, host, events) = state();
    rusqlite::Connection::open(&host.paths.database)
        .unwrap()
        .execute(
            "INSERT INTO connectors(id,name,json) VALUES('broken','Broken row','invalid JSON')",
            [],
        )
        .unwrap();
    let config = ConnectorConfig {
        id: Uuid::new_v4(),
        name: "Durable fixture".into(),
        transport: "stdio".into(),
        endpoint: String::new(),
        command: Some(PathBuf::from("/usr/bin/true")),
        args: vec!["one".into(), "two".into()],
        credential_ref: None,
        trusted: false,
    };
    let request_id = Uuid::new_v4();
    host.handle(HostCommand::MutateConnector(
        request_id,
        Some(config.clone()),
        None,
        None,
    ))
    .await
    .unwrap();
    match events.recv().await.unwrap() {
        HostEvent::ConnectorMutation {
            request_id: actual,
            result: Ok(receipt),
        } => {
            assert_eq!(actual, request_id);
            assert_eq!(receipt.saved.unwrap().id, config.id);
        }
        _ => panic!("Durable save must acknowledge success independently of catalog decoding"),
    }
    assert!(
        host.store.connectors().is_err(),
        "Fixture must fail catalog refresh"
    );
    let count: i64 = rusqlite::Connection::open(&host.paths.database)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM connectors WHERE id=?1",
            [config.id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn tracked_schedule_failure_is_correlated_and_does_not_claim_success() {
    let (_directory, host, events) = state();
    // An undecodable store is replaced; one over the size budget fails.
    std::fs::write(
        host.paths.artifacts.join("schedules.json"),
        vec![b' '; 2 * 1024 * 1024 + 1],
    )
    .unwrap();
    let request_id = Uuid::new_v4();
    host.handle(HostCommand::CreateScheduleTracked(
        request_id,
        "Fixture task".into(),
        "0 * * * * * *".into(),
        host.paths.data.clone(),
    ))
    .await
    .unwrap();
    match events.recv().await.unwrap() {
        HostEvent::ScheduleMutation {
            request_id: actual,
            result: Err(_),
        } => assert_eq!(actual, request_id),
        _ => panic!("Failed durable save requires correlated failure"),
    }
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn schedule_durable_ack_precedes_failed_list_refresh() {
    let (_directory, host, events) = state();
    // An undecodable store is replaced; one over the size budget fails.
    std::fs::write(
        host.paths.artifacts.join("schedules.json"),
        vec![b' '; 2 * 1024 * 1024 + 1],
    )
    .unwrap();
    let request_id = Uuid::new_v4();
    assert!(host.schedule_mutated(request_id, Ok(())).await.is_err());
    match events.recv().await.unwrap() {
        HostEvent::ScheduleMutation {
            request_id: actual,
            result: Ok(()),
        } => assert_eq!(actual, request_id),
        _ => panic!("Refresh failure must not replace durable success"),
    }
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn connector_failed_write_and_missing_reference_have_redacted_correlated_errors() {
    let (_directory, host, events) = state();
    let config = ConnectorConfig {
        id: Uuid::new_v4(),
        name: "Rejected fixture".into(),
        transport: "http".into(),
        endpoint: "https://example.test/mcp".into(),
        command: None,
        args: vec![],
        credential_ref: None,
        trusted: false,
    };
    let request_id = Uuid::new_v4();
    host.handle(HostCommand::MutateConnector(
        request_id,
        Some(config.clone()),
        None,
        Some("never-log-this-secret".into()),
    ))
    .await
    .unwrap();
    match events.recv().await.unwrap() {
        HostEvent::ConnectorMutation {
            request_id: actual,
            result: Err(error),
        } => {
            assert_eq!(actual, request_id);
            assert!(!error.contains("never-log-this-secret"));
            assert!(error.contains("Missing connector credential reference"));
        }
        _ => panic!("Credential precondition must fail before any platform vault operation"),
    }
    assert!(host.store.connectors().unwrap().is_empty());
    rusqlite::Connection::open(&host.paths.database).unwrap().execute_batch("CREATE TRIGGER reject_connector BEFORE INSERT ON connectors BEGIN SELECT RAISE(ABORT,'fixture write rejected'); END").unwrap();
    let request_id = Uuid::new_v4();
    host.handle(HostCommand::MutateConnector(
        request_id,
        Some(config),
        None,
        None,
    ))
    .await
    .unwrap();
    match events.recv().await.unwrap() {
        HostEvent::ConnectorMutation {
            request_id: actual,
            result: Err(error),
        } => {
            assert_eq!(actual, request_id);
            assert!(error.contains("fixture write rejected"));
        }
        _ => panic!("Failed SQLite write must not acknowledge success"),
    }
    assert!(host.store.connectors().unwrap().is_empty());
}

#[tokio::test]
async fn tracked_schedule_create_update_remove_acknowledge_actual_durable_state() {
    let (_directory, host, events) = state();
    let request_id = Uuid::new_v4();
    host.handle(HostCommand::CreateScheduleTracked(
        request_id,
        "Fixture task".into(),
        "0 0 * * * * *".into(),
        host.paths.data.clone(),
    ))
    .await
    .unwrap();
    assert!(
        matches!(events.recv().await.unwrap(),HostEvent::ScheduleMutation{request_id:actual,result:Ok(())} if actual==request_id)
    );
    let mut task = match events.recv().await.unwrap() {
        HostEvent::Schedules(mut rows) => {
            assert_eq!(rows.len(), 1);
            rows.remove(0)
        }
        _ => panic!("Expected durable task list"),
    };
    assert_eq!(
        host.default_scheduler().unwrap().list().await.unwrap()[0].id,
        task.id
    );
    task.enabled = false;
    let request_id = Uuid::new_v4();
    host.handle(HostCommand::SaveScheduleTracked(request_id, task.clone()))
        .await
        .unwrap();
    assert!(
        matches!(events.recv().await.unwrap(),HostEvent::ScheduleMutation{request_id:actual,result:Ok(())} if actual==request_id)
    );
    assert!(
        matches!(events.recv().await.unwrap(),HostEvent::Schedules(rows) if rows.len()==1 && !rows[0].enabled)
    );
    assert!(!host.default_scheduler().unwrap().list().await.unwrap()[0].enabled);
    let request_id = Uuid::new_v4();
    host.handle(HostCommand::RemoveScheduleTracked(request_id, task.id))
        .await
        .unwrap();
    assert!(
        matches!(events.recv().await.unwrap(),HostEvent::ScheduleMutation{request_id:actual,result:Ok(())} if actual==request_id)
    );
    assert!(matches!(events.recv().await.unwrap(),HostEvent::Schedules(rows) if rows.is_empty()));
    assert!(
        host.default_scheduler()
            .unwrap()
            .list()
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn local_connector_inspection_without_project_is_rejected_before_launch() {
    let (_directory, host, events) = state();
    let config = ConnectorConfig {
        id: Uuid::new_v4(),
        name: "Local fixture".into(),
        transport: "stdio".into(),
        endpoint: String::new(),
        command: Some(PathBuf::from("/usr/bin/true")),
        args: vec![],
        credential_ref: None,
        trusted: false,
    };
    host.store.save_connector(&config).unwrap();
    host.handle(HostCommand::ConnectorTools(Uuid::new_v4(), config.id, None))
        .await
        .unwrap();
    match events.recv().await.unwrap() {
        HostEvent::ConnectorToolsFailed {
            connector_id,
            message,
            ..
        } => {
            assert_eq!(connector_id, config.id);
            assert!(message.contains("Choose a project"));
        }
        _ => panic!("Local inspection requires an explicit captured project"),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn connector_remove_gate_prevents_waiting_inspection_from_launching_deleted_config() {
    let (directory, host, events) = state();
    let workspace = directory.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    let script = workspace.join("must-not-launch.sh");
    let marker = workspace.join("launched");
    std::fs::write(&script, "#!/bin/sh\ntouch launched\nsleep 30\n").unwrap();
    let config = ConnectorConfig {
        id: Uuid::new_v4(),
        name: "Deleted fixture".into(),
        transport: "stdio".into(),
        endpoint: String::new(),
        command: Some(PathBuf::from("/bin/sh")),
        args: vec![script.to_string_lossy().into_owned()],
        credential_ref: None,
        trusted: false,
    };
    host.store.save_connector(&config).unwrap();
    let held = host
        .connectors()
        .unwrap()
        .configuration_guard(config.id)
        .await
        .unwrap();
    let removal_id = Uuid::new_v4();
    let connector_id = config.id;
    let deleting = host.clone();
    let removal = tokio::spawn(async move {
        deleting
            .handle(HostCommand::MutateConnector(
                removal_id,
                None,
                Some(connector_id),
                None,
            ))
            .await
    });
    tokio::task::yield_now().await;
    let inspection_id = Uuid::new_v4();
    let inspecting = host.clone();
    let inspection = tokio::spawn(async move {
        inspecting
            .handle(HostCommand::ConnectorTools(
                inspection_id,
                connector_id,
                Some(workspace),
            ))
            .await
    });
    tokio::task::yield_now().await;
    assert!(!removal.is_finished());
    assert!(!inspection.is_finished());
    drop(held);
    removal.await.unwrap().unwrap();
    inspection.await.unwrap().unwrap();
    assert!(
        matches!(events.recv().await.unwrap(),HostEvent::ConnectorMutation{request_id,result:Ok(_)} if request_id==removal_id)
    );
    assert!(
        matches!(events.recv().await.unwrap(),HostEvent::ConnectorToolsFailed{request_id,message,..} if request_id==inspection_id && message.contains("no longer exists"))
    );
    assert!(
        !marker.exists(),
        "No process may launch from a configuration removed before inspection obtains ownership"
    );
    assert!(host.store.connectors().unwrap().is_empty());
    assert!(
        host.connectors()
            .unwrap()
            .cached_tools(&config)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn undecodable_settings_reset_to_defaults_once() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("xylo.db")).unwrap();
    let saved = AppSettings {
        theme: "dark".into(),
        ..Default::default()
    };
    store.save_settings(&saved).unwrap();
    assert_eq!(startup_settings(&store).unwrap().0.theme, "dark");
    rusqlite::Connection::open(directory.path().join("xylo.db"))
        .unwrap()
        .execute(
            "UPDATE settings SET value='{\"permissions\":\"ask_for_writes\"}' WHERE key='app'",
            [],
        )
        .unwrap();
    let (settings, reset) = startup_settings(&store).unwrap();
    assert!(reset);
    assert_eq!(settings.theme, AppSettings::default().theme);
    let (_, reset) = startup_settings(&store).unwrap();
    assert!(!reset, "defaults were saved, so the notice is one-time");
}

#[tokio::test]
async fn renaming_validates_the_title_and_publishes_the_conversation() {
    let (_dir, host, events) = state();
    let (turn, draft) = draft::input(&host);
    host.store.admit_draft_turn(&turn, &draft).unwrap();
    let id = turn.session_id;
    for invalid in ["   ", &"x".repeat(121)] {
        assert!(
            host.handle(HostCommand::RenameConversation(id, invalid.into()))
                .await
                .is_err()
        );
    }
    host.handle(HostCommand::RenameConversation(
        id,
        "  Release notes  ".into(),
    ))
    .await
    .unwrap();
    let HostEvent::ConversationRenamed(conversation) = events.recv().await.unwrap() else {
        panic!("expected the renamed conversation")
    };
    assert_eq!(conversation.title, "Release notes");
    assert_eq!(conversation.title_source, TitleSource::User);
    assert!(matches!(
        events.recv().await.unwrap(),
        HostEvent::ConversationPage(_)
    ));
    assert!(
        host.handle(HostCommand::RenameConversation(
            Uuid::new_v4(),
            "Missing".into()
        ))
        .await
        .is_err()
    );
}

#[tokio::test]
async fn approval_rules_are_listed_and_revoked_per_project() {
    let (_dir, host, events) = state();
    let workspace = host.paths.data.clone();
    let canonical = std::fs::canonicalize(&workspace).unwrap();
    for key in ["run:cargo test", "write:project"] {
        host.store
            .save_approval_rule(&ApprovalRule {
                key: key.into(),
                label: key.into(),
                workspace: canonical.clone(),
                created_at: now_ms(),
            })
            .unwrap();
    }
    let session = Uuid::new_v4();
    let (_incoming, _exit) = draft::mock_worker(&host, session, 2).await;
    let stop = {
        let workers = host.workers.lock().await;
        *workers[&session].workspace.lock() = Some(canonical.clone());
        workers[&session].stop.clone()
    };
    host.handle(HostCommand::ApprovalRules(workspace.clone()))
        .await
        .unwrap();
    let HostEvent::ApprovalRules {
        result: Ok(rules), ..
    } = events.recv().await.unwrap()
    else {
        panic!("expected the project's rules")
    };
    assert_eq!(rules.len(), 2);
    host.handle(HostCommand::RemoveApprovalRule(
        workspace.clone(),
        "run:cargo test".into(),
    ))
    .await
    .unwrap();
    let HostEvent::ApprovalRules {
        workspace: answered,
        result: Ok(rules),
    } = events.recv().await.unwrap()
    else {
        panic!("expected the remaining rules")
    };
    assert_eq!(answered, workspace);
    assert_eq!(
        rules
            .iter()
            .map(|rule| rule.key.as_str())
            .collect::<Vec<_>>(),
        vec!["write:project"]
    );
    assert!(
        stop.is_cancelled(),
        "an idle worker remembering the revoked rule is retired"
    );
}

#[tokio::test]
async fn removing_a_connection_moves_the_active_model_and_deletes_an_unshared_secret() {
    let (_dir, host, events) = state();
    let connection = |endpoint: &str, model: &str, credential: &str| ProviderConfig {
        name: endpoint.into(),
        endpoint: format!("https://{endpoint}/v1"),
        model: model.into(),
        credential_ref: Some(credential.into()),
        ..Default::default()
    };
    let removed_a = connection("removed.example", "a", "provider:removed");
    let removed_b = connection("removed.example", "b", "provider:removed");
    let kept = connection("kept.example", "c", "provider:kept");
    let settings = AppSettings {
        provider: removed_a.clone(),
        providers: vec![removed_a.clone(), removed_b, kept.clone()],
        ..Default::default()
    };
    host.handle(HostCommand::SaveSettings(settings))
        .await
        .unwrap();
    let _ = events.recv().await.unwrap();
    host.handle(HostCommand::RemoveProvider(removed_a))
        .await
        .unwrap();
    let HostEvent::Settings(settings) = events.recv().await.unwrap() else {
        panic!("expected updated settings")
    };
    assert_eq!(settings.providers.len(), 1);
    assert_eq!(settings.provider.endpoint, kept.endpoint);
    assert_eq!(host.store.settings().unwrap().providers.len(), 1);
    assert!(
        host.handle(HostCommand::RemoveProvider(connection(
            "removed.example",
            "a",
            "provider:removed"
        )))
        .await
        .is_err(),
        "removing an unknown connection is refused"
    );
    host.handle(HostCommand::RemoveProvider(kept))
        .await
        .unwrap();
    let HostEvent::Settings(settings) = events.recv().await.unwrap() else {
        panic!("expected updated settings")
    };
    assert!(settings.providers.is_empty());
    assert_eq!(
        settings.provider.endpoint,
        ProviderConfig::default().endpoint
    );
}

#[tokio::test]
async fn a_vault_unavailable_at_startup_opens_when_settings_are_saved() {
    let (_dir, host, _events) = state();
    *host.secrets.write() = Err("fixture failure".into());
    let error = host.vault().err().unwrap().to_string();
    assert!(error.contains("fixture failure"), "{error}");
    host.handle(HostCommand::SaveSettings(AppSettings::default()))
        .await
        .unwrap();
    assert!(host.vault().is_ok());
    assert!(host.connectors().is_ok());
}

#[tokio::test]
async fn a_failed_storage_switch_restores_the_saved_choice() {
    let (_dir, host, events) = state();
    let keychain = AppSettings {
        secret_storage: SecretStorage::OsKeychain,
        ..Default::default()
    };
    host.store.save_settings(&keychain).unwrap();
    *host.settings.lock().await = keychain.clone();
    // The vault is unavailable and its key path cannot become a file, so
    // switching to the local file fails after the setting was saved.
    *host.secrets.write() = Err("fixture failure".into());
    let key = host.paths.data.join("secrets.key");
    let _ = std::fs::remove_file(&key);
    std::fs::create_dir_all(&key).unwrap();
    let wanted = AppSettings {
        theme: "dark".into(),
        secret_storage: SecretStorage::LocalFile,
        ..Default::default()
    };
    assert!(
        host.handle(HostCommand::SaveSettings(wanted))
            .await
            .is_err()
    );
    let saved = host.store.settings().unwrap();
    assert_eq!(saved.secret_storage, SecretStorage::OsKeychain);
    assert_eq!(saved.theme, "dark", "the rest of the change stands");
    assert_eq!(
        host.settings.lock().await.secret_storage,
        SecretStorage::OsKeychain
    );
    let HostEvent::Settings(published) = events.recv().await.unwrap() else {
        panic!("the restored settings are published")
    };
    assert_eq!(published.secret_storage, SecretStorage::OsKeychain);
}

#[test]
fn startup_settings_follow_the_storage_the_vault_records() {
    // A reset database asks for another storage; the vault's own record wins
    // and the saved setting is corrected, so no saved key is stranded.
    let directory = tempfile::tempdir().unwrap();
    let paths = AppPaths::at(directory.path().to_owned()).unwrap();
    let store = Store::open(&paths.database).unwrap();
    CredentialVault::open(&paths.data, SecretStorage::LocalFile)
        .unwrap()
        .set("provider", "secret")
        .unwrap();
    let mut settings = AppSettings {
        secret_storage: SecretStorage::OsKeychain,
        ..Default::default()
    };
    store.save_settings(&settings).unwrap();
    let secrets = open_secrets(&paths.data, settings.secret_storage, &store).unwrap();
    assert_eq!(secrets.storage, SecretStorage::LocalFile);
    assert_eq!(
        secrets.vault.get("provider").unwrap().as_deref(),
        Some("secret")
    );
    follow_vault_storage(&store, &mut settings, &paths.data, Some(&secrets)).unwrap();
    assert_eq!(settings.secret_storage, SecretStorage::LocalFile);
    assert_eq!(
        store.settings().unwrap().secret_storage,
        SecretStorage::LocalFile
    );

    // An unavailable vault still reports the storage it records.
    let mut settings = AppSettings {
        secret_storage: SecretStorage::OsKeychain,
        ..Default::default()
    };
    follow_vault_storage(&store, &mut settings, &paths.data, None).unwrap();
    assert_eq!(settings.secret_storage, SecretStorage::LocalFile);
}

#[tokio::test]
async fn a_reopened_vault_that_keeps_its_key_elsewhere_corrects_the_setting() {
    let (_dir, host, events) = state();
    host.vault().unwrap().set("provider", "secret").unwrap();
    *host.secrets.write() = Err("fixture failure".into());
    let wanted = AppSettings {
        secret_storage: SecretStorage::OsKeychain,
        ..Default::default()
    };
    host.handle(HostCommand::SaveSettings(wanted))
        .await
        .unwrap();
    assert_eq!(
        host.vault().unwrap().get("provider").unwrap().as_deref(),
        Some("secret"),
        "the vault opened where it records its key; nothing was discarded"
    );
    assert_eq!(
        host.store.settings().unwrap().secret_storage,
        SecretStorage::LocalFile
    );
    let HostEvent::Settings(published) = events.recv().await.unwrap() else {
        panic!("the corrected settings are published")
    };
    assert_eq!(published.secret_storage, SecretStorage::LocalFile);
}

pub(super) mod draft;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_snapshot_received_before_a_project_open_cannot_undo_it() {
    let (directory, host, _events) = state();
    let project = directory.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let project = project.canonicalize().unwrap();
    // The open (received 2nd) is applied before the older snapshot (received 1st)
    // commits, because project opens run outside the settings commit order.
    let _ = settings_control::with_receipt(2, host.handle(HostCommand::Workspace(project.clone())))
        .await;
    let stale = AppSettings {
        theme: "from-older-snapshot".into(),
        ..Default::default()
    };
    settings_control::with_receipt(1, host.handle(HostCommand::SaveSettings(stale)))
        .await
        .unwrap();
    let saved = host.store.settings().unwrap();
    assert_eq!(saved.theme, "from-older-snapshot");
    assert_eq!(saved.last_workspace.as_ref(), Some(&project));
    assert!(saved.ui.open_projects.contains(&project));
    assert_eq!(
        host.settings.lock().await.last_workspace.as_ref(),
        Some(&project)
    );
    // A snapshot sent after the open decides the project fields again, so
    // leaving the project still works.
    let left = AppSettings {
        ui: saved.ui.clone(),
        ..Default::default()
    };
    settings_control::with_receipt(3, host.handle(HostCommand::SaveSettings(left)))
        .await
        .unwrap();
    assert_eq!(host.store.settings().unwrap().last_workspace, None);
}

#[test]
fn host_derived_settings_commit_keeps_live_project_fields() {
    let live = AppSettings {
        last_workspace: Some("/projects/new".into()),
        ui: xylo_core::UiPreferences {
            open_projects: vec!["/projects/old".into(), "/projects/new".into()],
            ..Default::default()
        },
        ..Default::default()
    };
    // remove_provider copies settings, then commits after an open may have landed.
    let mut copy = AppSettings {
        theme: "changed".into(),
        last_workspace: Some("/projects/old".into()),
        ui: xylo_core::UiPreferences {
            open_projects: vec!["/projects/old".into()],
            ..Default::default()
        },
        ..Default::default()
    };
    settings_control::merge_project_fields(
        &mut copy,
        &live,
        settings_control::ProjectFields::Current,
        Some(5),
        0,
    );
    assert_eq!(copy.theme, "changed");
    assert_eq!(copy.last_workspace, live.last_workspace);
    assert_eq!(copy.ui.open_projects, live.ui.open_projects);
}
