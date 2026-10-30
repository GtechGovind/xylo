use super::*;
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;
use xylo_core::{ChatMessage, ConnectorConfig, Conversation, MemoryRecord, now_ms};

fn database() -> (tempfile::TempDir, Store, Uuid) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("xylo.db")).unwrap();
    let id = Uuid::new_v4();
    store
        .upsert_conversation(&Conversation {
            id,
            title: "Durable session".into(),
            workspace: directory.path().to_owned(),
            updated_at: now_ms(),
        })
        .unwrap();
    (directory, store, id)
}
fn intent(session_id: Uuid, writes: bool) -> ActionIntent {
    ActionIntent {
        id: Uuid::new_v4(),
        session_id,
        turn_id: Uuid::new_v4(),
        name: "test_action".into(),
        arguments: json!({"amount":42}),
        writes,
        idempotency_key: "stable-call-1".into(),
    }
}
fn memory(scope: &str, text: &str) -> MemoryRecord {
    MemoryRecord {
        id: Uuid::new_v4(),
        scope: scope.into(),
        content: text.into(),
        source: "test fixture".into(),
        created_at: now_ms(),
        supersedes: None,
    }
}
fn vault() -> CredentialVault {
    CredentialVault::from_test_store("xylo-tests", keyring_core::mock::Store::new().unwrap())
}

#[test]
fn history_and_checkpoint_survive_reopen() {
    let (directory, store, session) = database();
    let turn = Uuid::new_v4();
    let message = ChatMessage {
        attachments: Vec::new(),
        id: Uuid::new_v4(),
        session_id: session,
        role: "user".into(),
        content: "resume me".into(),
        created_at: now_ms(),
    };
    store.append_message(&message).unwrap();
    store
        .save_checkpoint(
            session,
            turn,
            &json!([{"role":"user","content":"engine-private-state"}]),
        )
        .unwrap();
    let restored = Store::open(directory.path().join("xylo.db")).unwrap();
    assert_eq!(
        restored.messages(session, 50).unwrap()[0].content,
        "resume me"
    );
    let checkpoint = restored.checkpoint(session).unwrap().unwrap();
    assert_eq!(checkpoint.turn_id, turn);
    assert_eq!(checkpoint.messages[0]["content"], "engine-private-state");
    let durability: i64 = store
        .call(|c| Ok(c.pragma_query_value(None, "synchronous", |r| r.get(0))?))
        .unwrap();
    assert_eq!(durability, 2, "journal must use FULL durability");
}

#[test]
fn history_pages_preserve_append_order_with_tied_timestamps_and_concurrent_append() {
    let (_directory, store, session) = database();
    for index in 1..=6 {
        store
            .append_message(&ChatMessage {
                attachments: Vec::new(),
                id: Uuid::new_v4(),
                session_id: session,
                role: "user".into(),
                content: index.to_string(),
                created_at: 7,
            })
            .unwrap();
    }
    let newest = store.messages_page(session, None, 2).unwrap();
    assert_eq!(
        newest
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        ["5", "6"]
    );
    assert!(newest.has_more);
    store
        .append_message(&ChatMessage {
            attachments: Vec::new(),
            id: Uuid::new_v4(),
            session_id: session,
            role: "assistant".into(),
            content: "newly appended".into(),
            created_at: 1,
        })
        .unwrap();
    let middle = store.messages_page(session, newest.before, 2).unwrap();
    assert_eq!(
        middle
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        ["3", "4"]
    );
    assert!(middle.has_more);
    let oldest = store.messages_page(session, middle.before, 2).unwrap();
    assert_eq!(
        oldest
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        ["1", "2"]
    );
    assert!(!oldest.has_more);
    assert!(oldest.before.is_none());
    let latest = store.messages_page(session, None, 0).unwrap();
    assert_eq!(latest.messages.len(), 1);
    assert_eq!(latest.messages[0].content, "newly appended");
    assert!(
        store
            .messages_page(Uuid::new_v4(), None, 200)
            .unwrap()
            .messages
            .is_empty()
    );
}

