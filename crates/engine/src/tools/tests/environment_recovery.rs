//! Exercises the two durable journals through their public execution boundaries.
//! The executable is a local fixture; no container daemon or network is involved.
#![cfg(unix)]

use super::super::*;
use std::{fs, os::unix::fs::PermissionsExt};
use xylo_capabilities::environments::execute_management;
use xylo_core::{
    ContainerRuntime, ContainerTarget, Conversation, EngineBackend, EnvironmentOperationState,
    EnvironmentProfile, EnvironmentReceipt, EnvironmentRecord, EnvironmentTarget, ProviderConfig,
    now_ms,
};

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    config: CapabilityConfig,
    service: CapabilityService,
    broker: Arc<ToolBroker>,
    events: tokio::sync::mpsc::Receiver<EngineEvent>,
    environment: EnvironmentRecord,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let executable = root.join("fake-container");
        let quoted_root = format!("'{}'", root.to_str().unwrap().replace('\'', "'\\''"));
        let script = r#"#!/bin/sh
set -eu
root=FIXTURE_ROOT
printf '<%s>\n' "$@" >> "$root/argv"
[ "$1" = container ] || exit 99
verb=$2
shift 2
case "$verb" in
 inspect)
  printf '{"Id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","Config":{"Labels":{}},"State":{"Running":true}}'
  ;;
 exec)
  printf 'executed\n' >> "$root/executions"
  case "$*" in
   *'__hang__'*) printf 'captured prefix' ; printf 'captured diagnostic' >&2; touch "$root/dispatched"; sleep 20;;
   *'__fail__'*) printf 'fixture command failed' >&2; exit 7;;
   *'__uncertain__'*) printf 'fixture transport uncertain' >&2; exit 125;;
   *) printf 'fixture command completed';;
  esac
  ;;
 *) exit 98;;
