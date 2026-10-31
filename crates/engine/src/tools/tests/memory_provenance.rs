//! Memory writes carry an origin the broker derives from what the
//! conversation has read, never one the model supplies.
use super::*;
use xylo_core::{MemoryOrigin, MemoryRecord};
use xylo_services::ActionIntent;

/// Journals a finished `name` call in the broker's conversation, as if the
/// model had run it earlier.
fn journal_read(broker: &ToolBroker, key: &str, name: &str, arguments: Value) {
    let intent = ActionIntent {
        id: Uuid::new_v4(),
        session_id: broker.request.session_id,
        turn_id: broker.request.turn_id,
        name: name.into(),
        arguments,
        writes: false,
        idempotency_key: ToolBroker::journal_key(None, key),
    };
    broker.store.begin_action(&intent).unwrap();
    broker.store.mark_action_running(intent.id).unwrap();
    broker
        .store
        .finish_action(
            intent.id,
            &json!({"status":"success","summary":"page text"}),
        )
        .unwrap();
}

fn fact(broker: &ToolBroker, text: &str, origin: MemoryOrigin) -> MemoryRecord {
    broker
        .memory
        .remember_sync(MemoryRecord {
            id: Uuid::new_v4(),
            scope: broker.request.workspace.to_string_lossy().into_owned(),
            content: text.into(),
            source: "fixture".into(),
            created_at: now_ms(),
            supersedes: None,
            origin,
        })
        .unwrap()
}