#[test]
fn conversation_pages_use_timestamp_and_uuid_tiebreaker_without_offsets() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("conversation-pages.db")).unwrap();
    for id in 1..=5 {
        store
            .upsert_conversation(&Conversation {
                id: Uuid::from_u128(id),
                title: format!("Chat {id}"),
                workspace: directory.path().into(),
                updated_at: 42,
            })
            .unwrap();
    }
    let first = store.conversations_page(None, 2).unwrap();
    assert_eq!(
        first
            .conversations
            .iter()
            .map(|chat| chat.id.as_u128())
            .collect::<Vec<_>>(),
        [5, 4]
    );
    assert!(first.has_more);
    store
        .upsert_conversation(&Conversation {
            id: Uuid::from_u128(6),
            title: "Newer chat".into(),
            workspace: directory.path().into(),
            updated_at: 99,
        })
        .unwrap();
    let second = store.conversations_page(first.before, 2).unwrap();
    assert_eq!(
        second
            .conversations
            .iter()
            .map(|chat| chat.id.as_u128())
            .collect::<Vec<_>>(),
        [3, 2]
    );
    let last = store.conversations_page(second.before, 2).unwrap();
    assert_eq!(last.conversations[0].id.as_u128(), 1);
    assert!(!last.has_more);
    assert!(last.before.is_none());
    assert_eq!(
        store
            .conversations_page(None, 0)
            .unwrap()
            .conversations
            .len(),
        1
    );
}

#[test]
fn user_turn_registration_is_atomic_idempotent_and_preserves_conversation_title() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("turn-history.db");
    let host = Store::open(&database_path).unwrap();
    let worker = Store::open(&database_path).unwrap();
    let mut turn = xylo_core::TurnRequest {
        attachments: Vec::new(),
        session_id: Uuid::new_v4(),
        turn_id: Uuid::new_v4(),
        prompt: "First conversation title".into(),
        workspace: directory.path().join("original-workspace"),
        provider: Default::default(),
        permissions: Default::default(),
    };
    host.register_user_turn(&turn).unwrap();
    worker.register_user_turn(&turn).unwrap();
    let messages = host.messages(turn.session_id, 20).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].id, Uuid::new_v5(&turn.turn_id, b"user-message"));
    assert_eq!(messages[0].content, turn.prompt);
    let first_time = messages[0].created_at;
    let original_workspace = turn.workspace.clone();
    turn.turn_id = Uuid::new_v4();
    turn.prompt = "A later question should not rename this chat".into();
    turn.workspace = directory.path().join("different-workspace");
    worker.register_user_turn(&turn).unwrap();
    host.register_user_turn(&turn).unwrap();
    let conversations = host.conversations(10).unwrap();
    assert_eq!(conversations.len(), 1);
    assert_eq!(conversations[0].title, "First conversation title");
    assert_eq!(conversations[0].workspace, original_workspace);
    let messages = host.messages(turn.session_id, 20).unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].created_at, first_time);
    assert_eq!(messages[1].content, turn.prompt);

    turn.prompt = "Accidentally changed resumed input".into();
    assert!(host.register_user_turn(&turn).is_err());
    assert_eq!(host.messages(turn.session_id, 20).unwrap().len(), 2);
    turn.session_id = Uuid::new_v4();
    assert!(host.register_user_turn(&turn).is_err());
    assert_eq!(host.conversations(10).unwrap().len(), 1);
}

