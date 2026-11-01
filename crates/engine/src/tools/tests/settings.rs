use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_deny_is_authoritative_under_full_access_and_reloads() {
    let (directory, broker, _events) = fixture(PermissionMode::FullAccess);
    std::fs::create_dir_all(directory.path().join(".xylo")).unwrap();
    let path = directory.path().join(".xylo/settings.json");
    std::fs::write(
        &path,
        json!({"permissions":{"deny":["Write(*)"]}}).to_string(),
    )
    .unwrap();
    let result = run(
        &broker,
        "denied",
        "file_write",
        json!({"path":"blocked.txt","content":"no"}),
    )
    .await;
    assert!(result.is_err(), "Project deny must beat Full Access");
    assert!(!directory.path().join("blocked.txt").exists());
    std::fs::write(&path, "{}").unwrap();
    run(
        &broker,
        "allowed",
        "file_write",
        json!({"path":"allowed.txt","content":"yes"}),
    )
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_hook_updates_validated_input_and_keeps_context() {
    let (directory, broker, _events) = fixture(PermissionMode::FullAccess);
    std::fs::create_dir_all(directory.path().join(".xylo")).unwrap();
    let command = "printf '%s' '{\"updatedInput\":{\"path\":\"changed.txt\",\"content\":\"changed\"},\"additionalContext\":\"hook context\"}'";
    std::fs::write(directory.path().join(".xylo/settings.json"),json!({"hooks":{"PreToolUse":[{"matcher":"^Write$","hooks":[{"type":"command","command":command}]}]}}).to_string()).unwrap();
    run(
        &broker,
        "update",
        "file_write",
        json!({"path":"original.txt","content":"original"}),
    )
    .await
    .unwrap();
    assert!(!directory.path().join("original.txt").exists());
    assert_eq!(
        std::fs::read_to_string(directory.path().join("changed.txt")).unwrap(),
        "changed"
    );
    assert!(
        broker
            .take_notes()
            .iter()
            .any(|(_, text)| text == "hook context")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hook_updated_input_cannot_bypass_deny() {
    let (directory, broker, _events) = fixture(PermissionMode::FullAccess);
    std::fs::create_dir_all(directory.path().join(".xylo")).unwrap();
    std::fs::write(directory.path().join(".xylo/settings.json"),json!({"permissions":{"deny":["Write(secret.txt)"]},"hooks":{"PreToolUse":[{"hooks":[{"command":"printf '%s' '{\"decision\":\"allow\",\"updatedInput\":{\"path\":\"secret.txt\",\"content\":\"secret\"}}'"}]}]}}).to_string()).unwrap();
    assert!(
        run(
            &broker,
            "updated-deny",
            "file_write",
            json!({"path":"okay.txt","content":"ok"})
        )
        .await
        .is_err()
    );
    assert!(!directory.path().join("secret.txt").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hook_updated_shell_scope_requires_fresh_network_and_outside_approval() {
    let (directory, broker, mut events) = fixture(PermissionMode::AskForApproval);
    let outside = tempfile::tempdir().unwrap();
    let updated = json!({"command":"printf changed > marker","cwd":outside.path(),"network":true,"justification":"hook changed network scope"});
    let output = json!({"decision":"allow","updatedInput":updated});
    let command = format!("printf '%s' {}", shell_words::quote(&output.to_string()));
    std::fs::create_dir_all(directory.path().join(".xylo")).unwrap();
    std::fs::write(directory.path().join(".xylo/settings.json"),json!({"permissions":{"allow":["Bash(*)"]},"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"command":command}]}]}}).to_string()).unwrap();
    let task = broker.clone();
    let run = tokio::task::spawn_blocking(move || {
        task.execute(
            "scope-change",
            "shell_command",
            json!({"command":"printf original > original"}),
        )
    });
    let approval = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let EngineEvent::ApprovalRequired {
                action_id,
                reason,
                rule_key,
                arguments,
                ..
            } = events.recv().await.unwrap()
            {
                break (action_id, reason, rule_key, arguments);
            }
        }
    })
    .await;
    if approval.is_err() {
        broker.cancel.cancel();
    }
    let (id, reason, rule, args) = approval.unwrap();
    assert!(reason.unwrap().contains("hook changed network scope"));
    assert!(rule.is_none());
    assert_eq!(args["cwd"], json!(outside.path().canonicalize().unwrap()));
    broker
        .session
        .approvals()
        .decide_with(id, false, true)
        .unwrap();
    assert!(run.await.unwrap().is_err());
    assert!(!outside.path().join("marker").exists());
    assert!(!directory.path().join("original").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ask_rule_cannot_turn_read_only_refusal_into_approval() {
    let (directory, broker, _events) = fixture(PermissionMode::ReadOnly);
    xylo_services::settings_files::SettingsFiles::for_workspace(directory.path())
        .save_rule(
            "Write(*)",
            xylo_services::settings_files::PermissionDecision::Ask,
            false,
        )
        .unwrap();
    assert!(
        run(
            &broker,
            "readonly",
            "file_write",
            json!({"path":"no","content":"no"})
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("read-only")
    );
}

struct CaptureProvider(std::sync::Arc<parking_lot::Mutex<Vec<crate::model::ModelRequest>>>);
impl crate::model::ModelProvider for CaptureProvider {
    fn stream(
        &self,
        request: crate::model::ModelRequest,
        _: CancellationToken,
    ) -> crate::model::EventStream {
        self.0.lock().push(request);
        Box::pin(futures_util::stream::iter([
            Ok(crate::model::StreamEvent::Text("done".into())),
            Ok(crate::model::StreamEvent::Finished(
                crate::model::FinishReason::Stop,
            )),
        ]))
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_user_prompt_hook_receives_json_and_context_reaches_the_model() {
    let (directory, broker, _events) = fixture(PermissionMode::FullAccess);
    std::fs::create_dir_all(directory.path().join(".xylo")).unwrap();
    std::fs::write(directory.path().join(".xylo/settings.json"),json!({"hooks":{"UserPromptSubmit":[{"hooks":[{"command":"cat > hook-input.json; printf '%s' '{\"additionalContext\":\"Use the locally configured fixture context\"}'"}]}]}}).to_string()).unwrap();
    let requests = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    let (events, _receiver) = tokio::sync::mpsc::channel(128);
    let agent = crate::agent_loop::AgentLoop {
        provider: std::sync::Arc::new(CaptureProvider(requests.clone())),
        budget: crate::provider_guard::ContextBudget::new(&broker.request.provider),
        summary_deadline: Duration::from_secs(5),
        tools: std::sync::Arc::new(crate::broker_tools::BrokerTools::root(broker.clone())),
        system: "fixture".into(),
        max_tokens: 256,
        max_model_calls: 4,
        events,
        journal: None,
        permissions: None,
        cancel: CancellationToken::new(),
    };
    agent
        .run(vec![crate::model::Message::user("original request")])
        .await
        .unwrap();
    let input: Value =
        serde_json::from_slice(&std::fs::read(directory.path().join("hook-input.json")).unwrap())
            .unwrap();
    assert_eq!(input["hook_event_name"], "UserPromptSubmit");
    assert_eq!(input["prompt"], broker.request.prompt);
    assert!(requests.lock()[0].messages.iter().any(|m| {
        m.content
            .contains("Use the locally configured fixture context")
    }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_file_and_shell_tools_cannot_rewrite_xylo_authority() {
    let (directory, broker, _events) = fixture(PermissionMode::FullAccess);
    std::fs::create_dir_all(directory.path().join(".xylo")).unwrap();
    let path = directory.path().join(".xylo/settings.json");
    std::fs::write(&path, "{}").unwrap();
    run(
        &broker,
        "config-read",
        "file_read",
        json!({"path":".xylo/settings.json"}),
    )
    .await
    .unwrap();
    assert!(
        run(
            &broker,
            "config-write",
            "file_write",
            json!({"path":".xylo/settings.json","content":"{\"permissions\":{\"allow\":[\"*\"]}}"})
        )
        .await
        .is_err()
    );
    let _ = run(
        &broker,
        "config-shell",
        "shell_command",
        json!({"command":"printf changed > .xylo/settings.json"}),
    )
    .await;
    assert_eq!(std::fs::read_to_string(path).unwrap(), "{}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_deny_reports_refusal_before_any_custom_callback() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let (directory, broker, _events) = fixture(PermissionMode::FullAccess);
    let mut broker = std::sync::Arc::try_unwrap(broker).ok().unwrap();
    let called = std::sync::Arc::new(AtomicBool::new(false));
    let refused = std::sync::Arc::new(AtomicBool::new(false));
    let callback = called.clone();
    let observer = refused.clone();
    broker
        .configure_run(&crate::RunOptions {
            custom_tools: vec![ToolDefinition {
                name: "sdk_fixture".into(),
                description: "fixture".into(),
                parameters: json!({"type":"object"}),
                writes: true,
            }],
            custom_handler: Some(std::sync::Arc::new(move |_, _, _, _| {
                callback.store(true, Ordering::SeqCst);
                Ok(json!({}))
            })),
            on_refusal: Some(std::sync::Arc::new(move |_| {
                observer.store(true, Ordering::SeqCst);
            })),
            ..Default::default()
        })
        .unwrap();
    xylo_services::settings_files::SettingsFiles::for_workspace(directory.path())
        .save_rule(
            "sdk_fixture",
            xylo_services::settings_files::PermissionDecision::Deny,
            false,
        )
        .unwrap();
    let task = tokio::task::spawn_blocking(move || {
        broker.execute("custom-deny", "sdk_fixture", json!({}))
    });
    assert!(task.await.unwrap().is_err());
    assert!(!called.load(Ordering::SeqCst));
    assert!(refused.load(Ordering::SeqCst));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_f_compound_shell_deny_beats_full_access_and_broad_allow() {
    let (directory, broker, _events) = fixture(PermissionMode::FullAccess);
    std::fs::create_dir_all(directory.path().join(".xylo")).unwrap();
    std::fs::write(
        directory.path().join(".xylo/settings.json"),
        json!({"permissions":{"deny":["Bash(printf:*)"],"allow":["Bash(*)"]}}).to_string(),
    )
    .unwrap();
    let result = run(
        &broker,
        "compound-denied",
        "shell_command",
        json!({"command":"printf harmless > forbidden.txt; true"}),
    )
    .await;
    assert!(result.is_err(), "Compound command bypassed deny");
    assert!(!directory.path().join("forbidden.txt").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_f_compound_shell_ask_beats_full_access_and_broad_allow() {
    let (directory, broker, mut events) = fixture(PermissionMode::FullAccess);
    std::fs::create_dir_all(directory.path().join(".xylo")).unwrap();
    std::fs::write(
        directory.path().join(".xylo/settings.json"),
        json!({"permissions":{"ask":["Bash(printf:*)"],"allow":["Bash(*)"]}}).to_string(),
    )
    .unwrap();
    let task = broker.clone();
    let execution = tokio::task::spawn_blocking(move || {
        task.execute(
            "compound-ask",
            "shell_command",
            json!({"command":"true; printf harmless > unapproved.txt"}),
        )
    });
    let approval = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = events.recv().await {
            if let EngineEvent::ApprovalRequired { action_id, .. } = event {
                return action_id;
            }
        }
        panic!("Approval channel closed")
    })
    .await;
    if let Ok(id) = &approval {
        broker
            .session
            .approvals()
            .decide_with(*id, false, false)
            .unwrap();
    } else {
        broker.cancel.cancel();
    }
    let result = execution.await.unwrap();
    assert!(approval.is_ok(), "Compound command bypassed ask");
    assert!(result.is_err());
    assert!(!directory.path().join("unapproved.txt").exists());
}

struct StopBatchProvider(std::sync::Arc<parking_lot::Mutex<Vec<crate::model::ModelRequest>>>);
impl crate::model::ModelProvider for StopBatchProvider {
    fn stream(
        &self,
        request: crate::model::ModelRequest,
        _: CancellationToken,
    ) -> crate::model::EventStream {
        use crate::model::{FinishReason, StreamEvent};
        let first = {
            let mut requests = self.0.lock();
            requests.push(request);
            requests.len() == 1
        };
        let events = if first {
            vec![
                StreamEvent::ToolCall {
                    index: 0,
                    id: Some("first".into()),
                    name: Some("file_write".into()),
                    arguments: Some(json!({"path":"first.txt","content":"first"}).to_string()),
                },
                StreamEvent::ToolCall {
                    index: 1,
                    id: Some("second".into()),
                    name: Some("shell_command".into()),
                    arguments: Some(json!({"command":"printf second > second.txt"}).to_string()),
                },
                StreamEvent::Finished(FinishReason::ToolCalls),
            ]
        } else {
            vec![
                StreamEvent::Text("should not continue".into()),
                StreamEvent::Finished(FinishReason::Stop),
            ]
        };
        Box::pin(futures_util::stream::iter(events.into_iter().map(Ok)))
    }
}

async fn tool_boundary_stop_prevents_remaining_batch_and_model(event: &str) {
    let (directory, broker, _events) = fixture(PermissionMode::FullAccess);
    std::fs::create_dir_all(directory.path().join(".xylo")).unwrap();
    let mut settings = json!({"hooks":{}});
    settings["hooks"][event] = json!([{"matcher":"^Write$","hooks":[{"command":"printf '%s' '{\"continue\":false,\"stopReason\":\"fixture terminal stop\"}'"}]}]);
    if event == "PermissionRequest" {
        settings["permissions"] = json!({"ask":["Write(*)"]});
    }
    std::fs::write(
        directory.path().join(".xylo/settings.json"),
        settings.to_string(),
    )
    .unwrap();
    let requests = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    let (events, _receiver) = tokio::sync::mpsc::channel(128);
    let agent = crate::agent_loop::AgentLoop {
        provider: std::sync::Arc::new(StopBatchProvider(requests.clone())),
        budget: crate::provider_guard::ContextBudget::new(&broker.request.provider),
        summary_deadline: Duration::from_secs(5),
        tools: std::sync::Arc::new(crate::broker_tools::BrokerTools::root(broker.clone())),
        system: "fixture".into(),
        max_tokens: 256,
        max_model_calls: 4,
        events,
        journal: None,
        permissions: None,
        cancel: CancellationToken::new(),
    };
    let result = agent
        .run(vec![crate::model::Message::user("request")])
        .await;
    assert!(!directory.path().join("first.txt").exists());
    assert!(
        !directory.path().join("second.txt").exists(),
        "{event} failed to stop sequential dispatch"
    );
    assert_eq!(
        requests.lock().len(),
        1,
        "{event} allowed another model request"
    );
    let failure = result.expect_err("Terminal hook must end the turn");
    assert!(failure.error.to_string().contains("fixture terminal stop"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_f_pre_tool_stop_is_terminal_for_batch_and_model() {
    tool_boundary_stop_prevents_remaining_batch_and_model("PreToolUse").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_f_permission_request_stop_is_terminal_for_batch_and_model() {
    tool_boundary_stop_prevents_remaining_batch_and_model("PermissionRequest").await;
}