esac
"#;
        fs::write(&executable, script.replace("FIXTURE_ROOT", &quoted_root)).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let config = CapabilityConfig {
            workspace: root.clone(),
            artifacts: root.join("artifacts"),
            management_root: Some(root.join("native-management")),
            ..Default::default()
        };
        let profile = EnvironmentProfile {
            id: Uuid::new_v4(),
            name: "Recovery fixture".into(),
            target: EnvironmentTarget::Container {
                executable,
                runtime: ContainerRuntime::Docker,
                container: ContainerTarget::Existing {
                    target: "explicit-fixture-container".into(),
                },
            },
        };
        let saved = management(
            &config,
            "helper_environment_save",
            json!({"operation_id":Uuid::new_v4(),"expected_revision":0,"profile":profile}),
        )
        .await;
        let record = saved.environment.unwrap();
        let connected = management(
            &config,
            "helper_environment_connect",
            json!({"operation_id":Uuid::new_v4(),"environment_id":record.profile.id,"expected_revision":record.revision}),
        )
        .await;
        let environment = connected.environment.unwrap();
        assert!(environment.connected);

        let store = Store::open(root.join("xylo.db")).unwrap();
        let session_id = Uuid::new_v4();
        store
            .upsert_conversation(&Conversation {
                id: session_id,
                title: "Environment recovery".into(),
                workspace: root.clone(),
                updated_at: now_ms(),
                title_source: Default::default(),
                pinned: false,
                archived: false,
            })
            .unwrap();
        let request = TurnRequest {
            session_id,
            turn_id: Uuid::new_v4(),
            prompt: "Resume the already approved environment command".into(),
            workspace: root.clone(),
            provider: ProviderConfig::default(),
            attachments: Vec::new(),
            permissions: PermissionMode::AskForApproval,
            approved_rules: Vec::new(),
        };
        let (sender, events) = tokio::sync::mpsc::channel(64);
        let broker = Arc::new(
            ToolBroker::new(
                request,
                store,
                config.clone(),
                ToolSession::for_tests(PermissionMode::AskForApproval),
                sender,
                CancellationToken::new(),
            )
            .unwrap(),
        );
        Self {
            _directory: directory,
            root,
            service: CapabilityService::new(config.clone()).unwrap(),
            config,
            broker,
            events,
            environment,
        }
    }

    fn arguments(&self, command: &str) -> Value {
        json!({
            "operation_id":Uuid::new_v4(),
            "environment_id":self.environment.profile.id,
            "expected_revision":self.environment.revision,
            "command":[command,"one argument with spaces"],
            "timeout_seconds":5
        })
    }

    fn running_outer(&self, call_id: &str, arguments: &Value) -> Uuid {
        let intent = ActionIntent {
            id: Uuid::new_v4(),
            session_id: self.broker.request.session_id,
            turn_id: self.broker.request.turn_id,
            name: "environment_execute".into(),
            arguments: arguments.clone(),
            writes: true,
            idempotency_key: ToolBroker::journal_key(None, call_id),
        };
        assert!(matches!(
            self.broker.store.begin_action(&intent).unwrap(),
            ActionDecision::Execute
        ));
        // Represents the persisted approval and native dispatch before the lost ACK.
        self.broker.store.mark_action_running(intent.id).unwrap();
        intent.id
    }

    fn lose_acknowledgement(&self, id: Uuid) {
        self.broker
            .store
            .mark_action_unknown(id, "Fixture lost the outer completion acknowledgement")
            .unwrap();
    }

    async fn retry(&self, call_id: &str, arguments: &Value) -> Result<String> {
        let broker = self.broker.clone();
        let call_id = call_id.to_owned();
        let arguments = arguments.clone();
        tokio::task::spawn_blocking(move || {
            broker.execute(&call_id, "environment_execute", arguments)
        })
        .await
        .unwrap()
    }

    async fn approve_execution(&mut self, call_id: &str, arguments: &Value) -> Result<String> {
        let broker = self.broker.clone();
        let call_id = call_id.to_owned();
        let arguments = arguments.clone();
        let mut execution = tokio::task::spawn_blocking(move || {
            broker.execute(&call_id, "environment_execute", arguments)
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    result = &mut execution => return result.unwrap(),
                    event = self.events.recv() => {
                        if let EngineEvent::ApprovalRequired {action_id,..} = event.unwrap() {
                            self.broker.session.approvals().decide_with(action_id, true, false).unwrap();
                        }
                    }
                }
            }
        })
        .await
        .expect("Fixture must receive approval and finish within its bounded deadline")
    }

    fn set_output_budget(&mut self, bytes: usize) {
        self.config.max_output_bytes = bytes;
        let (events, receiver) = tokio::sync::mpsc::channel(64);
        self.broker = Arc::new(
            ToolBroker::new(
                self.broker.request.clone(),
                self.broker.store.clone(),
                self.config.clone(),
                ToolSession::for_tests(PermissionMode::AskForApproval),
                events,
                CancellationToken::new(),
            )
            .unwrap(),
        );
        self.events = receiver;
    }

    fn argv(&self) -> Vec<u8> {
        fs::read(self.root.join("argv")).unwrap()
    }

    fn assert_single_execution(&self, argv: &[u8]) {
        assert_eq!(self.argv(), argv, "Recovery must not launch even a probe");
        assert_eq!(
            fs::read_to_string(self.root.join("executions"))
                .unwrap()
                .lines()
                .count(),
            1,
            "The approved command must run exactly once"
        );
    }

    fn assert_retained_artifacts(&self, observation: &Value, complete: bool) {
        let receipt = receipt(observation);
        let paths = self.assert_retained_receipt(&receipt, complete);
        assert_eq!(
            observation["artifacts"],
            serde_json::to_value(paths).unwrap()
        );
    }

    fn assert_retained_receipt(
        &self,
        receipt: &EnvironmentReceipt,
        complete: bool,
    ) -> Vec<PathBuf> {
        use sha2::{Digest, Sha256};
        let output = receipt.output.as_ref().unwrap_or_else(|| {
            panic!("Finalized receipt must retain its captured output: {receipt:?}")
        });
        assert_eq!(output.artifacts.len(), 2);
        output
            .artifacts
            .iter()
            .map(|artifact| {
                assert_eq!(artifact.complete, complete);
                let bytes = fs::read(&artifact.path).unwrap();
                assert_eq!(artifact.stored_bytes, bytes.len() as u64);
                assert_eq!(artifact.sha256, hex::encode(Sha256::digest(&bytes)));
                artifact.path.clone()
            })
            .collect()
    }

    async fn retained_receipt(&self, operation_id: Uuid) -> EnvironmentReceipt {
        let mut last = None;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                // This is an observation of the exact saved operation. It must
                // never execute the original command or a native status probe.
                let current = management(
                    &self.config,
                    "helper_environment_operation",
                    json!({"operation_id":operation_id,"environment_id":self.environment.profile.id,"action":"execute"}),
                )
                .await;
                assert_eq!(current.operation_id, operation_id);
                assert_eq!(current.environment_id, self.environment.profile.id);
                assert_eq!(current.action, xylo_core::EnvironmentAction::Execute);
                if current.output.is_some() {
                    return current;
                }
                last = Some(current);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("Retained native output did not finish; last saved receipt: {last:?}"))
    }

    fn assert_recovery_event(&mut self, id: Uuid, expected: &Value, success: bool) {
        match self.events.try_recv().unwrap() {
            EngineEvent::ToolFinished {
                action_id,
                name,
                result,
                success: actual,
                ..
            } => {
                assert_eq!(action_id, id);
                assert_eq!(name, "environment_execute");
                assert_eq!(serde_json::from_str::<Value>(&result).unwrap(), *expected);
                assert_eq!(actual, success);
            }
            event => panic!("Unexpected recovery event: {event:?}"),
        }
        assert!(
            self.events.try_recv().is_err(),
            "Recovery must not request approval or announce another dispatch"
        );
    }
}