#[test]
fn successful_action_is_reused_and_changed_arguments_rejected() {
    let (_directory, store, session) = database();
    let action = intent(session, true);
    assert_eq!(
        store.begin_action(&action).unwrap(),
        ActionDecision::Execute
    );
    assert_eq!(
        store.begin_action(&action).unwrap(),
        ActionDecision::InFlight
    );
    store.mark_action_running(action.id).unwrap();
    store
        .finish_action(action.id, &json!({"remote_id":"123"}))
        .unwrap();
    assert_eq!(
        store.begin_action(&action).unwrap(),
        ActionDecision::Reuse(json!({"remote_id":"123"}))
    );
    let mut changed = action;
    changed.arguments = json!({"amount":43});
    assert!(store.begin_action(&changed).is_err());
}
#[test]
fn interrupted_write_is_unknown_until_evidence_is_recorded() {
    let (directory, store, session) = database();
    let action = intent(session, true);
    store.begin_action(&action).unwrap();
    store.mark_action_running(action.id).unwrap();
    let restarted = Store::open(directory.path().join("xylo.db")).unwrap();
    assert_eq!(restarted.recover_interrupted().unwrap(), 1);
    assert_eq!(
        restarted.begin_action(&action).unwrap(),
        ActionDecision::OutcomeUnknown
    );
    assert!(
        restarted
            .reconcile_action(action.id, Some(&json!({"id":"confirmed"})), "")
            .is_err()
    );
    restarted
        .reconcile_action(
            action.id,
            Some(&json!({"id":"confirmed"})),
            "Provider lookup confirmed id",
        )
        .unwrap();
    assert_eq!(
        store.begin_action(&action).unwrap(),
        ActionDecision::Reuse(json!({"id":"confirmed"}))
    );
    assert!(store.unresolved_actions(session).unwrap().is_empty());
}

#[test]
fn unsuccessful_observations_keep_artifacts_after_restart_and_reconciliation() {
    let (directory, store, session) = database();
    for unknown in [false, true] {
        let mut action = intent(session, unknown);
        action.idempotency_key = format!("partial-result-{unknown}");
        store.begin_action(&action).unwrap();
        store.mark_action_running(action.id).unwrap();
        let observation = json!({"status":if unknown {"outcome_unknown"} else {"failed"},
            "summary":"A partial file was committed", "artifacts":["partial.txt"]});
        if unknown {
            store
                .mark_action_unknown_with_result(action.id, &observation, "Unconfirmed write")
                .unwrap();
        } else {
            store
                .fail_action_with_result(action.id, &observation, "Read failed")
                .unwrap();
        }
        let reopened = Store::open(directory.path().join("xylo.db")).unwrap();
        assert_eq!(
            reopened.action(action.id).unwrap().unwrap().result,
            Some(observation.clone())
        );
        if unknown {
            assert_eq!(
                reopened.begin_action(&action).unwrap(),
                ActionDecision::OutcomeUnknown
            );
            reopened
                .reconcile_action(
                    action.id,
                    None,
                    "Inspected the actual target; command failed after writing partial.txt",
                )
                .unwrap();
            assert_eq!(
                reopened.action(action.id).unwrap().unwrap().result,
                Some(observation)
            );
        }
        assert!(reopened.begin_action(&action).is_err());
    }
}

