use super::*;
use xylo_core::{Conversation, ProviderConfig, now_ms};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_native_connector_writes_do_not_create_uncertain_actions_or_approvals() {
    let (_directory, broker, mut events) = fixture(PermissionMode::AskForWrites);
    let cases = [
        ("gmail", "gmail_create_draft", json!({"raw_email":42})),
        ("gmail", "gmail_create_draft", json!({"raw_email":""})),
        (
            "google_calendar",
            "calendar_create_event",
            json!({"event":[]}),
        ),
        (
            "google_calendar",
            "calendar_create_event",
            json!({"event":{"summary":"missing time"}}),
        ),
        (
            "github",
            "github_create_issue",
            json!({"owner":"o","repo":"r","title":"x","unexpected":true}),
        ),
        (
            "github",
            "github_create_issue",
            json!({"owner":"..","repo":"r","title":"x"}),
        ),
        (
            "slack",
            "slack_send_message",
            json!({"channel":"C1","text":42}),
        ),
    ];
    let execution = broker.clone();
    tokio::task::spawn_blocking(move || {
        for (index, (transport, tool, arguments)) in cases.into_iter().enumerate() {
            let id = Uuid::new_v4();
            execution
                .store
                .save_connector(&ConnectorConfig {
                    id,
                    name: "No-account fixture".into(),
                    transport: transport.into(),
                    endpoint: "http://127.0.0.1:1".into(),
                    command: None,
                    args: Vec::new(),
                    credential_ref: None,
                })
                .unwrap();
            let key = format!("invalid-native-{index}");
            let error = execution
                .execute(
                    &key,
                    "connector_call",
                    json!({"connector_id":id,"tool":tool,"arguments":arguments}),
                )
                .unwrap_err()
                .to_string();
            assert!(
                !error.contains("credential") && !error.contains("HTTP"),
                "{error}"
            );
            assert!(
                execution.journaled(&key).unwrap().is_none(),
                "Rejected input must not leave an uncertain action"
            );
        }
    })
    .await
    .unwrap();
    assert!(
        broker
            .store
            .unresolved_actions(broker.request.session_id)
            .unwrap()
            .is_empty()
    );
    assert!(
        events.try_recv().is_err(),
        "Pure preflight must not emit execution or approval events"
    );
}

