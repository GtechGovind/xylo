use super::*;
use xylo_core::{InteractionAnswer, QuestionAnswer};

#[test]
fn interaction_plan_mode_is_a_supported_read_only_preset() {
    let mode: PermissionMode = serde_json::from_str("\"plan\"").expect("Plan preset must decode");
    assert!(!mode.allows_writes());
}

fn questions() -> Value {
    json!({"questions":[{"header":"Scope","question":"Which change?","options":[{"label":"Small","description":"One file"},{"label":"Broad","description":"All files"}],"multi_select":false}]})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interaction_question_answer_returns_labels_in_read_only_mode() {
    let (_root, broker, mut events) = fixture(PermissionMode::ReadOnly);
    let run = tokio::task::spawn_blocking({
        let broker = broker.clone();
        move || broker.execute("question", "ask_user", questions())
    });
    let id = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let EngineEvent::QuestionRequired { action_id, .. } = events.recv().await.unwrap() {
                break action_id;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        broker
            .session
            .answer(
                id,
                InteractionAnswer::Questions {
                    answers: vec![QuestionAnswer {
                        selected: vec![2],
                        other: None
                    }]
                }
            )
            .is_err()
    );
    broker
        .session
        .answer(
            id,
            InteractionAnswer::Questions {
                answers: vec![QuestionAnswer {
                    selected: vec![1],
                    other: None,
                }],
            },
        )
        .unwrap();
    let result = run.await.unwrap().unwrap();
    assert!(result.contains("Broad"), "{result}");
    assert!(
        broker
            .session
            .answer(id, InteractionAnswer::Reject)
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interaction_plan_approval_changes_live_mode_and_allows_same_broker_to_write() {
    let (root, broker, mut events) = fixture(PermissionMode::Plan);
    let execution = broker.clone();
    let run = tokio::task::spawn_blocking(move || {
        execution.execute("plan", "exit_plan_mode", json!({"plan":"Create one file."}))
    });
    let id = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let EngineEvent::PlanRequired { action_id, .. } = events.recv().await.unwrap() {
                break action_id;
            }
        }
    })
    .await
    .unwrap();
    broker
        .session
        .answer(
            id,
            InteractionAnswer::ApprovePlan {
                mode: PermissionMode::AskForApproval,
            },
        )
        .unwrap();
    run.await.unwrap().unwrap();
    assert_eq!(broker.mode(), PermissionMode::AskForApproval);
    let writing = broker.clone();
    tokio::task::spawn_blocking(move || {
        writing.execute(
            "write",
            "file_write",
            json!({"path":"approved.txt","content":"approved"}),
        )
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(root.path().join("approved.txt")).unwrap(),
        "approved"
    );
    // This is the live cell used by the worker's SetPermissions handler.
    broker.session.permissions().set(PermissionMode::Plan);
    let refused = tokio::task::spawn_blocking(move || {
        broker.execute(
            "after-plan",
            "file_write",
            json!({"path":"refused.txt","content":"must not write"}),
        )
    })
    .await
    .unwrap();
    assert!(refused.is_err());
    assert!(!root.path().join("refused.txt").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interaction_cancellation_withdraws_question_and_leaves_no_uncertain_action() {
    let (_root, broker, mut events) = fixture(PermissionMode::ReadOnly);
    let execution = broker.clone();
    let run = tokio::task::spawn_blocking(move || {
        execution.execute("cancel-question", "ask_user", questions())
    });
    let id = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let EngineEvent::QuestionRequired { action_id, .. } = events.recv().await.unwrap() {
                break action_id;
            }
        }
    })
    .await
    .unwrap();
    broker.cancel.cancel();
    assert!(
        run.await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
    assert!(
        broker
            .session
            .answer(id, InteractionAnswer::Reject)
            .is_err()
    );
    assert!(
        broker
            .store
            .unresolved_actions(broker.request.session_id)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interaction_headless_declines_without_waiting_or_mutating_custom_handler() {
    let (_root, mut broker, _events) = fixture(PermissionMode::Plan);
    Arc::get_mut(&mut broker)
        .unwrap()
        .configure_run(&crate::RunOptions::default())
        .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::task::spawn_blocking(move || {
            broker.execute(
                "headless-plan",
                "exit_plan_mode",
                json!({"plan":"Review this plan"}),
            )
        }),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    assert!(result.contains("reject"));
}

#[test]
fn interaction_plan_refuses_writes_hooks_network_and_native_execution() {
    let args = json!({});
    let workspace = std::env::temp_dir();
    for (name, writes, origin, network) in [
        ("file_write", true, Origin::Model, None),
        ("lsp", true, Origin::Model, None),
        ("file_read", false, Origin::Hook, None),
        ("file_read", false, Origin::Model, Some("fetch")),
    ] {
        let call = Call {
            name,
            arguments: &args,
            writes,
            origin,
            reach: &Reach::Project,
            workspace: &workspace,
            cwd: None,
            network,
            untrusted_input: false,
        };
        assert!(
            matches!(
                permissions::decide(PermissionMode::Plan, &call),
                Verdict::Refuse(_)
            ),
            "{name}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interaction_feedback_rejection_and_invalid_mode_never_grant_authority() {
    for answer in [
        InteractionAnswer::KeepPlanning {
            feedback: "Investigate tests first".into(),
        },
        InteractionAnswer::Reject,
    ] {
        let (_root, broker, mut events) = fixture(PermissionMode::Plan);
        let execution = broker.clone();
        let run = tokio::task::spawn_blocking(move || {
            execution.execute("review", "exit_plan_mode", json!({"plan":"Edit one file"}))
        });
        let id = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let EngineEvent::PlanRequired { action_id, .. } = events.recv().await.unwrap() {
                    break action_id;
                }
            }
        })
        .await
        .unwrap();
        assert!(
            broker
                .session
                .answer(
                    id,
                    InteractionAnswer::ApprovePlan {
                        mode: PermissionMode::Plan
                    }
                )
                .is_err()
        );
        broker.session.answer(id, answer).unwrap();
        run.await.unwrap().unwrap();
        assert_eq!(broker.mode(), PermissionMode::Plan);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interaction_multiple_choice_and_other_answer_preserve_user_text() {
    let (_root, broker, mut events) = fixture(PermissionMode::ReadOnly);
    let execution = broker.clone();
    let mut args = questions();
    args["questions"][0]["multi_select"] = json!(true);
    let run = tokio::task::spawn_blocking(move || execution.execute("choices", "ask_user", args));
    let id = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let EngineEvent::QuestionRequired { action_id, .. } = events.recv().await.unwrap() {
                break action_id;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        broker
            .session
            .answer(
                id,
                InteractionAnswer::Questions {
                    answers: vec![QuestionAnswer {
                        selected: vec![0, 0],
                        other: None
                    }]
                }
            )
            .is_err()
    );
    broker
        .session
        .answer(
            id,
            InteractionAnswer::Questions {
                answers: vec![QuestionAnswer {
                    selected: vec![0, 1],
                    other: Some("Include docs too".into()),
                }],
            },
        )
        .unwrap();
    let result = run.await.unwrap().unwrap();
    assert!(
        result.contains("Small") && result.contains("Broad") && result.contains("Include docs too")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interaction_plan_refuses_a_remembered_file_write() {
    let (root, broker, _events) = fixture(PermissionMode::Plan);
    std::fs::create_dir_all(root.path().join(".xylo")).unwrap();
    std::fs::write(
        root.path().join(".xylo/settings.local.json"),
        json!({"permissions":{"allow":["Write(*)"]}}).to_string(),
    )
    .unwrap();
    let execution = broker.clone();
    let result = tokio::task::spawn_blocking(move || {
        execution.execute(
            "forbidden",
            "file_write",
            json!({"path":"forbidden.txt","content":"no"}),
        )
    })
    .await
    .unwrap();
    assert!(result.unwrap_err().to_string().contains("Plan mode"));
    assert!(!broker.request.workspace.join("forbidden.txt").exists());
}