#[test]
fn reaped_worker_recovery_is_session_scoped_and_preserves_confirmed_results() {
    let (_directory, store, session) = database();
    let running_write = intent(session, true);
    let mut child_write = intent(session, true);
    child_write.idempotency_key = "child:8:delegate:call-1".into();
    let running_read = intent(session, false);
    let pending_write = intent(session, true);
    let pending_read = intent(session, false);
    let succeeded = intent(session, true);
    let denied = intent(session, true);
    let foreign = intent(Uuid::new_v4(), true);
    let mut foreign_conversation = store.conversations(1).unwrap().remove(0);
    foreign_conversation.id = foreign.session_id;
    store.upsert_conversation(&foreign_conversation).unwrap();
    for action in [
        &running_write,
        &child_write,
        &running_read,
        &pending_write,
        &pending_read,
        &succeeded,
        &denied,
        &foreign,
    ] {
        store.begin_action(action).unwrap();
    }
    for action in [
        &running_write,
        &child_write,
        &running_read,
        &succeeded,
        &foreign,
    ] {
        store.mark_action_running(action.id).unwrap();
    }
    store
        .finish_action(succeeded.id, &json!({"remote_id":"confirmed"}))
        .unwrap();
    store.deny_action(denied.id).unwrap();
    let foreign_before = serde_json::to_value(store.action(foreign.id).unwrap()).unwrap();
    let denied_before = serde_json::to_value(store.action(denied.id).unwrap()).unwrap();
    assert_eq!(store.recover_session_interrupted(session).unwrap(), 5);
    for action in [&running_write, &child_write] {
        assert_eq!(
            store.begin_action(action).unwrap(),
            ActionDecision::OutcomeUnknown
        );
    }
    for action in [&running_read, &pending_write, &pending_read] {
        assert_eq!(store.action(action.id).unwrap().unwrap().state, "failed");
        assert!(
            store.begin_action(action).is_err(),
            "recovery must not redispatch"
        );
    }
    assert_eq!(
        store.begin_action(&succeeded).unwrap(),
        ActionDecision::Reuse(json!({"remote_id":"confirmed"}))
    );
    assert_eq!(
        serde_json::to_value(store.action(foreign.id).unwrap()).unwrap(),
        foreign_before
    );
    assert_eq!(
        serde_json::to_value(store.action(denied.id).unwrap()).unwrap(),
        denied_before
    );
    assert_eq!(
        store.begin_action(&foreign).unwrap(),
        ActionDecision::InFlight
    );
    assert_eq!(store.recover_session_interrupted(session).unwrap(), 0);
    store
        .reconcile_action(
            running_write.id,
            None,
            "Provider confirmed no external effect",
        )
        .unwrap();
    assert_eq!(
        store.action(running_write.id).unwrap().unwrap().state,
        "failed"
    );
}

