use super::*;
use xylo_core::{Conversation, ProviderConfig, now_ms};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_native_connector_writes_do_not_create_uncertain_actions_or_approvals() {
    let (_directory, broker, mut events) = fixture(PermissionMode::AskForApproval);
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

type Fixture = (
    tempfile::TempDir,
    Arc<ToolBroker>,
    tokio::sync::mpsc::Receiver<EngineEvent>,
);

pub(super) fn fixture(mode: PermissionMode) -> Fixture {
    fixture_with(mode, |_| {})
}

fn fixture_with(mode: PermissionMode, adjust: impl FnOnce(&mut TurnRequest)) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join("profile")).unwrap();
    let store = Store::open(directory.path().join("profile/xylo.db")).unwrap();
    let session = Uuid::new_v4();
    store
        .upsert_conversation(&Conversation {
            id: session,
            title: "Fixture".into(),
            workspace: directory.path().into(),
            updated_at: now_ms(),
            title_source: Default::default(),
            pinned: false,
            archived: false,
        })
        .unwrap();
    let mut request = TurnRequest {
        session_id: session,
        turn_id: Uuid::new_v4(),
        prompt: "fixture".into(),
        workspace: directory.path().into(),
        provider: ProviderConfig::default(),
        attachments: Vec::new(),
        permissions: mode,
        approved_rules: Vec::new(),
    };
    adjust(&mut request);
    let (events, receiver) = tokio::sync::mpsc::channel(256);
    let broker = ToolBroker::new(
        request,
        store,
        CapabilityConfig {
            workspace: directory.path().into(),
            artifacts: directory.path().join("profile/artifacts"),
            management_root: Some(directory.path().join("profile/native-management")),
            ..Default::default()
        },
        ToolSession::for_tests(mode),
        events,
        CancellationToken::new(),
    )
    .unwrap();
    (directory, Arc::new(broker), receiver)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_dispatch_cannot_invoke_application_management_controls() {
    let (_directory, broker, _events) = fixture(PermissionMode::AskForApproval);
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
    let (_directory, broker, _events) = fixture(PermissionMode::AskForApproval);
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
    let (_directory, broker, _events) = fixture(PermissionMode::AskForApproval);
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
pub(super) async fn run(
    broker: &Arc<ToolBroker>,
    call: &str,
    name: &str,
    arguments: Value,
) -> Result<String> {
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
    let (directory, broker, _events) = fixture(PermissionMode::ApproveForMe);
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
    let (directory, broker, mut events) = fixture(PermissionMode::AskForApproval);
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
async fn always_allow_covers_matching_actions_for_the_turn() {
    let (_directory, broker, mut events) = fixture(PermissionMode::AskForApproval);
    let outside = tempfile::tempdir().unwrap();
    let folder = outside.path().canonicalize().unwrap();
    let notes = folder.join("notes.txt");
    let task = broker.clone();
    let target = notes.clone();
    let first = tokio::task::spawn_blocking(move || {
        task.execute(
            "write",
            "file_write",
            json!({"path":target,"content":"one\n"}),
        )
    });
    while let Some(event) = events.recv().await {
        if let EngineEvent::ApprovalRequired {
            action_id,
            arguments,
            rule,
            rule_key,
            reason,
            parent_id,
            ..
        } = event
        {
            assert_eq!(rule.unwrap(), format!("file edits in {}", folder.display()));
            assert_eq!(rule_key.unwrap(), format!("edit:{}", folder.display()));
            assert!(reason.unwrap().contains("outside the project"));
            assert_eq!(parent_id, None);
            assert!(arguments["diff"].as_str().unwrap().contains("+one"));
            assert!(arguments["request"].get("content").is_none());
            broker
                .session
                .approvals()
                .decide_with(action_id, true, true)
                .unwrap();
            break;
        }
    }
    first.await.unwrap().unwrap();
    let task = broker.clone();
    let target = notes.clone();
    tokio::task::spawn_blocking(move || {
        task.execute(
            "edit",
            "file_edit",
            json!({"path":target,"old_string":"one","new_string":"two"}),
        )
    })
    .await
    .unwrap()
    .unwrap();
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event, EngineEvent::ApprovalRequired { .. }));
    }
    assert_eq!(std::fs::read_to_string(&notes).unwrap(), "two\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approved_rules_from_the_turn_request_skip_the_prompt() {
    let outside = tempfile::tempdir().unwrap();
    let folder = outside.path().canonicalize().unwrap();
    let (_directory, broker, mut events) =
        fixture_with(PermissionMode::AskForApproval, |request| {
            request.approved_rules = vec![format!("edit:{}", folder.display())];
        });
    run(
        &broker,
        "seeded",
        "file_write",
        json!({"path":folder.join("seeded.txt"),"content":"seeded"}),
    )
    .await
    .unwrap();
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event, EngineEvent::ApprovalRequired { .. }));
    }
    assert_eq!(
        std::fs::read_to_string(folder.join("seeded.txt")).unwrap(),
        "seeded"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_connector_write_classification_cannot_be_overridden_by_arguments() {
    let (_directory, broker, mut events) = fixture(PermissionMode::ReadOnly);
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
    // Every connector call asks in Read only, even one classified as a read.
    let task = broker.clone();
    let read = tokio::task::spawn_blocking(move || {
        task.execute(
            "read",
            "connector_call",
            json!({"connector_id":id,"tool":"github_search_issues","arguments":{"query":"repo:o/r"}}),
        )
    });
    loop {
        if let EngineEvent::ApprovalRequired { action_id, .. } = events.recv().await.unwrap() {
            broker
                .session
                .approvals()
                .decide_with(action_id, true, false)
                .unwrap();
            break;
        }
    }
    let error = read.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("credential"));
    assert_eq!(broker.journaled("read").unwrap().unwrap().state, "failed");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_observation_is_not_journaled_as_success() {
    let (_directory, broker, _events) = fixture(PermissionMode::AskForApproval);
    broker.set_delegate(Arc::new(|_call: &DelegateCall| {
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
async fn incomplete_delegate_is_failed_with_partial_result_preserved() {
    let (_directory, broker, _events) = fixture(PermissionMode::AskForApproval);
    broker.set_delegate(Arc::new(|_call: &DelegateCall| Ok(json!({"status":"incomplete","result":"partial findings","stopped_reason":"Assigned turn limit reached"}))));
    let task = broker.clone();
    assert!(
        tokio::task::spawn_blocking(move || task.execute(
            "incomplete-child",
            "agent_delegate",
            json!({"task":"fixture"})
        ))
        .await
        .unwrap()
        .is_err()
    );
    let action = broker.journaled("incomplete-child").unwrap().unwrap();
    assert_eq!(action.state, "failed");
    assert_eq!(action.result.unwrap()["result"], "partial findings");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uncertain_observation_is_retained_and_never_replayed_as_success() {
    let (_directory, broker, mut events) = fixture(PermissionMode::AskForApproval);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = calls.clone();
    let observation = json!({"status":"outcome_unknown","summary":"x".repeat(20_000),
        "artifacts":["partial.txt"]});
    let output = observation.clone();
    broker.set_delegate(Arc::new(move |_call: &DelegateCall| {
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
            // A delegate's answer reaches the UI at the model's bound.
            assert!(result.len() <= 64 * 1024);
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
async fn model_supplied_plugin_capabilities_are_refused_before_any_intent() {
    let (_directory, broker, mut events) = fixture(PermissionMode::FullAccess);
    let error = run(
        &broker,
        "self-granted",
        "plugin_run_installed",
        json!({"id":"fixture","package_sha256":"0".repeat(64),"capabilities":["read_workspace"]}),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("reviewed"), "{error}");
    assert!(broker.journaled("self-granted").unwrap().is_none());
    assert!(events.try_recv().is_err());
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
    let (_directory, broker, mut events) = fixture(PermissionMode::AskForApproval);
    let outside = tempfile::tempdir().unwrap();
    let never = outside.path().join("never.txt");
    let task = broker.clone();
    let target = never.clone();
    let running = tokio::task::spawn_blocking(move || {
        task.execute(
            "write-denied",
            "file_write",
            json!({"path":target,"content":"denied"}),
        )
    });
    let mut phases = Vec::new();
    while let Some(event) = events.recv().await {
        match event {
            EngineEvent::Phase { phase } => phases.push(phase),
            EngineEvent::ApprovalRequired { action_id, .. } => {
                broker
                    .session
                    .approvals()
                    .decide_with(action_id, false, false)
                    .unwrap();
                break;
            }
            _ => {}
        }
    }
    assert!(running.await.unwrap().is_err());
    assert!(!never.exists());
    assert_eq!(
        broker.journaled("write-denied").unwrap().unwrap().state,
        "denied"
    );
    while let Ok(event) = events.try_recv() {
        if let EngineEvent::Phase { phase } = event {
            phases.push(phase);
        }
    }
    assert_eq!(
        phases,
        [
            xylo_core::TurnPhase::WaitingForApproval,
            xylo_core::TurnPhase::RunningTools
        ]
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
    assert!(context.contains("Learned procedure"), "{context}");
    assert!(context.contains("skill_read"));
    assert!(
        !context.contains("--offline"),
        "Procedure steps load on demand, never in the prompt: {context}"
    );
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
    let (_directory, broker, _events) = fixture(PermissionMode::AskForApproval);
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
    let (directory, broker, _events) = fixture(PermissionMode::AskForApproval);
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
        ToolSession::for_tests(broker.mode()),
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
    broker.set_delegate(Arc::new(move |_call: &DelegateCall| {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_tool_allowlist_is_enforced_through_dynamic_dispatch() {
    let (_directory, parent, _events) = fixture(PermissionMode::ApproveForMe);
    let mut child = parent.scoped_child(Uuid::new_v4()).unwrap();
    child.restrict_tools(&["Read".into()]).unwrap();
    assert!(
        child.definitions().iter().all(|definition| matches!(
            definition.name.as_str(),
            "file_read" | "extension_resource"
        ))
    );
    let denied = tokio::task::block_in_place(|| {
        child.execute_child(
            "parent",
            "child",
            "call_tool",
            json!({"name":"shell_command","arguments":{"command":"touch forbidden"}}),
        )
    })
    .unwrap_err();
    assert!(denied.to_string().contains("not authorized"));
    assert!(!parent.cancellation_token().is_cancelled());
    let mut constrained = parent.scoped_child(Uuid::new_v4()).unwrap();
    assert!(constrained.restrict_tools(&["Bash(git:*)".into()]).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_empty_child_tool_allowlist_denies_every_tool() {
    let (_directory, parent, _events) = fixture(PermissionMode::ApproveForMe);
    let mut child = parent.scoped_child(Uuid::new_v4()).unwrap();
    child.restrict_tools(&[]).unwrap();
    assert!(child.definitions().is_empty());
    let direct = tokio::task::block_in_place(|| {
        child.execute_child("parent", "read", "file_read", json!({"path":"README.md"}))
    });
    assert!(direct.unwrap_err().to_string().contains("not authorized"));
    let dynamic = tokio::task::block_in_place(|| {
        child.execute_child(
            "parent",
            "dynamic",
            "call_tool",
            json!({"name":"file_read","arguments":{"path":"README.md"}}),
        )
    });
    assert!(dynamic.unwrap_err().to_string().contains("not authorized"));
}

fn install_portable_test_skill(broker: &ToolBroker, name: &str, body: &str) -> String {
    let source = tempfile::tempdir().unwrap();
    std::fs::write(
        source.path().join("SKILL.md"),
        format!("---\nname: {name}\ndescription: Permission regression fixture\n---\n{body}"),
    )
    .unwrap();
    let service = super::portable::service(broker);
    let package = service.install_local(source.path()).unwrap();
    service.set_enabled(&package.id, true).unwrap();
    format!("{}:{name}", package.id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn portable_dynamic_context_is_denied_in_read_only_tasks_while_plain_skill_loads() {
    let (directory, broker, mut events) = fixture(PermissionMode::ReadOnly);
    let plain =
        install_portable_test_skill(&broker, "plain", "Inspect the files without changing them.");
    let dynamic =
        install_portable_test_skill(&broker, "dynamic", "!`printf executed > dynamic-marker`");
    let task = broker.clone();
    tokio::task::spawn_blocking(move || {
        let loaded = task
            .execute("plain", "extension_skill", json!({"id": plain}))
            .unwrap();
        assert!(loaded.contains("Inspect the files"));
        let error = task
            .execute("dynamic", "extension_skill", json!({"id": dynamic}))
            .unwrap_err();
        assert!(
            error.to_string().contains("requires write permission"),
            "{error}"
        );
        assert!(task.journaled("dynamic").unwrap().is_none());
    })
    .await
    .unwrap();
    assert!(!directory.path().join("dynamic-marker").exists());
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event, EngineEvent::ApprovalRequired { .. }));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn portable_dynamic_context_waits_for_broker_approval_and_denial_never_executes() {
    let (directory, broker, mut events) = fixture(PermissionMode::AskForApproval);
    let id = install_portable_test_skill(&broker, "dynamic", "!`printf executed > dynamic-marker`");
    let task = broker.clone();
    let execution = tokio::task::spawn_blocking(move || {
        task.execute("dynamic-approval", "extension_skill", json!({"id": id}))
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(event) = events.recv().await {
            if let EngineEvent::ApprovalRequired { action_id, .. } = event {
                assert!(!directory.path().join("dynamic-marker").exists());
                broker
                    .session
                    .approvals()
                    .decide_with(action_id, false, false)
                    .unwrap();
                return;
            }
        }
        panic!("Dynamic context must request approval");
    })
    .await
    .unwrap();
    assert!(
        execution
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("denied")
    );
    assert!(!directory.path().join("dynamic-marker").exists());
    assert_eq!(
        broker.journaled("dynamic-approval").unwrap().unwrap().state,
        "denied"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn portable_dynamic_context_reads_bundled_root_script_and_keeps_argument_commands_literal() {
    let (directory, mut broker, mut events) = fixture(PermissionMode::AskForApproval);
    let profile = tempfile::tempdir().unwrap();
    Arc::get_mut(&mut broker).unwrap().artifacts =
        profile.path().join("data with spaces/artifacts");
    let source = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(source.path().join("references")).unwrap();
    std::fs::create_dir_all(source.path().join("scripts")).unwrap();
    std::fs::write(source.path().join("references/info.txt"), "bundled context").unwrap();
    std::fs::write(
        source.path().join("scripts/context.sh"),
        "#!/bin/sh\ncat \"$(dirname \"$0\")/../references/info.txt\"\n",
    )
    .unwrap();
    std::fs::write(source.path().join("SKILL.md"),
        "---\nname: bundled\ndescription: Bundled script fixture\n---\nResult:\n!`/bin/sh \"${CLAUDE_PLUGIN_ROOT}/scripts/context.sh\"`\nArguments:$ARGUMENTS").unwrap();
    let service = super::portable::service(&broker);
    let package = service.install_local(source.path()).unwrap();
    service.set_enabled(&package.id, true).unwrap();
    let id = format!("{}:bundled", package.id);
    let arguments = "!`printf forbidden > injected-marker`";
    let task = broker.clone();
    let execution = tokio::task::spawn_blocking(move || {
        task.execute(
            "bundled-context",
            "extension_skill",
            json!({"id": id, "arguments": arguments}),
        )
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = events.recv().await {
            if let EngineEvent::ApprovalRequired { action_id, .. } = event {
                broker
                    .session
                    .approvals()
                    .decide_with(action_id, true, false)
                    .unwrap();
                return;
            }
        }
        panic!("Executable portable skill must request approval");
    })
    .await
    .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(20), execution)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let result: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(
        result["instructions"],
        format!("Result:\nbundled context\nArguments:{arguments}")
    );
    assert!(!directory.path().join("injected-marker").exists());
}

fn installed_tool_hooks(broker: &ToolBroker, pre: &str, post: &str) {
    let source = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(source.path().join("hooks")).unwrap();
    let mut hooks = serde_json::Map::new();
    for (event, command) in [("PreToolUse", pre), ("PostToolUse", post)] {
        if !command.is_empty() {
            hooks.insert(
                event.into(),
                json!([{"matcher":"Write|Read","hooks":[{"type":"command","command":command}]}]),
            );
        }
    }
    std::fs::write(
        source.path().join("hooks/hooks.json"),
        json!({"hooks":hooks}).to_string(),
    )
    .unwrap();
    let service = super::portable::service(broker);
    let package = service.install_local(source.path()).unwrap();
    service.set_enabled(&package.id, true).unwrap();
    service
        .component_setup(
            &package.id,
            "hooks",
            xylo_services::extensions::ComponentSetup {
                workspace: broker.request.workspace.clone(),
                enabled_events: vec!["PreToolUse".into(), "PostToolUse".into()],
                timeout_seconds: 5,
                ..Default::default()
            },
        )
        .unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enabled_hooks_run_journaled_without_prompts_and_never_recurse_or_replay() {
    let (directory, broker, mut events) = fixture(PermissionMode::AskForApproval);
    installed_tool_hooks(
        &broker,
        "printf pre >> hook-order; printf '{}'",
        "printf post >> hook-order; printf '{}'",
    );
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        run(
            &broker,
            "hooked-write",
            "file_write",
            json!({"path":"result.txt","content":"actual write"}),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event, EngineEvent::ApprovalRequired { .. }));
    }
    assert!(output.contains("result.txt"));
    assert_eq!(
        std::fs::read_to_string(directory.path().join("result.txt")).unwrap(),
        "actual write"
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("hook-order")).unwrap(),
        "prepost"
    );
    let actions = broker
        .store
        .actions_by_name("extension_component", 20)
        .unwrap();
    assert_eq!(actions.len(), 2);
    assert!(actions.iter().all(|action| action.state == "succeeded"));
    assert!(
        actions
            .iter()
            .all(|action| action.intent.arguments["input"]["tool_name"] == "file_write")
    );
    let replay = run(
        &broker,
        "hooked-write",
        "file_write",
        json!({"path":"result.txt","content":"actual write"}),
    )
    .await
    .unwrap();
    assert_eq!(replay, output);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("hook-order")).unwrap(),
        "prepost"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hook_denial_blocks_original_mutation_and_read_only_tasks_still_run_hooks() {
    let (directory, broker, _events) = fixture(PermissionMode::ApproveForMe);
    installed_tool_hooks(
        &broker,
        "printf hook > hook-marker; printf '{\"hookSpecificOutput\":{\"permissionDecision\":\"deny\"}}'",
        "",
    );
    let error = tokio::time::timeout(
        Duration::from_secs(10),
        run(
            &broker,
            "blocked-write",
            "file_write",
            json!({"path":"should-not-exist.txt","content":"blocked"}),
        ),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(error.to_string().contains("Pre-tool hook prevented"));
    assert!(!directory.path().join("should-not-exist.txt").exists());
    assert_eq!(
        broker.journaled("blocked-write").unwrap().unwrap().state,
        "failed"
    );
    let (directory, broker, mut events) = fixture(PermissionMode::ReadOnly);
    std::fs::write(directory.path().join("sample.txt"), "readable").unwrap();
    installed_tool_hooks(&broker, "printf ran > hook-marker; printf '{}'", "");
    let read = run(
        &broker,
        "readonly-hook",
        "file_read",
        json!({"path":"sample.txt"}),
    )
    .await
    .unwrap();
    assert!(read.contains("readable"));
    assert!(directory.path().join("hook-marker").exists());
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event, EngineEvent::ApprovalRequired { .. }));
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn post_hook_failure_preserves_completed_tool_receipt_and_blocks_uncertain_hook_replay() {
    let (directory, broker, _events) = fixture(PermissionMode::ApproveForMe);
    installed_tool_hooks(&broker, "", "printf once >> post-marker; exit 1");
    let task = broker.clone();
    let execute = tokio::task::spawn_blocking(move || {
        task.execute(
            "post-write",
            "file_write",
            json!({"path":"written.txt","content":"durable"}),
        )
    });
    let result: Value = serde_json::from_str(&execute.await.unwrap().unwrap()).unwrap();
    assert!(
        result["post_tool_hook_error"]
            .as_str()
            .unwrap()
            .contains("Tool completed")
    );
    assert_eq!(
        broker.journaled("post-write").unwrap().unwrap().state,
        "succeeded"
    );
    assert_eq!(
        broker
            .store
            .actions_by_name("extension_component", 5)
            .unwrap()[0]
            .state,
        "outcome_unknown"
    );
    run(
        &broker,
        "post-write",
        "file_write",
        json!({"path":"written.txt","content":"durable"}),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(directory.path().join("post-marker")).unwrap(),
        "once"
    );
    let next = run(
        &broker,
        "next-read",
        "file_read",
        json!({"path":"written.txt"}),
    )
    .await
    .unwrap();
    let next: Value = serde_json::from_str(&next).unwrap();
    assert!(
        next["post_tool_hook_error"]
            .as_str()
            .unwrap()
            .contains("previous hook outcome is unknown")
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("post-marker")).unwrap(),
        "once"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hook_updated_input_is_rejected_before_original_mutation() {
    let (directory, broker, _events) = fixture(PermissionMode::ApproveForMe);
    installed_tool_hooks(
        &broker,
        "printf '{\"hookSpecificOutput\":{\"permissionDecision\":\"allow\",\"updatedInput\":{\"path\":\"changed.txt\"}}}'",
        "",
    );
    let task = broker.clone();
    let execute = tokio::task::spawn_blocking(move || {
        task.execute(
            "updated-input",
            "file_write",
            json!({"path":"original.txt","content":"blocked"}),
        )
    });
    let error = execute.await.unwrap().unwrap_err();
    assert!(format!("{error:#}").contains("Unsupported hook execution-changing directive"));
    assert!(!directory.path().join("original.txt").exists());
    assert!(!directory.path().join("changed.txt").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn skill_update_during_approval_never_executes_replacement_commands() {
    let (directory, broker, mut events) = fixture(PermissionMode::AskForApproval);
    let source = tempfile::tempdir().unwrap();
    std::fs::write(
        source.path().join("SKILL.md"),
        "---\nname: pinned\ndescription: Version fixture\n---\n!`printf old > old-marker`\n",
    )
    .unwrap();
    let service = super::portable::service(&broker);
    let package = service.install_local(source.path()).unwrap();
    service.set_enabled(&package.id, true).unwrap();
    let id = format!("{}:pinned", package.id);
    let task = broker.clone();
    let execute = tokio::task::spawn_blocking(move || {
        task.execute("pinned-skill", "extension_skill", json!({"id":id}))
    });
    tokio::time::timeout(Duration::from_secs(5),async{while let Some(event)=events.recv().await{if let EngineEvent::ApprovalRequired{action_id,..}=event{
        let binding=broker.store.extension_action_binding(action_id).unwrap().unwrap();assert_eq!(binding["kind"],"skill");assert!(binding["snapshot"]["dynamic_commands"][0].as_str().unwrap().contains("old-marker"));
        std::fs::write(source.path().join("SKILL.md"),"---\nname: pinned\ndescription: Version fixture\n---\n!`printf new > new-marker`\n").unwrap();let updated=service.install_local(source.path()).unwrap();assert_ne!(updated.digest,package.digest);service.set_enabled(&updated.id,true).unwrap();broker.session.approvals().decide_with(action_id, true, false).unwrap();return;
    }}}).await.unwrap();
    let error = execute.await.unwrap().unwrap_err();
    let error = format!("{error:#}");
    assert!(error.contains("Skill changed since approval"), "{error}");
    assert!(!directory.path().join("old-marker").exists());
    assert!(!directory.path().join("new-marker").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn approved_sandbox_rust_toolchain_builds_std_fixture_without_home_credentials() {
    let (directory, broker, mut events) = fixture(PermissionMode::AskForApproval);
    std::fs::create_dir_all(directory.path().join("src")).unwrap();
    std::fs::write(directory.path().join("Cargo.toml"),"[package]\nname = \"xylo-sandbox-rust-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
    std::fs::write(directory.path().join("src/lib.rs"),"#[test]\nfn standard_library_works() { assert_eq!(std::path::Path::new(\"src/lib.rs\").file_name().unwrap(), \"lib.rs\"); }\n").unwrap();
    let home_probe = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    let home_secret = home_probe.path().join("generated-provider-credential");
    std::fs::write(&home_secret, "xylo-home-denial-sentinel").unwrap();
    let profile_secret = broker
        .artifacts
        .parent()
        .unwrap()
        .join("generated-provider-credential");
    std::fs::write(&profile_secret, "xylo-profile-denial-sentinel").unwrap();
    let approval_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = approval_count.clone();
    let real_cargo = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cargo"));
    let cargo_probe = tempfile::tempdir_in(&real_cargo).unwrap();
    let cargo_secret = cargo_probe.path().join("generated-credential");
    std::fs::write(&cargo_secret, "xylo-cargo-credential-sentinel").unwrap();
    let cache = std::fs::read_dir(real_cargo.join("registry/cache"))
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("index.crates.io-")
        })
        .unwrap()
        .path();
    let cache_probe = tempfile::NamedTempFile::new_in(cache).unwrap();
    std::fs::write(cache_probe.path(), "xylo-cache-immutable-sentinel").unwrap();
    let approvals = broker.clone();
    let approver = tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if let EngineEvent::ApprovalRequired {
                action_id, name, ..
            } = event
            {
                assert!(matches!(name.as_str(), "run_command" | "shell_command"));
                approvals
                    .session
                    .approvals()
                    .decide_with(action_id, true, false)
                    .unwrap();
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
    });
    let version = tokio::time::timeout(
        Duration::from_secs(15),
        run(
            &broker,
            "rust-version",
            "run_command",
            json!({"program":"cargo","args":["--version"]}),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(version.contains("cargo "), "{version}");
    let tests=tokio::time::timeout(Duration::from_secs(45),run(&broker,"rust-build-test","shell_command",json!({"command":"rustc --version && cargo test --offline --quiet","timeout_seconds":40}))).await.unwrap().unwrap();
    assert!(tests.contains("1 passed"), "{tests}");
    std::fs::write(directory.path().join("Cargo.toml"),"[package]\nname = \"xylo-sandbox-rust-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nserde_json = \"1\"\n".replace("\\\"", "\"")).unwrap();
    std::fs::write(directory.path().join("src/lib.rs"),"#[test]\nfn cached_public_dependency_works() { let value: serde_json::Value = serde_json::from_str(r#\"{\"cached\":true}\"#).unwrap(); assert_eq!(value[\"cached\"], true); }\n".replace("\\\"", "\"")).unwrap();
    let cached = tokio::time::timeout(
        Duration::from_secs(60),
        run(
            &broker,
            "rust-cached-test",
            "shell_command",
            json!({"command":"cargo test --offline --quiet","timeout_seconds":55}),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(cached.contains("1 passed"), "{cached}");
    let command = format!(
        "if cat {} > home-leak; then exit 91; fi; if cat {} > profile-leak; then exit 92; fi; if cat {} > cargo-credential-leak; then exit 93; fi; if printf changed > {}; then exit 94; fi; printf denied",
        shell_words::quote(&home_secret.to_string_lossy()),
        shell_words::quote(&profile_secret.to_string_lossy()),
        shell_words::quote(&cargo_secret.to_string_lossy()),
        shell_words::quote(&cache_probe.path().to_string_lossy())
    );
    let denied = run(
        &broker,
        "rust-credentials-denied",
        "shell_command",
        json!({"command":command}),
    )
    .await
    .unwrap();
    assert!(denied.contains("denied"));
    assert!(
        !std::fs::read_to_string(directory.path().join("home-leak"))
            .unwrap()
            .contains("xylo-home-denial-sentinel")
    );
    assert!(
        !std::fs::read_to_string(directory.path().join("profile-leak"))
            .unwrap()
            .contains("xylo-profile-denial-sentinel")
    );
    assert!(
        !std::fs::read_to_string(directory.path().join("cargo-credential-leak"))
            .unwrap()
            .contains("xylo-cargo-credential-sentinel")
    );
    assert_eq!(
        std::fs::read_to_string(cache_probe.path()).unwrap(),
        "xylo-cache-immutable-sentinel"
    );
    // Sandboxed project commands run without prompts in the default mode.
    assert_eq!(approval_count.load(std::sync::atomic::Ordering::SeqCst), 0);
    approver.abort();
    let _ = approver.await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_git_read_tools_never_execute_repository_fsmonitor_hooks() {
    use std::os::unix::fs::PermissionsExt;
    let (directory, broker, _events) = fixture(PermissionMode::ReadOnly);
    let git = code::resolve_program("git", directory.path()).unwrap();
    for arguments in [vec!["init", "--quiet"], vec!["add", "tracked.txt"]] {
        if arguments[0] == "add" {
            std::fs::write(directory.path().join("tracked.txt"), "fixture\n").unwrap();
        }
        let status = std::process::Command::new(&git)
            .args(arguments)
            .current_dir(directory.path())
            .status()
            .unwrap();
        assert!(status.success());
    }
    let marker = directory.path().join("fsmonitor-executed");
    let hook = directory.path().join("fsmonitor.sh");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\nprintf invoked > {}\nprintf 'token\\0'\n",
            shell_words::quote(&marker.to_string_lossy()),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        std::process::Command::new(&git)
            .args(["config", "core.fsmonitor", hook.to_str().unwrap()])
            .current_dir(directory.path())
            .status()
            .unwrap()
            .success()
    );
    let task = broker.clone();
    let observed = tokio::task::spawn_blocking(move || {
        let mut observed = Vec::new();
        for tool in ["git_status", "git_diff"] {
            task.execute(tool, tool, json!({})).unwrap();
            observed.push((tool, marker.exists()));
            if marker.exists() {
                std::fs::remove_file(&marker).unwrap();
            }
        }
        observed
    })
    .await
    .unwrap();
    assert!(
        observed.iter().all(|(_, ran)| !ran),
        "Read tools ran repository code: {observed:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_edits_to_one_file_both_land() {
    let (directory, broker, _events) = fixture(PermissionMode::AskForApproval);
    let lines: String = (0..64).map(|index| format!("line {index}\n")).collect();
    std::fs::write(directory.path().join("shared.txt"), &lines).unwrap();
    let mut edits = Vec::new();
    for index in 0..64 {
        let task = broker.clone();
        edits.push(tokio::task::spawn_blocking(move || {
            task.execute(
                &format!("edit-{index}"),
                "file_edit",
                json!({"path":"shared.txt","old_string":format!("line {index}\n"),"new_string":format!("edited {index}\n")}),
            )
        }));
    }
    for edit in edits {
        edit.await.unwrap().unwrap();
    }
    let expected: String = (0..64).map(|index| format!("edited {index}\n")).collect();
    assert_eq!(
        std::fs::read_to_string(directory.path().join("shared.txt")).unwrap(),
        expected
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permission_changes_apply_to_the_next_call_of_a_running_task() {
    let (directory, broker, mut events) = fixture(PermissionMode::AskForApproval);
    assert!(
        broker
            .initial_definitions()
            .iter()
            .any(|tool| tool.name == "file_write")
    );
    broker.session.permissions().set(PermissionMode::ReadOnly);
    assert!(broker.initial_definitions().iter().all(|tool| !tool.writes));
    let refused = run(
        &broker,
        "read-only-write",
        "file_write",
        json!({"path":"a.txt","content":"x"}),
    )
    .await
    .unwrap_err();
    assert!(refused.to_string().contains("read-only"), "{refused}");
    assert!(broker.journaled("read-only-write").unwrap().is_none());
    broker.session.permissions().set(PermissionMode::FullAccess);
    let outside = tempfile::tempdir().unwrap();
    run(
        &broker,
        "full-access-write",
        "file_write",
        json!({"path":outside.path().join("b.txt"),"content":"anywhere"}),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(outside.path().join("b.txt")).unwrap(),
        "anywhere"
    );
    broker
        .session
        .permissions()
        .set(PermissionMode::AskForApproval);
    run(
        &broker,
        "project-write",
        "file_write",
        json!({"path":"c.txt","content":"inside"}),
    )
    .await
    .unwrap();
    assert!(directory.path().join("c.txt").exists());
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event, EngineEvent::ApprovalRequired { .. }));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approve_for_me_asks_only_for_risky_actions() {
    let (directory, broker, mut events) = fixture(PermissionMode::ApproveForMe);
    std::fs::create_dir_all(directory.path().join("build")).unwrap();
    run(
        &broker,
        "safe",
        "run_command",
        json!({"program":"git","args":["--version"]}),
    )
    .await
    .unwrap();
    let task = broker.clone();
    let risky = tokio::task::spawn_blocking(move || {
        task.execute("risky", "shell_command", json!({"command":"rm -rf build"}))
    });
    let reason = loop {
        if let EngineEvent::ApprovalRequired {
            action_id, reason, ..
        } = events.recv().await.unwrap()
        {
            broker
                .session
                .approvals()
                .decide_with(action_id, false, false)
                .unwrap();
            break reason.unwrap();
        }
    };
    assert!(reason.contains("Deletes"), "{reason}");
    assert!(risky.await.unwrap().is_err());
    assert!(directory.path().join("build").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_events_carry_the_parent_action_and_delegate_results_are_not_placeholders() {
    let (directory, broker, mut events) = fixture(PermissionMode::AskForApproval);
    std::fs::write(directory.path().join("child.txt"), "child").unwrap();
    let parent = Uuid::new_v4();
    let child = broker.scoped_child(parent).unwrap();
    tokio::task::block_in_place(|| {
        child
            .execute_child("parent", "read", "file_read", json!({"path":"child.txt"}))
            .unwrap()
    });
    let mut stamped = 0;
    while let Ok(event) = events.try_recv() {
        match event {
            EngineEvent::ToolStarted { parent_id, .. }
            | EngineEvent::ToolFinished { parent_id, .. } => {
                assert_eq!(parent_id, Some(parent));
                stamped += 1;
            }
            _ => {}
        }
    }
    assert_eq!(stamped, 2);
    let summary = "finding ".repeat(4000);
    let answer = summary.clone();
    broker.set_delegate(Arc::new(move |_call: &DelegateCall| {
        Ok(json!({"status":"completed","result":answer}))
    }));
    run(
        &broker,
        "delegate",
        "agent_delegate",
        json!({"task":"look"}),
    )
    .await
    .unwrap();
    let presented = loop {
        if let EngineEvent::ToolFinished { name, result, .. } = events.recv().await.unwrap()
            && name == "agent_delegate"
        {
            break result;
        }
    };
    let presented: Value = serde_json::from_str(&presented).unwrap();
    assert_eq!(presented["result"], summary);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn built_in_skills_load_on_demand_without_approval() {
    let (_directory, broker, mut events) = fixture(PermissionMode::ReadOnly);
    let skill = crate::prompts::bundled_skills()
        .iter()
        .find(|skill| !skill.resources.is_empty())
        .unwrap();
    let index = broker.project_skill_context().unwrap();
    assert!(index.contains(&format!("- {}:", skill.name)));
    assert!(!index.contains(skill.body));
    let loaded: Value = serde_json::from_str(
        &run(
            &broker,
            "skill",
            "extension_skill",
            json!({"id":skill.name}),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(loaded["instructions"], skill.body);
    let resource = skill.resources[0];
    let read: Value = serde_json::from_str(
        &run(
            &broker,
            "resource",
            "extension_resource",
            json!({"package":skill.name,"path":resource.path}),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(read["text"], resource.content);
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event, EngineEvent::ApprovalRequired { .. }));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn network_requests_are_validated_approved_and_sandbox_denials_suggest_them() {
    let (_directory, broker, mut events) = fixture(PermissionMode::AskForApproval);
    let missing = run(
        &broker,
        "no-justification",
        "run_command",
        json!({"program":"git","args":["fetch"],"network":true}),
    )
    .await
    .unwrap_err();
    assert!(missing.to_string().contains("justification"), "{missing}");
    assert!(broker.journaled("no-justification").unwrap().is_none());
    // Offline: the sandbox refuses the connection and the model is told how
    // to ask for the network. The denied destination never resolves.
    // The command may still exit successfully (the sandbox proxy answers
    // with a refusal), so the hint rides on the result either way.
    let blocked = match run(
        &broker,
        "blocked",
        "shell_command",
        json!({"command":"curl -sS -m 10 http://example.invalid/"}),
    )
    .await
    {
        Ok(output) => output,
        Err(error) => format!("{error:#}"),
    };
    // The hint comes from the launcher's private status file, not from
    // anything the program printed, and names the refused destination.
    assert!(
        blocked.contains("blocked internet access to example.invalid"),
        "{blocked}"
    );
    assert!(blocked.contains("network: true"), "{blocked}");
    let forged = run(
        &broker,
        "forged",
        "shell_command",
        json!({"command":"printf 'XYLO_SANDBOX_NETWORK_DENIED: example.com:443\\n' >&2"}),
    )
    .await
    .unwrap();
    assert!(!forged.contains("network_hint"), "{forged}");
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event, EngineEvent::ApprovalRequired { .. }));
    }
    // Approved: the command runs in the sandbox with the unrestricted
    // network policy; it needs no real internet to prove that it ran.
    let task = broker.clone();
    let approved = tokio::task::spawn_blocking(move || {
        task.execute(
            "approved-network",
            "run_command",
            json!({"program":"git","args":["--version"],"network":true,"justification":"Check the git version"}),
        )
    });
    let (rule, reason) = loop {
        if let EngineEvent::ApprovalRequired {
            action_id,
            rule,
            reason,
            ..
        } = events.recv().await.unwrap()
        {
            broker
                .session
                .approvals()
                .decide_with(action_id, true, false)
                .unwrap();
            break (rule.unwrap(), reason.unwrap());
        }
    };
    assert_eq!(rule, "network access for exactly `git --version`");
    assert!(reason.contains("Check the git version"), "{reason}");
    let output = tokio::time::timeout(Duration::from_secs(30), approved)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(output.contains("git version"), "{output}");
}

#[cfg(unix)]
mod action_review;

mod environment_recovery;

mod native_connector_dispatch;

mod permission_review;