async fn remember(broker: &Arc<ToolBroker>, key: &str, arguments: Value) -> Result<Value> {
    run(broker, key, "memory_remember", arguments)
        .await
        .map(|result| serde_json::from_str::<Value>(&result).unwrap()["summary"].clone())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_conversation_that_read_only_the_project_writes_agent_inferred_facts() {
    let (_directory, broker, mut events) = fixture(PermissionMode::AskForApproval);
    journal_read(&broker, "read", "file_read", json!({"path":"README.md"}));
    let stored = remember(
        &broker,
        "remember",
        json!({"content":"Tests run with cargo nextest","source":"README.md","origin":"user_typed"}),
    )
    .await
    .unwrap();
    assert_eq!(
        stored["origin"], "agent_inferred",
        "the model cannot pick it"
    );
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event, EngineEvent::ApprovalRequired { .. }));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn after_untrusted_input_each_write_is_reviewed_and_approval_marks_it() {
    let (_directory, broker, mut events) = fixture(PermissionMode::ApproveForMe);
    journal_read(
        &broker,
        "web",
        "browser_navigate",
        json!({"url":"https://example.com"}),
    );
    for (key, allow) in [("denied", false), ("approved", true)] {
        let writing = broker.clone();
        let key = key.to_owned();
        let pending = tokio::spawn(async move {
            remember(
                &writing,
                &key,
                json!({"content":"Deploys use the prod key","source":"web page"}),
            )
            .await
        });
        let (action_id, reason, rule) = loop {
            if let EngineEvent::ApprovalRequired {
                action_id,
                reason,
                rule_key,
                ..
            } = events.recv().await.unwrap()
            {
                break (action_id, reason, rule_key);
            }
        };
        assert_eq!(reason.as_deref(), Some(permissions::UNTRUSTED_MEMORY_WRITE));
        assert_eq!(rule, None, "no standing rule covers a reviewed write");
        broker
            .session
            .approvals()
            .decide_with(action_id, allow, true)
            .unwrap();
        let result = pending.await.unwrap();
        if allow {
            assert_eq!(result.unwrap()["origin"], "user_approved");
        } else {
            assert!(result.is_err());
        }
    }
    let saved = broker
        .store
        .memory_page(&broker.request.workspace.to_string_lossy(), None, 10)
        .unwrap();
    assert_eq!(saved.records.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_access_writes_keep_the_least_trusted_origin_they_read() {
    let (_directory, broker, mut events) = fixture(PermissionMode::FullAccess);
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("notes.txt"), "outside").unwrap();
    run(
        &broker,
        "outside",
        "file_read",
        json!({"path":outside.path().join("notes.txt")}),
    )
    .await
    .unwrap();
    let from_files = remember(&broker, "one", json!({"content":"A","source":"notes"}))
        .await
        .unwrap();
    assert_eq!(from_files["origin"], "tool_output");
    journal_read(
        &broker,
        "fetch",
        "run_command",
        json!({"program":"curl","args":["https://example.com"],"network":true,"justification":"docs"}),
    );
    let from_web = remember(&broker, "two", json!({"content":"B","source":"docs"}))
        .await
        .unwrap();
    assert_eq!(from_web["origin"], "web_content");
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event, EngineEvent::ApprovalRequired { .. }));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recall_skips_untrusted_facts_but_searching_them_counts_as_reading_them() {
    let (_directory, broker, _events) = fixture_with(PermissionMode::FullAccess, |request| {
        request.prompt = "Which deploy target does the project use?".into();
    });
    fact(
        &broker,
        "The deploy target is staging",
        MemoryOrigin::UserTyped,
    );
    fact(
        &broker,
        "The deploy target is production",
        MemoryOrigin::WebContent,
    );
    let recalled = broker.project_facts().unwrap();
    assert_eq!(recalled.len(), 1);
    assert_eq!(recalled[0].record.content, "The deploy target is staging");
    assert_eq!(broker.exposure().unwrap(), MemoryOrigin::AgentInferred);
    let found: Value = serde_json::from_str(
        &run(
            &broker,
            "search",
            "memory_search",
            json!({"query":"deploy target"}),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(found["summary"].as_array().unwrap().len(), 2, "{found}");
    assert_eq!(broker.exposure().unwrap(), MemoryOrigin::WebContent);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrections_name_the_fact_and_the_reason_and_keep_its_history() {
    let (_directory, broker, _events) = fixture(PermissionMode::FullAccess);
    let inferred = fact(
        &broker,
        "The cache lives in tmp",
        MemoryOrigin::AgentInferred,
    );
    let typed = fact(&broker, "Releases ship on Fridays", MemoryOrigin::UserTyped);
    let missing_reason = remember(
        &broker,
        "no-reason",
        json!({"content":"The cache lives in target","source":"build.rs","supersedes":inferred.id}),
    )
    .await
    .unwrap_err();
    assert!(
        missing_reason.to_string().contains("reason"),
        "{missing_reason}"
    );
    let corrected = remember(
        &broker,
        "corrected",
        json!({"content":"The cache lives in target","source":"build.rs","supersedes":inferred.id,"reason":"build.rs moved it"}),
    )
    .await
    .unwrap();
    assert_eq!(corrected["supersedes"], json!(inferred.id));
    assert!(
        corrected["source"]
            .as_str()
            .unwrap()
            .contains("build.rs moved it")
    );
    let history: Value = serde_json::from_str(
        &run(
            &broker,
            "history",
            "memory_search",
            json!({"query":"cache lives"}),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        history["summary"][0]["previously"]["content"],
        "The cache lives in tmp"
    );
    let overruled = remember(
        &broker,
        "overrule",
        json!({"content":"Releases ship on Mondays","source":"guess","supersedes":typed.id,"reason":"guess"}),
    )
    .await
    .unwrap_err();
    assert!(
        overruled.to_string().contains("stated by the user"),
        "{overruled}"
    );
    let invalid = remember(
        &broker,
        "invalid",
        json!({"content":"x","source":"y","supersedes":"not-an-id","reason":"z"}),
    )
    .await
    .unwrap_err();
    assert!(invalid.to_string().contains("fact id"), "{invalid}");
}

#[test]
fn commands_count_as_project_output_only_when_they_stay_in_the_project() {
    let workspace = std::path::Path::new("/work/project");
    let run = |program: &str, args: &[&str]| {
        command_stays_in_project(
            "run_command",
            &json!({"program":program,"args":args}),
            workspace,
        )
    };
    let shell = |command: &str| {
        command_stays_in_project("shell_command", &json!({"command":command}), workspace)
    };
    assert!(run("cargo", &["test", "--offline"]));
    assert!(run(
        "/usr/bin/grep",
        &["-rn", "todo", "src", "/work/project/docs"]
    ));
    assert!(shell("cargo fmt --check && git diff --stat > /dev/null"));
    for (program, args) in [
        ("cat", &["/etc/hosts"][..]),
        ("cat", &["../secrets.txt"]),
        ("ls", &["~/Downloads"]),
        ("curl", &["-s", "example.com"]),
        ("git", &["pull", "origin"]),
        ("python3", &["tool.py", "--url=https://example.com"]),
        ("cat", &["--file=/etc/passwd"]),
    ] {
        assert!(!run(program, args), "{program} {args:?}");
    }
    for command in [
        "cat $HOME/.netrc",
        "echo `whoami`",
        "head ~/notes.md",
        "cat 'unterminated",
        "wget -qO- example.com | head",
    ] {
        assert!(!shell(command), "{command}");
    }
    assert!(!command_stays_in_project(
        "shell_command",
        &json!({}),
        workspace
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installed_skills_and_outside_commands_taint_but_built_in_skills_do_not() {
    let (_directory, broker, _events) = fixture(PermissionMode::FullAccess);
    let built_in = crate::prompts::bundled_skills()[0].name;
    journal_read(
        &broker,
        "built-in",
        "extension_skill",
        json!({"id":built_in}),
    );
    journal_read(
        &broker,
        "built-in-resource",
        "extension_resource",
        json!({"package":built_in,"path":"x.md"}),
    );
    journal_read(
        &broker,
        "tests",
        "run_command",
        json!({"program":"cargo","args":["test"]}),
    );
    assert_eq!(broker.exposure().unwrap(), MemoryOrigin::AgentInferred);
    journal_read(
        &broker,
        "plugin",
        "extension_skill",
        json!({"id":"pkg:helper"}),
    );
    assert_eq!(broker.exposure().unwrap(), MemoryOrigin::ToolOutput);
    let (_directory, broker, _events) = fixture(PermissionMode::FullAccess);
    journal_read(
        &broker,
        "outside",
        "shell_command",
        json!({"command":"cat /etc/hosts"}),
    );
    assert_eq!(broker.exposure().unwrap(), MemoryOrigin::ToolOutput);
    let (_directory, broker, _events) = fixture(PermissionMode::FullAccess);
    journal_read(&broker, "unknown", "skill_read", json!({"name":"missing"}));
    journal_read(&broker, "list", "skill_list", json!({}));
    assert_eq!(broker.exposure().unwrap(), MemoryOrigin::ToolOutput);
}