#[test]
fn denial_and_pending_recovery_never_execute_implicitly() {
    let (_directory, store, session) = database();
    let denied = intent(session, true);
    store.begin_action(&denied).unwrap();
    store.deny_action(denied.id).unwrap();
    assert!(store.mark_action_running(denied.id).is_err());
    assert!(store.begin_action(&denied).is_err());
    let pending = intent(session, false);
    store.begin_action(&pending).unwrap();
    assert_eq!(store.recover_interrupted().unwrap(), 1);
    assert!(store.begin_action(&pending).is_err());
}
#[test]
fn memory_is_scoped_superseded_and_forgotten_atomically() {
    let (_directory, store, _) = database();
    let backend = SqliteMemory::new(store);
    let first = memory("project-a", "The deployment branch is staging");
    backend.remember_sync(first.clone()).unwrap();
    backend
        .remember_sync(memory("project-b", "The deployment branch is production"))
        .unwrap();
    assert_eq!(
        backend
            .recall_sync("project-a", "deployment", 10)
            .unwrap()
            .len(),
        1
    );
    let mut invalid = memory("project-b", "cross scope overwrite");
    invalid.supersedes = Some(first.id);
    assert!(backend.remember_sync(invalid).is_err());
    assert_eq!(
        backend.recall_sync("project-a", "deployment", 10).unwrap()[0].id,
        first.id
    );
    let mut replacement = memory("project-a", "The deployment branch is release");
    replacement.supersedes = Some(first.id);
    backend.remember_sync(replacement.clone()).unwrap();
    let found = backend.recall_sync("project-a", "deployment", 10).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, replacement.id);
    backend.forget_sync(replacement.id).unwrap();
    assert!(
        backend
            .recall_sync("project-a", "deployment", 10)
            .unwrap()
            .is_empty()
    );
    assert!(
        backend
            .recall_sync("project-b", "deployment", 10)
            .unwrap()
            .len()
            == 1
    );
}
#[test]
fn lexical_memory_accepts_punctuation_without_loading_model() {
    let (_directory, store, _) = database();
    let memory = SqliteMemory::new(store);
    memory
        .remember_sync(super::tests::memory(
            "repo",
            "PaymentService card reader configuration",
        ))
        .unwrap();
    assert_eq!(
        memory
            .recall_sync("repo", "PaymentService \"configuration\"", 10)
            .unwrap()
            .len(),
        1
    );
    assert!(
        memory
            .enable_embeddings(EmbeddingAssets {
                directory: "/no-such-model-assets".into(),
                model_id: "fixture".into(),
                dimensions: 384,
                cpu_threads: 2,
                pooling: EmbeddingPooling::Mean,
                runtime_library: None,
            })
            .is_err()
    );
    assert!(memory.reindex("repo", 10).is_err());
}
#[test]
fn skill_promotion_requires_evaluation_and_rollback_preserves_history() {
    let (_directory, store, _) = database();
    let v1 = store
        .create_skill_version("review", "Check the actual diff")
        .unwrap();
    assert!(store.promote_skill("review", v1.version).is_err());
    let evaluation = SkillEvaluation {
        passed: true,
        score: 1.0,
        cases: 3,
        evidence: "fixture suite: 3/3 passed".into(),
    };
    store
        .evaluate_skill("review", v1.version, evaluation.clone())
        .unwrap();
    store.promote_skill("review", v1.version).unwrap();
    let v2 = store
        .create_skill_version("review", "Check diff and request paths")
        .unwrap();
    store
        .evaluate_skill("review", v2.version, evaluation)
        .unwrap();
    store.promote_skill("review", v2.version).unwrap();
    assert_eq!(store.active_skill("review").unwrap().unwrap().version, 2);
    store.rollback_skill("review", 1).unwrap();
    assert_eq!(store.active_skill("review").unwrap().unwrap().version, 1);
    assert_eq!(store.skill_versions("review").unwrap().len(), 2);
}
#[test]
fn credential_boundary_uses_store_and_has_no_plaintext_fallback() {
    let vault = vault();
    assert_eq!(vault.get("token").unwrap(), None);
    vault.set("token", "secret-fixture").unwrap();
    assert_eq!(
        vault.get("token").unwrap().as_deref(),
        Some("secret-fixture")
    );
    vault.delete("token").unwrap();
    assert_eq!(vault.get("token").unwrap(), None);
    assert!(vault.set("", "secret").is_err());
}

#[test]
fn native_tool_catalogues_declare_mutations_and_bounded_pages() {
    for transport in [
        "github",
        "google_drive",
        "gmail",
        "google_calendar",
        "slack",
    ] {
        let tools = NativeConnectors::tools(transport).unwrap();
        assert!(tools.len() >= 4);
        assert!(tools.iter().any(|t| t.writes));
        assert!(tools.iter().any(|t| !t.writes));
        for tool in tools {
            assert_eq!(tool.parameters["additionalProperties"], false);
        }
    }
}
#[tokio::test]
async fn oauth_rejects_wrong_state_before_network_exchange() {
    let config = OAuthConfig {
        authorization_endpoint: "https://provider.example/authorize".into(),
        token_endpoint: "https://provider.example/token".into(),
        client_id: "public-desktop-client".into(),
        scopes: vec!["read".into()],
    };
    let flow = PendingOAuth::begin(config, "http://127.0.0.1:45678/callback").unwrap();
    assert!(
        flow.authorization_url
            .contains("code_challenge_method=S256")
    );
    assert!(!flow.authorization_url.contains("code_verifier"));
    assert!(
        flow.complete("code", "wrong-state", &Default::default())
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("state mismatch")
    );
}