async fn management(config: &CapabilityConfig, name: &str, args: Value) -> EnvironmentReceipt {
    serde_json::from_value(
        execute_management(config, name, &args, CancellationToken::new())
            .await
            .unwrap(),
    )
    .unwrap()
}

fn receipt(observation: &Value) -> EnvironmentReceipt {
    serde_json::from_str(observation["summary"].as_str().unwrap()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_outer_ack_recovers_exact_success_and_cached_retry_without_dispatch() {
    let mut fixture = Fixture::new().await;
    let call_id = "lost-success";
    let arguments = fixture.arguments("__success__");
    let id = fixture.running_outer(call_id, &arguments);
    let observed = serde_json::to_value(
        fixture
            .service
            .execute("environment_execute", &arguments, CancellationToken::new())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        receipt(&observed).state,
        EnvironmentOperationState::Succeeded
    );
    fixture.assert_retained_artifacts(&observed, true);
    fixture.lose_acknowledgement(id);
    let argv = fixture.argv();

    for _ in 0..3 {
        let recovered: Value =
            serde_json::from_str(&fixture.retry(call_id, &arguments).await.unwrap()).unwrap();
        assert_eq!(recovered, observed);
        fixture.assert_single_execution(&argv);
    }
    let action = fixture.broker.store.action(id).unwrap().unwrap();
    assert_eq!(action.state, "succeeded");
    assert_eq!(action.result.as_ref(), Some(&observed));
    assert_eq!(action.intent.arguments, arguments);
    fixture.assert_recovery_event(id, &observed, true);
    fixture.broker.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn known_native_failure_resolves_outer_unknown_without_dispatch() {
    let mut fixture = Fixture::new().await;
    let call_id = "lost-failure";
    let arguments = fixture.arguments("__fail__");
    let id = fixture.running_outer(call_id, &arguments);
    let observed = serde_json::to_value(
        fixture
            .service
            .execute("environment_execute", &arguments, CancellationToken::new())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(receipt(&observed).state, EnvironmentOperationState::Failed);
    assert_eq!(receipt(&observed).output.unwrap().exit_code, Some(7));
    fixture.assert_retained_artifacts(&observed, true);
    fixture.lose_acknowledgement(id);
    let argv = fixture.argv();

    for _ in 0..3 {
        let error = fixture.retry(call_id, &arguments).await.unwrap_err();
        assert_eq!(error.downcast_ref::<ObservedFailure>().unwrap().0, observed);
        fixture.assert_single_execution(&argv);
    }
    let action = fixture.broker.store.action(id).unwrap().unwrap();
    assert_eq!(action.state, "failed");
    assert_eq!(action.result.as_ref(), Some(&observed));
    fixture.assert_recovery_event(id, &observed, false);
    fixture.broker.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_native_write_stays_unknown_until_observed_failure_is_recorded() {
    let mut fixture = Fixture::new().await;
    let call_id = "lost-cancelled-write";
    let arguments = fixture.arguments("__hang__");
    let id = fixture.running_outer(call_id, &arguments);
    let cancel = CancellationToken::new();
    let execution = fixture
        .service
        .execute("environment_execute", &arguments, cancel.clone());
    let interrupt = async {
        tokio::time::timeout(Duration::from_secs(3), async {
            while !fixture.root.join("dispatched").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("Fixture must reach the actual native dispatch before cancellation");
        // Hold the fixture's real publication lease after dispatch. This makes
        // the bounded acknowledgement precede finalization deterministically,
        // just as concurrent disk work can do, without replacing any I/O.
        let publication = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(
                fixture
                    .root
                    .join("native-management/environments/captures/storage.lock"),
            )
            .unwrap();
        fs2::FileExt::try_lock_exclusive(&publication).unwrap();
        cancel.cancel();
        publication
    };
    let (observation, publication) = tokio::join!(execution, interrupt);
    let unknown = serde_json::to_value(observation.unwrap()).unwrap();
    assert_eq!(
        receipt(&unknown).state,
        EnvironmentOperationState::OutcomeUnknown
    );
    // The cancellation acknowledgement is bounded to two seconds while the
    // retained task still owns its writer, leases and durable output. Under
    // concurrent fsync load this response can precede output finalization.
    assert!(receipt(&unknown).output.is_none());
    assert_eq!(unknown["artifacts"], json!([]));
    fixture.lose_acknowledgement(id);
    let argv = fixture.argv();

    assert!(fixture.retry(call_id, &arguments).await.is_err());
    assert_eq!(
        fixture.broker.store.action(id).unwrap().unwrap().state,
        "outcome_unknown"
    );
    assert!(fixture.events.try_recv().is_err());
    fixture.assert_single_execution(&argv);

    drop(publication);
    let saved = fixture
        .retained_receipt(serde_json::from_value(arguments["operation_id"].clone()).unwrap())
        .await;
    assert_eq!(saved.state, EnvironmentOperationState::OutcomeUnknown);
    let output = saved.output.as_ref().unwrap();
    assert_eq!(output.stdout, "captured prefix");
    assert_eq!(output.stderr, "captured diagnostic");
    fixture.assert_retained_receipt(&saved, false);
    assert!(fixture.retry(call_id, &arguments).await.is_err());
    assert_eq!(
        fixture.broker.store.action(id).unwrap().unwrap().state,
        "outcome_unknown"
    );
    assert!(fixture.events.try_recv().is_err());
    fixture.assert_single_execution(&argv);

    let resolved = management(
        &fixture.config,
        "helper_environment_resolve",
        json!({"operation_id":arguments["operation_id"],"succeeded":false,"note":"Fixture execution marker inspected; command was interrupted and no successful completion was observed."}),
    )
    .await;
    assert_eq!(resolved.state, EnvironmentOperationState::Failed);
    let error = fixture.retry(call_id, &arguments).await.unwrap_err();
    let observed = &error.downcast_ref::<ObservedFailure>().unwrap().0;
    fixture.assert_retained_artifacts(observed, false);
    assert_eq!(receipt(observed).state, EnvironmentOperationState::Failed);
    assert_eq!(
        serde_json::to_value(receipt(observed)).unwrap(),
        serde_json::to_value(resolved).unwrap()
    );
    let action = fixture.broker.store.action(id).unwrap().unwrap();
    assert_eq!(action.state, "failed");
    assert_eq!(action.result.as_ref(), Some(observed));
    fixture.assert_single_execution(&argv);
    fixture.assert_recovery_event(id, observed, false);
    fixture.broker.shutdown().await.unwrap();
}

/// One bounded loopback response qualifies the real engine entry point without
/// accepting paid calls or replacing the engine with a mock implementation.
pub(super) async fn provider_fixture() -> (String, tokio::task::JoinHandle<Value>) {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            let mut length = 0;
            let mut headers_bytes = 0;
            loop {
                let mut line = String::new();
                assert!(socket.read_line(&mut line).await.unwrap() > 0);
                headers_bytes += line.len();
                assert!(headers_bytes <= 32 * 1024);
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse::<usize>().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
            }
            assert!((1..=1024 * 1024).contains(&length));
            let mut body = vec![0; length];
            socket.read_exact(&mut body).await.unwrap();
            let response = format!(
                "data: {}\n\ndata: [DONE]\n\n",
                json!({"id":"resume-fixture","object":"chat.completion.chunk","model":"fixture-model","choices":[{"index":0,"delta":{"role":"assistant","content":"Resumed from the saved command result."},"finish_reason":"stop"}]})
            );
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.len()
            );
            socket.get_mut().write_all(header.as_bytes()).await.unwrap();
            socket.get_mut().write_all(response.as_bytes()).await.unwrap();
            serde_json::from_slice(&body).unwrap()
        })
        .await
        .expect("Engine must reach the bounded provider fixture after receipt recovery")
    });
    (endpoint, task)
}

fn resumable_checkpoint(fixture: &Fixture, arguments: &Value, call_id: &str) {
    use crate::model::{Message, ToolCall};
    let messages = vec![
        Message::user("Execute the explicitly approved environment command."),
        Message::assistant_with_tool_calls(
            "",
            vec![ToolCall::new(
                call_id,
                "environment_execute",
                arguments.to_string(),
            )],
        ),
    ];
    fixture
        .broker
        .store
        .save_checkpoint(
            fixture.broker.request.session_id,
            fixture.broker.request.turn_id,
            &serde_json::to_value(messages).unwrap(),
        )
        .unwrap();
}

async fn engine_resumes_terminal_receipt(command: &str, expected_state: &str) {
    let fixture = Fixture::new().await;
    let call_id = "engine-resume-old-turn";
    let arguments = fixture.arguments(command);
    let id = fixture.running_outer(call_id, &arguments);
    let observed = serde_json::to_value(
        fixture
            .service
            .execute("environment_execute", &arguments, CancellationToken::new())
            .await
            .unwrap(),
    )
    .unwrap();
    fixture.lose_acknowledgement(id);
    resumable_checkpoint(&fixture, &arguments, call_id);
    let original = fixture.broker.store.action(id).unwrap().unwrap().intent;
    let argv = fixture.argv();
    let (endpoint, provider) = provider_fixture().await;
    let mut request = fixture.broker.request.clone();
    // Receipt recovery must keep the old intent's turn, even when the user sends
    // a new turn to resume the chat after the worker acknowledgement was lost.
    request.turn_id = Uuid::new_v4();
    request.provider = ProviderConfig {
        endpoint,
        model: "fixture-model".into(),
        context_tokens: 65536,
        output_tokens: 256,
        timeout_seconds: 5,
        ..Default::default()
    };
    let paths = crate::AppPaths::at(fixture.root.clone()).unwrap();
    let engine = crate::AgentEngine::new(
        fixture.broker.store.clone(),
        paths,
        ToolSession::for_tests(PermissionMode::AskForApproval),
    );
    let (events, mut received) = tokio::sync::mpsc::channel(256);
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        engine.run(request.clone(), events, CancellationToken::new()),
    )
    .await
    .unwrap();
    assert!(outcome.is_ok(), "Actual engine resume failed: {outcome:?}");
    let sent = provider.await.unwrap();
    let tool_result = sent["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("The real provider request must contain the recovered old-turn result");
    assert_eq!(tool_result["tool_call_id"], call_id);
    assert_eq!(
        serde_json::from_str::<Value>(tool_result["content"].as_str().unwrap()).unwrap(),
        observed
    );
    let action = fixture.broker.store.action(id).unwrap().unwrap();
    assert_eq!(action.state, expected_state);
    assert_eq!(action.intent.id, original.id);
    assert_eq!(action.intent.turn_id, original.turn_id);
    assert_eq!(action.intent.idempotency_key, original.idempotency_key);
    assert_eq!(action.result.as_ref(), Some(&observed));
    assert!(
        fixture
            .broker
            .store
            .action_for_key(
                request.session_id,
                request.turn_id,
                &original.idempotency_key
            )
            .unwrap()
            .is_none(),
        "Recovery must not create a replacement action in the new turn"
    );
    let mut completed = false;
    while let Ok(event) = received.try_recv() {
        assert!(
            !matches!(
                event,
                EngineEvent::ApprovalRequired { .. } | EngineEvent::ToolStarted { .. }
            ),
            "Resume cannot approve or dispatch the recovered command again"
        );
        completed |= matches!(event, EngineEvent::Completed { .. });
    }
    assert!(completed);
    fixture.assert_single_execution(&argv);
    fixture.broker.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_entry_recovers_success_before_unresolved_gate_using_original_turn() {
    engine_resumes_terminal_receipt("__success__", "succeeded").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_entry_recovers_failure_before_unresolved_gate_using_original_turn() {
    engine_resumes_terminal_receipt("__fail__", "failed").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_entry_keeps_absent_or_unrelated_unknown_actions_blocked_without_dispatch() {
    use crate::model::{Message, ToolCall};
    for unrelated in [false, true] {
        let fixture = Fixture::new().await;
        let arguments = fixture.arguments("__success__");
        let id = fixture.running_outer("still-unknown", &arguments);
        if unrelated {
            // A terminal native receipt may be recovered, but an unrelated write
            // still forbids both model requests and absent checkpoint dispatch.
            fixture
                .service
                .execute("environment_execute", &arguments, CancellationToken::new())
                .await
                .unwrap();
            let other = ActionIntent {
                id: Uuid::new_v4(),
                session_id: fixture.broker.request.session_id,
                turn_id: fixture.broker.request.turn_id,
                name: "run_command".into(),
                arguments: json!({"program":"unrelated","args":[]}),
                writes: true,
                idempotency_key: ToolBroker::journal_key(None, "unrelated-unknown"),
            };
            fixture.broker.store.begin_action(&other).unwrap();
            fixture.broker.store.mark_action_running(other.id).unwrap();
            fixture.lose_acknowledgement(other.id);
        }
        fixture.lose_acknowledgement(id);
        let checkpoint = vec![Message::assistant_with_tool_calls(
            "",
            vec![ToolCall::new(
                "never-dispatched",
                "file_write",
                json!({"path":"must-not-write.txt","content":"not authorized"}).to_string(),
            )],
        )];
        fixture
            .broker
            .store
            .save_checkpoint(
                fixture.broker.request.session_id,
                fixture.broker.request.turn_id,
                &serde_json::to_value(checkpoint).unwrap(),
            )
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut request = fixture.broker.request.clone();
        request.provider.endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        request.provider.model = "fixture-model".into();
        let argv = fixture.argv();
        let engine = crate::AgentEngine::new(
            fixture.broker.store.clone(),
            crate::AppPaths::at(fixture.root.clone()).unwrap(),
            ToolSession::for_tests(PermissionMode::AskForApproval),
        );
        let (events, mut received) = tokio::sync::mpsc::channel(256);
        engine
            .run(request, events, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(fixture.argv(), argv);
        assert!(!fixture.root.join("must-not-write.txt").exists());
        assert!(
            tokio::time::timeout(Duration::from_millis(30), listener.accept())
                .await
                .is_err(),
            "An unresolved action must block provider dispatch"
        );
        assert_eq!(
            fixture.broker.store.action(id).unwrap().unwrap().state,
            if unrelated {
                "succeeded"
            } else {
                "outcome_unknown"
            }
        );
        let mut blocked = false;
        while let Ok(event) = received.try_recv() {
            match event {
                EngineEvent::Error { message, .. } => {
                    blocked |= message.contains("outcome-unknown")
                }
                EngineEvent::ApprovalRequired { .. } | EngineEvent::ToolStarted { .. } => {
                    panic!("An unresolved action must block dispatch: {event:?}")
                }
                _ => {}
            }
        }
        assert!(
            blocked,
            "The engine must end with the unresolved-action error"
        );
        fixture.broker.shutdown().await.unwrap();
    }
}

async fn assert_engine_can_continue(fixture: &Fixture) {
    let (endpoint, provider) = provider_fixture().await;
    let mut request = fixture.broker.request.clone();
    request.turn_id = Uuid::new_v4();
    request.provider = ProviderConfig {
        endpoint,
        model: "fixture-model".into(),
        context_tokens: 65536,
        output_tokens: 256,
        timeout_seconds: 5,
        ..Default::default()
    };
    let engine = crate::AgentEngine::new(
        fixture.broker.store.clone(),
        crate::AppPaths::at(fixture.root.clone()).unwrap(),
        ToolSession::for_tests(PermissionMode::AskForApproval),
    );
    let (events, mut received) = tokio::sync::mpsc::channel(256);
    tokio::time::timeout(
        Duration::from_secs(10),
        engine.run(request.clone(), events, CancellationToken::new()),
    )
    .await
    .unwrap()
    .unwrap();
    provider.await.unwrap();
    assert!(
        fixture
            .broker
            .store
            .completed_turn(request.session_id, request.turn_id)
            .unwrap()
            .is_some(),
        "A locally rejected command must not permanently block the actual engine"
    );
    while let Ok(event) = received.try_recv() {
        assert!(!matches!(
            event,
            EngineEvent::ApprovalRequired { .. } | EngineEvent::ToolStarted { .. }
        ));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broker_preflight_rejections_are_failed_and_allow_actual_engine_to_continue() {
    use xylo_capabilities::environments::CommandNotDispatched;
    for case in ["revision", "disconnected", "command", "budget"] {
        let mut fixture = Fixture::new().await;
        let mut arguments = fixture.arguments("__success__");
        match case {
            "revision" => arguments["expected_revision"] = json!(fixture.environment.revision + 1),
            "disconnected" => {
                let disconnected = management(
                    &fixture.config,
                    "helper_environment_disconnect",
                    json!({"operation_id":Uuid::new_v4(),"environment_id":fixture.environment.profile.id,"expected_revision":fixture.environment.revision}),
                )
                .await;
                let record = disconnected.environment.unwrap();
                assert!(!record.connected);
                arguments["expected_revision"] = json!(record.revision);
            }
            "command" => arguments["command"] = json!([]),
            "budget" => fixture.set_output_budget(1024),
            _ => unreachable!(),
        }
        let argv = fixture.argv();
        let error = fixture
            .approve_execution(case, &arguments)
            .await
            .unwrap_err();
        assert!(
            error.downcast_ref::<CommandNotDispatched>().is_some(),
            "{case} must be a proven local validation rejection: {error:#}"
        );
        let action = fixture.broker.journaled(case).unwrap().unwrap();
        assert_eq!(action.state, "failed", "{case} must not become unknown");
        assert!(
            fixture
                .broker
                .store
                .unresolved_actions(action.intent.session_id)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            fixture.argv(),
            argv,
            "Pure rejection must not even probe the client"
        );
        assert!(!fixture.root.join("executions").exists());
        assert!(fixture.retry(case, &arguments).await.is_err());
        assert_eq!(
            fixture.argv(),
            argv,
            "A failed call ID must not be replayed"
        );
        assert_engine_can_continue(&fixture).await;
        assert_eq!(fixture.argv(), argv);
        assert!(!fixture.root.join("executions").exists());
        fixture.broker.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_fresh_environment_requests_have_no_journal_approval_or_dispatch() {
    let mut fixture = Fixture::new().await;
    let argv = fixture.argv();
    for case in ["malformed-uuid", "missing-command"] {
        let mut arguments = fixture.arguments("__success__");
        if case == "malformed-uuid" {
            arguments["operation_id"] = json!("not-a-uuid");
        } else {
            arguments.as_object_mut().unwrap().remove("command");
        }
        let error = fixture.retry(case, &arguments).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Invalid environment command request")
        );
        assert!(fixture.broker.journaled(case).unwrap().is_none());
        assert!(fixture.events.try_recv().is_err());
        assert_eq!(fixture.argv(), argv);
        assert!(!fixture.root.join("executions").exists());
    }
    assert_engine_can_continue(&fixture).await;
    assert_eq!(fixture.argv(), argv);
    fixture.broker.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn existing_uncertain_receipt_is_not_classified_as_a_fresh_preflight_rejection() {
    use xylo_capabilities::environments::CommandNotDispatched;
    let mut fixture = Fixture::new().await;
    let arguments = fixture.arguments("__uncertain__");
    let unknown = fixture
        .service
        .execute("environment_execute", &arguments, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        receipt(&serde_json::to_value(unknown).unwrap()).state,
        EnvironmentOperationState::OutcomeUnknown
    );
    let argv = fixture.argv();
    let error = fixture
        .approve_execution("existing-unknown", &arguments)
        .await
        .unwrap_err();
    assert!(error.downcast_ref::<CommandNotDispatched>().is_none());
    let action = fixture
        .broker
        .journaled("existing-unknown")
        .unwrap()
        .unwrap();
    assert_eq!(action.state, "outcome_unknown");
    assert!(fixture.retry("existing-unknown", &arguments).await.is_err());
    fixture.assert_single_execution(&argv);
    fixture.broker.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_original_operation_lease_cancellation_remains_unknown_without_reexecution() {
    use xylo_capabilities::environments::CommandNotDispatched;
    let mut fixture = Fixture::new().await;
    let arguments = fixture.arguments("__hang__");
    let service = CapabilityService::new(fixture.config.clone()).unwrap();
    let native_cancel = CancellationToken::new();
    let native_token = native_cancel.clone();
    let native_arguments = arguments.clone();
    let native = tokio::spawn(async move {
        service
            .execute("environment_execute", &native_arguments, native_token)
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while !fixture.root.join("dispatched").exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let argv = fixture.argv();
    let broker = fixture.broker.clone();
    let cancel_waiter = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if broker
                    .journaled("existing-pending")
                    .unwrap()
                    .is_some_and(|action| action.state == "running")
                {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    broker.cancel.cancel();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    });
    let error = fixture
        .approve_execution("existing-pending", &arguments)
        .await
        .unwrap_err();
    cancel_waiter.await.unwrap();
    assert!(error.downcast_ref::<CommandNotDispatched>().is_none());
    assert_eq!(
        fixture
            .broker
            .journaled("existing-pending")
            .unwrap()
            .unwrap()
            .state,
        "outcome_unknown"
    );
    fixture.assert_single_execution(&argv);
    native_cancel.cancel();
    let interrupted = native.await.unwrap().unwrap();
    assert_eq!(
        receipt(&serde_json::to_value(interrupted).unwrap()).state,
        EnvironmentOperationState::OutcomeUnknown
    );
    fixture.assert_single_execution(&argv);
    fixture.broker.shutdown().await.unwrap();
}
