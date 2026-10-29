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
    let vault = CredentialVault::new("com.qurkos.xylo.host-tests").unwrap();
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
    let mut settings = AppSettings::default();
    settings.provider = ProviderConfig { family:ApiFamily::Azure, endpoint:"https://resource.openai.azure.com/openai/deployments/explicit-deployment?api-version=2024-10-21".into(), model:"explicit-deployment".into(), ..Default::default() };
    host.handle(HostCommand::SaveProvider(
        generation,
        settings.clone(),
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
        generation, settings, target, None,
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
            generation, settings, target, None,
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
        let mut settings = AppSettings::default();
        settings.theme = "older".into();
        old_host
            .handle(HostCommand::SaveSettings(settings))
            .await
            .unwrap();
    });
    ready.await.unwrap();
    let new_host = host.clone();
    let newer = tokio::spawn(async move {
        let _commit = second.enter().await;
        let mut settings = AppSettings::default();
        settings.theme = "newer".into();
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
            AppSettings::default(),
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
        let mut settings = AppSettings::default();
        settings.theme = "newer-after-error".into();
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
            AppSettings::default(),
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
        let mut settings = AppSettings::default();
        settings.theme = "committed-while-catalog-waits".into();
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