async fn mock_server(
    handler: Arc<dyn Fn(Value, String) -> (u16, Value) + Send + Sync>,
) -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let handler = handler.clone();
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                let mut buffer = [0u8; 4096];
                let end = loop {
                    let count = socket.read(&mut buffer).await.unwrap_or(0);
                    if count == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buffer[..count]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        break end + 4;
                    }
                    if bytes.len() > 32768 {
                        return;
                    }
                };
                let headers = String::from_utf8_lossy(&bytes[..end]).to_string();
                let length = headers
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|n| n.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                while bytes.len() < end + length {
                    let count = socket.read(&mut buffer).await.unwrap_or(0);
                    if count == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buffer[..count]);
                }
                let request =
                    serde_json::from_slice(&bytes[end..end + length]).unwrap_or(Value::Null);
                let (status, body) = handler(request, headers);
                let body = if status == 204 {
                    String::new()
                } else {
                    body.to_string()
                };
                let response = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    (format!("http://{address}"), task)
}
#[tokio::test]
async fn native_connector_sends_correct_route_and_reports_provider_failure() {
    let (url, server) = mock_server(Arc::new(|_, headers| {
        assert!(headers.starts_with("GET /search/issues?"));
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("authorization: bearer fixture-token")
        );
        (403, json!({"message":"fixture permission denied"}))
    }))
    .await;
    let vault = vault();
    vault.set("github-token", "fixture-token").unwrap();
    let connector = NativeConnectors::new(vault).unwrap();
    let config = ConnectorConfig {
        id: Uuid::new_v4(),
        name: "Fixture GitHub".into(),
        transport: "github".into(),
        endpoint: url,
        command: None,
        args: Vec::new(),
        credential_ref: Some("github-token".into()),
    };
    let error = connector
        .call(
            &config,
            "github_search_issues",
            json!({"query":"repo:owner/repo"}),
            &Default::default(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("403"));
    assert!(!error.to_string().contains("fixture-token"));
    server.abort();
}
#[tokio::test]
async fn mcp_discovery_call_and_metadata_cache_use_real_protocol() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let initialized = Arc::new(AtomicUsize::new(0));
    let counter = initialized.clone();
    let (url,server)=mock_server(Arc::new(move|request,_|{let Some(method)=request.get("method").and_then(Value::as_str)else{return(400,json!({}))};let id=request.get("id").cloned();let result=match method{
        "initialize"=>{counter.fetch_add(1,Ordering::SeqCst);json!({"protocolVersion":request["params"]["protocolVersion"],"capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}})},
        "notifications/initialized"=>return(204,Value::Null),
        "tools/list"=>json!({"tools":[{"name":"echo","description":"Fixture echo","inputSchema":{"type":"object","properties":{"value":{"type":"string"}}},"annotations":{"readOnlyHint":true}}]}),
        "tools/call"=>json!({"content":[{"type":"text","text":request["params"]["arguments"]["value"]}]}),
        _=>return(404,json!({})),};(200,json!({"jsonrpc":"2.0","id":id,"result":result}))})).await;
    let (_directory, store, _) = database();
    let broker = ConnectorBroker::new(store.clone(), vault());
    let config = ConnectorConfig {
        id: Uuid::new_v4(),
        name: "Fixture MCP".into(),
        transport: "http".into(),
        endpoint: url,
        command: None,
        args: Vec::new(),
        credential_ref: None,
    };
    assert!(broker.cached_tools(&config).unwrap().is_empty());
    let tools = broker.tools(&config, &Default::default()).await.unwrap();
    assert_eq!(tools[0].name, "echo");
    assert!(tools[0].writes, "Untrusted MCP readOnlyHint cannot authorize read-only execution");
    assert_eq!(broker.cached_tools(&config).unwrap().len(), 1);
    store.call(|connection| { connection.execute("UPDATE connector_tools SET json=replace(json, '\"writes\":true', '\"writes\":false')", [])?; Ok(()) }).unwrap();
    assert!(broker.cached_tools(&config).unwrap()[0].writes, "Old cached annotations cannot bypass the host write gate");
    let result = broker
        .call(
            &config,
            "echo",
            json!({"value":"protocol works"}),
            &Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(result["content"][0]["text"], "protocol works");
    assert_eq!(initialized.load(Ordering::SeqCst), 1);
    broker.disconnect(config.id).await.unwrap();
    server.abort();
}

#[test]
fn completion_receipt_checkpoint_and_assistant_history_commit_once() {
    let (directory, store, session) = database();
    let turn = Uuid::new_v4();
    let usage = xylo_core::Usage {
        input_tokens: 10,
        output_tokens: 20,
        elapsed_ms: 30,
    };
    let checkpoint = json!([{"role":"assistant","content":"finished"}]);
    store
        .complete_turn(session, turn, &checkpoint, "finished", &usage)
        .unwrap();
    let reopened = Store::open(directory.path().join("xylo.db")).unwrap();
    assert_eq!(
        reopened
            .completed_turn(session, turn)
            .unwrap()
            .unwrap()
            .text,
        "finished"
    );
    assert_eq!(
        reopened.checkpoint(session).unwrap().unwrap().messages,
        checkpoint
    );
    assert_eq!(reopened.messages(session, 10).unwrap().len(), 1);
    reopened
        .complete_turn(session, turn, &checkpoint, "finished", &usage)
        .unwrap();
    assert_eq!(reopened.messages(session, 10).unwrap().len(), 1);
    assert!(
        reopened
            .complete_turn(session, turn, &checkpoint, "different", &usage)
            .is_err()
    );
}
#[test]
fn schema_one_database_migrates_without_losing_history() {
    let (directory, store, session) = database();
    store
        .call(|conn| {
            conn.execute_batch("DROP TABLE completed_turns; PRAGMA user_version=1;")?;
            Ok(())
        })
        .unwrap();
    let migrated = Store::open(directory.path().join("xylo.db")).unwrap();
    assert_eq!(migrated.conversations(10).unwrap()[0].id, session);
    assert!(
        migrated
            .completed_turn(session, Uuid::new_v4())
            .unwrap()
            .is_none()
    );
    let version: i64 = migrated
        .call(|conn| Ok(conn.pragma_query_value(None, "user_version", |r| r.get(0))?))
        .unwrap();
    assert_eq!(version, 3);
}

#[tokio::test]
async fn authenticated_http_mcp_uses_isolated_vault_and_rejects_missing_token() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = accepted.clone();
    let (url, server) = mock_server(Arc::new(move |request, headers| {
        if !headers.to_ascii_lowercase().contains("authorization: bearer isolated-mcp-token") {
            return (401, json!({"error":"Authentication required"}));
        }
        counter.fetch_add(1, Ordering::SeqCst);
        let id = request.get("id").cloned();
        let result = match request["method"].as_str().unwrap_or_default() {
            "initialize" => json!({"protocolVersion":request["params"]["protocolVersion"],"capabilities":{"tools":{}},"serverInfo":{"name":"authenticated-fixture","version":"1"}}),
            "notifications/initialized" => return (204, Value::Null),
            "tools/list" => json!({"tools":[{"name":"secured_tool","inputSchema":{"type":"object"}}]}),
            _ => return (404,json!({})),
        };
        (200,json!({"jsonrpc":"2.0","id":id,"result":result}))
    })).await;
    let (_directory, store, _) = database();
    let vault = vault(); // isolated keyring mock; never opens the platform keychain
    let broker = ConnectorBroker::new(store, vault.clone());
    let mut config = ConnectorConfig { id:Uuid::new_v4(),name:"Authenticated fixture".into(),transport:"http".into(),endpoint:url,command:None,args:vec![],credential_ref:None };
    assert!(broker.tools(&config,&Default::default()).await.is_err());
    assert_eq!(accepted.load(Ordering::SeqCst),0);
    vault.set("isolated-mcp-reference","isolated-mcp-token").unwrap();
    config.credential_ref = Some("isolated-mcp-reference".into());
    let tools = broker.tools(&config,&Default::default()).await.unwrap();
    assert_eq!(tools[0].name,"secured_tool");
    assert!(accepted.load(Ordering::SeqCst) >= 3);
    server.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn scoped_mcp_reuses_only_matching_workspace_and_environment() {
    use std::os::unix::fs::PermissionsExt;
    let (directory,store,_) = database();
    let first=directory.path().join("first");
    let second=directory.path().join("second");
    std::fs::create_dir_all(&first).unwrap();
    std::fs::create_dir_all(&second).unwrap();
    let script=directory.path().join("scoped-mcp");
    std::fs::write(&script,r#"#!/usr/bin/python3
import json,sys,pathlib,os
marker=pathlib.Path('launches.txt')
marker.write_text(str(int(marker.read_text())+1 if marker.exists() else 1))
for line in sys.stdin:
    request=json.loads(line)
    if 'id' not in request: continue
    if request['method']=='initialize': result={'protocolVersion':request['params']['protocolVersion'],'capabilities':{'tools':{}},'serverInfo':{'name':'scoped-fixture','version':'1'}}
    elif request['method']=='tools/list': result={'tools':[{'name':pathlib.Path.cwd().name+'_'+os.environ['FIXTURE_SCOPE'],'inputSchema':{'type':'object'}}]}
    else: result={}
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}),flush=True)
"#).unwrap();
    std::fs::set_permissions(&script,std::fs::Permissions::from_mode(0o755)).unwrap();
    let broker=ConnectorBroker::new(store.clone(),vault());
    let config=ConnectorConfig { id:Uuid::new_v4(),name:"Scoped fixture".into(),transport:"stdio".into(),endpoint:String::new(),command:Some(script.clone()),args:vec![],credential_ref:None };
    let mut scope=crate::ConnectorLaunchScope { workspace:first.clone(),read_roots:vec![script],environment:std::collections::BTreeMap::from([("FIXTURE_SCOPE".into(),"one".into())]),..Default::default() };
    assert_eq!(broker.tools_scoped(&config,&scope,&Default::default()).await.unwrap()[0].name,"first_one");
    let first_broker=ConnectorBroker::new(store.clone(),vault()).with_local_scope(scope.clone());
    assert_eq!(first_broker.cached_tools(&config).unwrap()[0].name,"first_one","Matching local scope must retain useful cached metadata");
    assert_eq!(broker.tools_scoped(&config,&scope,&Default::default()).await.unwrap()[0].name,"first_one");
    assert_eq!(std::fs::read_to_string(first.join("launches.txt")).unwrap(),"1","Matching scope should reuse its process");
    scope.workspace=second.clone();
    let second_broker=ConnectorBroker::new(store.clone(),vault()).with_local_scope(scope.clone());
    assert!(second_broker.cached_tools(&config).unwrap().is_empty(),"A catalog from workspace A must not be exposed in workspace B");
    assert_eq!(broker.tools_scoped(&config,&scope,&Default::default()).await.unwrap()[0].name,"second_one");
    assert_eq!(second_broker.cached_tools(&config).unwrap()[0].name,"second_one");
    assert!(first_broker.cached_tools(&config).unwrap().is_empty());
    assert_eq!(std::fs::read_to_string(second.join("launches.txt")).unwrap(),"1");
    scope.environment.insert("FIXTURE_SCOPE".into(),"two".into());
    assert_eq!(broker.tools_scoped(&config,&scope,&Default::default()).await.unwrap()[0].name,"second_two");
    assert!(second_broker.cached_tools(&config).unwrap().is_empty(),"Old environment must not read new policy catalog");
    assert_eq!(std::fs::read_to_string(second.join("launches.txt")).unwrap(),"2","Changed environment must reconnect");
    broker.disconnect(config.id).await.unwrap();
}