pub(super) fn fixture(
    mode: PermissionMode,
) -> (
    tempfile::TempDir,
    Arc<ToolBroker>,
    tokio::sync::mpsc::Receiver<EngineEvent>,
) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("xylo.db")).unwrap();
    let session = Uuid::new_v4();
    store
        .upsert_conversation(&Conversation {
            id: session,
            title: "Fixture".into(),
            workspace: directory.path().into(),
            updated_at: now_ms(),
        })
        .unwrap();
    let request = TurnRequest {
        session_id: session,
        turn_id: Uuid::new_v4(),
        prompt: "fixture".into(),
        workspace: directory.path().into(),
        provider: ProviderConfig::default(),
        attachments: Vec::new(),
        permissions: mode,
    };
    let (events, receiver) = tokio::sync::mpsc::channel(256);
    let broker = ToolBroker::new(
        request,
        store,
        CapabilityConfig {
            workspace: directory.path().into(),
            artifacts: directory.path().join("artifacts"),
            management_root: Some(directory.path().join("native-management")),
            ..Default::default()
        },
        ApprovalGate::default(),
        events,
        CancellationToken::new(),
    )
    .unwrap();
    (directory, Arc::new(broker), receiver)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_dispatch_cannot_invoke_application_management_controls() {
    let (_directory, broker, _events) = fixture(PermissionMode::AskForWrites);
    for name in [
        "helper_plugin_install",
        "helper_plugin_set_grants",
        "helper_plugin_remove",
        "helper_environment_save",
        "helper_environment_connect",
        "helper_environment_remove",
    ] {
        assert!(broker.definitions().iter().all(|tool| tool.name != name));
    }
    tokio::task::spawn_blocking(move || {
        for name in ["helper_plugin_install", "helper_environment_save"] {
            for alias in [false, true] {
                let key = format!("management-{name}-{alias}");
                let result = if alias {
                    broker.execute(&key, "call_tool", json!({"name": name, "arguments": {}}))
                } else {
                    broker.execute(&key, name, json!({}))
                };
                assert!(result.unwrap_err().to_string().contains("Unknown tool"));
                assert!(broker.journaled(&key).unwrap().is_none());
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installed_extension_and_environment_execution_obey_read_only_tasks() {
    let (_directory, broker, _events) = fixture(PermissionMode::ReadOnly);
    tokio::task::spawn_blocking(move || {
        for name in ["plugin_run_installed", "environment_execute"] {
            assert!(
                broker
                    .definitions()
                    .iter()
                    .find(|tool| tool.name == name)
                    .unwrap()
                    .writes
            );
            let key = format!("read-only-{name}");
            let error = broker.execute(&key, name, json!({})).unwrap_err();
            assert!(error.to_string().contains("requires write permission"));
            assert!(broker.journaled(&key).unwrap().is_none());
        }
    })
    .await
    .unwrap();
}
#[test]
fn keys_are_namespaced_and_unambiguous() {
    assert_ne!(
        ToolBroker::journal_key(None, "child:1:x:y"),
        ToolBroker::journal_key(Some("x"), "y")
    );
    assert_ne!(
        ToolBroker::journal_key(Some("a:b"), "c"),
        ToolBroker::journal_key(Some("a"), "b:c")
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn children_cannot_delegate_directly_or_through_dispatch_alias() {
    let (_directory, broker, _events) = fixture(PermissionMode::AskForWrites);
    let result = tokio::task::spawn_blocking(move || {
        assert!(
            broker
                .execute_child(
                    "parent-a",
                    "same-id",
                    "agent_delegate",
                    json!({"task":"child"})
                )
                .is_err()
        );
        assert!(
            broker
                .execute_child(
                    "parent-a",
                    "same-id",
                    "call_tool",
                    json!({"name":"agent_delegate","arguments":{"task":"child"}})
                )
                .is_err()
        );
        broker
            .store
            .unresolved_actions(broker.request.session_id)
            .unwrap()
    })
    .await
    .unwrap();
    assert!(result.is_empty());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_call_identity_does_not_reuse_sibling_result() {
    let (directory, broker, _events) = fixture(PermissionMode::ReadOnly);
    std::fs::write(directory.path().join("value.txt"), "first").unwrap();
    let task = broker.clone();
    tokio::task::spawn_blocking(move || {
        task.execute_child(
            "parent-a",
            "same-id",
            "file_read",
            json!({"path":"value.txt"}),
        )
    })
    .await
    .unwrap()
    .unwrap();
    std::fs::write(directory.path().join("value.txt"), "second").unwrap();
    let task = broker.clone();
    let second = tokio::task::spawn_blocking(move || {
        task.execute_child(
            "parent-b",
            "same-id",
            "file_read",
            json!({"path":"value.txt"}),
        )
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&second).unwrap()["summary"]["content"],
        "     1\tsecond\n"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execution_error_after_running_always_finishes_the_journal_state() {
    let (_directory, broker, _events) = fixture(PermissionMode::AskForWrites);
    let task = broker.clone();
    assert!(
        tokio::task::spawn_blocking(move || task.execute(
            "delegate-no-handler",
            "agent_delegate",
            json!({"task":"missing executor"})
        ))
        .await
        .unwrap()
        .is_err()
    );
    let action = broker.journaled("delegate-no-handler").unwrap().unwrap();
    assert_eq!(action.state, "failed");
    assert!(
        broker
            .store
            .unresolved_actions(broker.request.session_id)
            .unwrap()
            .is_empty()
    );
}
fn needles(directory: &std::path::Path) {
    let body: String = (0..2000)
        .map(|i| format!("needle {i} {}\n", "z".repeat(120)))
        .collect();
    std::fs::write(directory.join("large.txt"), body).unwrap();
}
async fn run(broker: &Arc<ToolBroker>, call: &str, name: &str, arguments: Value) -> Result<String> {
    let task = broker.clone();
    let (call, name) = (call.to_owned(), name.to_owned());
    tokio::task::spawn_blocking(move || task.execute(&call, &name, arguments))
        .await
        .unwrap()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn presentation_failure_does_not_erase_a_completed_result() {
    let (directory, broker, _events) = fixture(PermissionMode::ReadOnly);
    needles(directory.path());
    std::fs::remove_dir_all(&broker.artifacts).unwrap();
    let task = broker.clone();
    let output = tokio::task::spawn_blocking(move || {
        task.execute(
            "search-big",
            "search_project",
            json!({"query":"needle","path":"large.txt","max_results":2000}),
        )
    })
    .await
    .unwrap()
    .unwrap();
    let value = serde_json::from_str::<Value>(&output).unwrap();
    assert!(value.get("presentation_error").is_some());
    assert!(
        value["summary"]["results"]
            .as_str()
            .unwrap()
            .contains("needle 0 ")
    );
    let action = broker.journaled("search-big").unwrap().unwrap();
    assert_eq!(action.state, "succeeded");
    assert_eq!(action.result.unwrap()["summary"]["matches"], 2000);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_results_reach_the_model_while_events_stay_small() {
    let (directory, broker, mut events) = fixture(PermissionMode::ReadOnly);
    needles(directory.path());
    let task = broker.clone();
    let output = tokio::task::spawn_blocking(move || {
        task.execute("read-big", "file_read", json!({"path":"large.txt"}))
    })
    .await
    .unwrap()
    .unwrap();
    let value = serde_json::from_str::<Value>(&output).unwrap();
    let content = value["summary"]["content"].as_str().unwrap();
    assert!(content.starts_with("     1\tneedle 0 "));
    assert_eq!(value["summary"]["truncated"], true);
    let next = value["summary"]["next_offset"].as_u64().unwrap();
    while let Ok(event) = events.try_recv() {
        if let EngineEvent::ToolFinished { result, .. } = event {
            assert!(result.len() <= 8192);
        }
    }
    let rest = run(
        &broker,
        "read-rest",
        "file_read",
        json!({"path":"large.txt","offset":next}),
    )
    .await
    .unwrap();
    assert!(rest.contains(&format!("needle {} ", next - 1)));
    // Oversized observations keep both ends and a readable saved full result.
    let search = run(
        &broker,
        "search-big",
        "search_project",
        json!({"query":"needle","path":"large.txt","max_results":2000}),
    )
    .await
    .unwrap();
    assert!(search.len() <= 64 * 1024);
    let search: Value = serde_json::from_str(&search).unwrap();
    assert_eq!(search["truncated"], true);
    let full = search["full_result"].as_str().unwrap().to_owned();
    let saved: Value = serde_json::from_str(
        &run(
            &broker,
            "read-full",
            "file_read",
            json!({"path":full.clone()}),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert!(
        saved["summary"]["content"]
            .as_str()
            .unwrap()
            .contains("needle 0 ")
    );
    let total = saved["summary"]["total_lines"].as_u64().unwrap();
    assert!(total > 2000);
    let end = run(
        &broker,
        "read-full-end",
        "file_read",
        json!({"path":full,"offset":total - 3}),
    )
    .await
    .unwrap();
    assert!(end.contains("needle 1999"), "{end}");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failing_commands_are_definitive_observations_with_their_output() {
    let (directory, broker, _events) = fixture(PermissionMode::Auto);
    let _ = directory;
    let error = run(
        &broker,
        "fails",
        "shell_command",
        json!({"command":"git definitely-not-a-subcommand"}),
    )
    .await
    .unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("\"exit_code\":1"), "{text}");
    assert!(text.contains("definitely-not-a-subcommand"), "{text}");
    assert_eq!(broker.journaled("fails").unwrap().unwrap().state, "failed");
    let missing = run(
        &broker,
        "missing",
        "run_command",
        json!({"program":"definitely-not-a-real-program-xyz","args":[]}),
    )
    .await
    .unwrap_err();
    assert!(format!("{missing:#}").contains("was not found"));
    assert_eq!(
        broker.journaled("missing").unwrap().unwrap().state,
        "failed"
    );
    let ok = run(
        &broker,
        "works",
        "run_command",
        json!({"program":"git","args":["--version"]}),
    )
    .await
    .unwrap();
    assert!(ok.contains("git version"));
    assert!(
        broker
            .store
            .unresolved_actions(broker.request.session_id)
            .unwrap()
            .is_empty()
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_edits_apply_without_prompts_and_inapplicable_edits_leave_no_intent() {
    let (directory, broker, mut events) = fixture(PermissionMode::AcceptEdits);
    std::fs::write(directory.path().join("lib.rs"), "fn a() {}\n").unwrap();
    let output = run(
        &broker,
        "edit",
        "file_edit",
        json!({"path":"lib.rs","old_string":"fn a()","new_string":"fn b()"}),
    )
    .await
    .unwrap();
    assert!(output.contains("+fn b() {}"));
    assert_eq!(
        std::fs::read_to_string(directory.path().join("lib.rs")).unwrap(),
        "fn b() {}\n"
    );
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event, EngineEvent::ApprovalRequired { .. }));
    }
    assert!(
        run(
            &broker,
            "edit-missing",
            "file_edit",
            json!({"path":"lib.rs","old_string":"absent","new_string":"x"}),
        )
        .await
        .is_err()
    );
    assert!(broker.journaled("edit-missing").unwrap().is_none());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn always_allow_covers_matching_actions_for_the_session() {
    let (directory, broker, mut events) = fixture(PermissionMode::AskForWrites);
    let task = broker.clone();
    let first = tokio::task::spawn_blocking(move || {
        task.execute(
            "write",
            "file_write",
            json!({"path":"notes.txt","content":"one\n"}),
        )
    });
    while let Some(event) = events.recv().await {
        if let EngineEvent::ApprovalRequired {
            action_id,
            arguments,
            rule,
            ..
        } = event
        {
            assert_eq!(rule.as_deref(), Some("file edits in this project"));
            assert!(arguments["diff"].as_str().unwrap().contains("+one"));
            broker.approvals.decide_with(action_id, true, true).unwrap();
            break;
        }
    }
    first.await.unwrap().unwrap();
    let task = broker.clone();
    tokio::task::spawn_blocking(move || {
        task.execute(
            "edit",
            "file_edit",
            json!({"path":"notes.txt","old_string":"one","new_string":"two"}),
        )
    })
    .await
    .unwrap()
    .unwrap();
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event, EngineEvent::ApprovalRequired { .. }));
    }
    assert_eq!(
        std::fs::read_to_string(directory.path().join("notes.txt")).unwrap(),
        "two\n"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_connector_write_classification_cannot_be_overridden_by_arguments() {
    let (_directory, broker, _events) = fixture(PermissionMode::ReadOnly);
    let connector = ConnectorConfig {
        id: Uuid::new_v4(),
        name: "Fixture".into(),
        transport: "github".into(),
        endpoint: String::new(),
        command: None,
        args: Vec::new(),
        credential_ref: None,
    };
    broker.store.save_connector(&connector).unwrap();
    let id = connector.id;
    let task = broker.clone();
    let error=tokio::task::spawn_blocking(move||task.execute("write","connector_call",json!({"connector_id":id,"tool":"github_create_issue","arguments":{"owner":"o","repo":"r","title":"t"},"writes":false}))).await.unwrap().unwrap_err();
    assert!(error.to_string().contains("read-only"));
    assert!(broker.journaled("write").unwrap().is_none());
    let task = broker.clone();
    let error=tokio::task::spawn_blocking(move||task.execute("read","connector_call",json!({"connector_id":id,"tool":"github_search_issues","arguments":{"query":"repo:o/r"}}))).await.unwrap().unwrap_err();
    assert!(error.to_string().contains("credential"));
    assert_eq!(broker.journaled("read").unwrap().unwrap().state, "failed");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_observation_is_not_journaled_as_success() {
    let (_directory, broker, _events) = fixture(PermissionMode::AskForWrites);
    broker.set_delegate(Arc::new(|_, _| {
        Ok(json!({"status":"failed","error":"fixture provider failure"}))
    }));
    let task = broker.clone();
    assert!(
        tokio::task::spawn_blocking(move || task.execute(
            "delegate-failure",
            "agent_delegate",
            json!({"task":"fixture"})
        ))
        .await
        .unwrap()
        .is_err()
    );
    assert_eq!(
        broker.journaled("delegate-failure").unwrap().unwrap().state,
        "failed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uncertain_observation_is_retained_and_never_replayed_as_success() {
    let (_directory, broker, mut events) = fixture(PermissionMode::AskForWrites);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = calls.clone();
    let observation = json!({"status":"outcome_unknown","summary":"x".repeat(20_000),
        "artifacts":["partial.txt"]});
    let output = observation.clone();
    broker.set_delegate(Arc::new(move |_, _| {
        count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(output.clone())
    }));
    let task = broker.clone();
    tokio::task::spawn_blocking(move || {
        assert!(
            task.execute("uncertain", "agent_delegate", json!({"task":"fixture"}))
                .is_err()
        );
        assert!(
            task.execute("uncertain", "agent_delegate", json!({"task":"fixture"}))
                .is_err()
        );
    })
    .await
    .unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    let action = broker.journaled("uncertain").unwrap().unwrap();
    assert_eq!(action.state, "outcome_unknown");
    assert_eq!(action.result, Some(observation));
    let mut finished = None;
    while let Ok(event) = events.try_recv() {
        if let EngineEvent::ToolFinished {
            result, success, ..
        } = event
        {
            assert!(!success);
            assert!(result.len() <= 8192);
            finished = Some(serde_json::from_str::<Value>(&result).unwrap());
        }
    }
    let presented = finished.unwrap();
    assert_eq!(presented["status"], "outcome_unknown");
    assert!(
        presented["artifacts"]
            .as_array()
            .unwrap()
            .contains(&json!("partial.txt"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_plugin_partial_write_keeps_structured_artifacts_in_journal_and_event() {
    let started = std::time::Instant::now();
    let (directory, broker, mut events) = fixture(PermissionMode::AskForWrites);
    eprintln!(
        "plugin fixture setup completed at {:?}; hardware {:?}",
        started.elapsed(),
        broker.resources.snapshot().unwrap()
    );
    let path = directory.path().join("partial.wat");
    std::fs::write(&path, r#"(component
      (import "write-text" (func $write (param "name" string) (param "text" string) (result (result string (error string)))))
      (core module $memory
        (memory (export "memory") 1)
        (data (i32.const 512) "partial")
        (func (export "realloc") (param i32 i32 i32 i32) (result i32) i32.const 2048))
      (core instance $mem (instantiate $memory))
      (core func $write-core (canon lower (func $write)
        (memory (core memory $mem "memory")) (realloc (core func $mem "realloc"))))
      (core module $wrapper
        (import "host" "write" (func $write (param i32 i32 i32 i32 i32)))
        (func (export "run") (param i32 i32) (result i32)
          i32.const 512 i32.const 7 local.get 0 local.get 1 i32.const 16 call $write
          unreachable i32.const 16))
      (core instance $wrapper-instance (instantiate $wrapper
        (with "host" (instance (export "write" (func $write-core))))))
      (func (export "run") (param "input" string) (result (result string (error string)))
        (canon lift (core func $wrapper-instance "run")
          (memory (core memory $mem "memory")) (realloc (core func $mem "realloc")))))"#).unwrap();
    let args = json!({"path":path,"input":"Committed before guest failure","capabilities":["write_artifacts"]});
    let task = broker.clone();
    let mut run =
        tokio::task::spawn_blocking(move || task.execute("partial-write", "plugin_run", args));
    let mut checkpoints = tokio::time::interval(Duration::from_secs(1));
    let finished = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let event = tokio::select! {
                event = events.recv() => event.unwrap(),
                _ = checkpoints.tick() => {
                    let management = directory.path().join("native-management");
                    let admission = management.join("admission");
                    let ledger: Vec<_> = std::fs::read_dir(&admission).into_iter().flatten().filter_map(Result::ok).filter(|entry| entry.path().extension().is_some_and(|extension| extension == "json")).map(|entry| std::fs::read_to_string(entry.path()).unwrap_or_else(|error| error.to_string())).collect();
                    let compile_lock = management.join("plugins/compile.lock");
                    let compile = match std::fs::OpenOptions::new().read(true).write(true).open(compile_lock) {
                        Ok(file) => match fs2::FileExt::try_lock_exclusive(&file) { Ok(()) => "unlocked", Err(_) => "held" },
                        Err(_) => "not-created",
                    };
                    eprintln!("plugin checkpoint {:?}: journal={:?}, local-hardware={:?}, shared={ledger:?}, compile={compile}, artifact={}, run_finished={}", started.elapsed(), broker.journaled("partial-write").unwrap().map(|action| action.state), broker.resources.snapshot().unwrap(), broker.artifacts.join("partial.txt").exists(), run.is_finished());
                    continue;
                }
            };
            match event {
                EngineEvent::ApprovalRequired { action_id, .. } => {
                    eprintln!("plugin approval at {:?}", started.elapsed());
                    broker.approvals.decide(action_id, true).unwrap()
                }
                EngineEvent::ToolFinished {
                    result, success, ..
                } => {
                    eprintln!("plugin ToolFinished at {:?}: success={success}", started.elapsed());
                    assert!(!success);
                    break serde_json::from_str::<Value>(&result).unwrap_or_else(|error| {
                        panic!("Plugin event was not structured: {error}: {result}")
                    });
                }
                _ => {}
            }
        }
    })
    .await;
    let finished = match finished {
        Ok(finished) => finished,
        Err(error) => {
            broker.cancel.cancel();
            let cleanup = tokio::time::timeout(Duration::from_secs(5), &mut run).await;
            panic!(
                "Plugin fixture timed out at {:?}: {error}; cancellation cleanup={cleanup:?}",
                started.elapsed()
            );
        }
    };
    assert!(run.await.unwrap().is_err());
    let artifact = broker.artifacts.join("partial.txt").canonicalize().unwrap();
    assert_eq!(
        std::fs::read_to_string(&artifact).unwrap(),
        "Committed before guest failure"
    );
    assert!(
        finished["artifacts"]
            .as_array()
            .unwrap()
            .contains(&json!(artifact))
    );
    let action = broker.journaled("partial-write").unwrap().unwrap();
    assert_eq!(action.state, "outcome_unknown");
    assert_eq!(action.result.as_ref().unwrap()["status"], "failed");
    assert_eq!(action.result.unwrap()["artifacts"], json!([artifact]));
}

#[tokio::test]
async fn bounded_presentations_preserve_status_refs_and_replay_without_overwrite() {
    let (_directory, broker, _events) = fixture(PermissionMode::ReadOnly);
    for length in [20_000, 100_000] {
        let id = Uuid::new_v4();
        let result = json!({"status":"failed","summary":"Ã©ðŸª¶".repeat(length),
            "artifacts":["original.txt"]});
        let first = broker.bound_result(id, result.clone());
        let replay = broker.bound_result(id, result.clone());
        assert_eq!(first, replay);
        assert_eq!(first["status"], "failed");
        assert!(serde_json::to_vec(&first).unwrap().len() <= 8192);
        assert!(
            first["artifacts"]
                .as_array()
                .unwrap()
                .contains(&json!("original.txt"))
        );
        let saved: Value = serde_json::from_slice(
            &std::fs::read(broker.artifacts.join(format!("tool-{id}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(saved, result);
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn denied_write_never_touches_the_file() {
    let (directory, broker, mut events) = fixture(PermissionMode::AskForWrites);
    let task = broker.clone();
    let running = tokio::task::spawn_blocking(move || {
        task.execute(
            "write-denied",
            "file_write",
            json!({"path":"never.txt","content":"denied"}),
        )
    });
    while let Some(event) = events.recv().await {
        if let EngineEvent::ApprovalRequired { action_id, .. } = event {
            broker.approvals.decide(action_id, false).unwrap();
            break;
        }
    }
    assert!(running.await.unwrap().is_err());
    assert!(!directory.path().join("never.txt").exists());
    assert_eq!(
        broker.journaled("write-denied").unwrap().unwrap().state,
        "denied"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retrieved_verified_command_remains_advisory_and_cannot_bypass_read_only_policy() {
    let (_directory, broker, _receiver) = fixture(PermissionMode::ReadOnly);
    let learning = LearningService::new(broker.store.clone());
    for _ in 0..2 {
        let mut request = broker.request.clone();
        request.turn_id = Uuid::new_v4();
        request.prompt = "Fixture Rust verification".into();
        broker.store.register_user_turn(&request).unwrap();
        let intent = ActionIntent {
            id: Uuid::new_v4(),
            session_id: request.session_id,
            turn_id: request.turn_id,
            name: "run_command".into(),
            arguments: json!({"program":"cargo","args":["check","--offline"]}),
            writes: true,
            idempotency_key: "verified".into(),
        };
        broker.store.begin_action(&intent).unwrap();
        broker.store.mark_action_running(intent.id).unwrap();
        broker
            .store
            .finish_action(intent.id, &json!({"status":"success","summary":{"code":0}}))
            .unwrap();
        broker
            .store
            .complete_turn(
                request.session_id,
                request.turn_id,
                &json!([]),
                "Verified",
                &xylo_core::Usage::default(),
            )
            .unwrap();
        learning
            .process_pending(4, &CancellationToken::new())
            .await
            .unwrap();
    }
    let context = broker.project_skill_context().unwrap();
    assert!(context.contains("cargo"));
    assert!(context.contains("current approval required"));
    let execute = broker.clone();
    let error = tokio::task::spawn_blocking(move || {
        execute.execute(
            "attempt-reuse",
            "run_command",
            json!({"program":"cargo","args":["check","--offline"]}),
        )
    })
    .await
    .unwrap()
    .unwrap_err();
    assert!(error.to_string().contains("read-only"));
    assert!(
        broker
            .store
            .action_for_key(
                broker.request.session_id,
                broker.request.turn_id,
                &ToolBroker::journal_key(None, "attempt-reuse")
            )
            .unwrap()
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_turn_advertises_no_writing_tools() {
    let (_directory, broker, _events) = fixture(PermissionMode::ReadOnly);
    assert!(broker.initial_definitions().iter().all(|tool| !tool.writes));
    assert!(
        broker
            .initial_definitions()
            .iter()
            .any(|tool| tool.name == "file_read")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unconfigured_image_provider_is_not_advertised_by_tool_search() {
    let (_directory, broker, _events) = fixture(PermissionMode::AskForWrites);
    tokio::task::spawn_blocking(move || {
        let result = broker
            .execute(
                "discover-images",
                "search_tools",
                json!({"query":"image_generate"}),
            )
            .unwrap();
        let value: Value = serde_json::from_str(&result).unwrap();
        assert!(!value.to_string().contains("Generate one image"), "{value}");
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connector_discovery_snapshot_never_grants_execution_after_removal() {
    let (directory, broker, _events) = fixture(PermissionMode::AskForWrites);
    assert!(
        !broker
            .advertised
            .iter()
            .any(|tool| tool.name == "connector_tools")
    );
    let id = Uuid::new_v4();
    broker
        .store
        .save_connector(&ConnectorConfig {
            id,
            name: "Fixture connection".into(),
            transport: "gmail".into(),
            endpoint: "https://gmail.googleapis.com".into(),
            command: None,
            args: vec![],
            credential_ref: None,
        })
        .unwrap();
    assert!(
        !broker
            .advertised
            .iter()
            .any(|tool| tool.name == "connector_tools")
    );
    let (events, _receiver) = tokio::sync::mpsc::channel(16);
    let next = ToolBroker::new(
        broker.request.clone(),
        broker.store.clone(),
        CapabilityConfig {
            workspace: directory.path().into(),
            artifacts: directory.path().join("artifacts"),
            management_root: Some(directory.path().join("native-management")),
            ..Default::default()
        },
        ApprovalGate::default(),
        events,
        CancellationToken::new(),
    )
    .unwrap();
    assert!(
        next.advertised
            .iter()
            .any(|tool| tool.name == "connector_tools")
    );
    broker.store.delete_connector(id).unwrap();
    tokio::task::spawn_blocking(move || {
        assert!(
            next.execute(
                "removed-connector",
                "connector_tools",
                json!({"connector_id": id})
            )
            .is_err()
        );
        assert!(next.journaled("removed-connector").unwrap().is_none());
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn independent_delegates_enter_callbacks_before_either_finishes() {
    let (_directory, broker, _events) = fixture(PermissionMode::ReadOnly);
    let (entered, mut arrivals) = tokio::sync::mpsc::channel(2);
    let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let waiting = release.clone();
    broker.set_delegate(Arc::new(move |_, _| {
        entered.blocking_send(()).unwrap();
        let (lock, changed) = &*waiting;
        let released = lock.lock().unwrap();
        let (released, _) = changed
            .wait_timeout_while(released, Duration::from_secs(5), |released| !*released)
            .unwrap();
        ensure!(*released, "Test did not release delegate callback");
        Ok(json!({"status":"completed","result":"independent"}))
    }));
    let mut work = Vec::new();
    for index in 0..2 {
        let task = broker.clone();
        work.push(tokio::task::spawn_blocking(move || {
            task.execute(
                &format!("parallel-delegate-{index}"),
                "agent_delegate",
                json!({"task":"independent read-only investigation"}),
            )
        }));
    }
    tokio::time::timeout(Duration::from_secs(2), arrivals.recv())
        .await
        .unwrap()
        .unwrap();
    let both_entered = tokio::time::timeout(Duration::from_millis(500), arrivals.recv())
        .await
        .is_ok();
    {
        *release.0.lock().unwrap() = true;
        release.1.notify_all();
    }
    for task in work {
        task.await.unwrap().unwrap();
    }
    assert!(
        both_entered,
        "A delegate callback held the registration mutex and serialized independent work"
    );
}
