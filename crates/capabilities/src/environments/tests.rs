use super::*;
#[cfg(unix)]
use std::fs;

#[cfg(unix)]
struct Fixture {
    root: tempfile::TempDir,
    config: CapabilityConfig,
    client: PathBuf,
}
#[cfg(unix)]
fn executable(path: &Path, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, script).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
#[cfg(unix)]
fn fixture(ssh: bool) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let base = root.path().canonicalize().unwrap();
    let client = base.join(if ssh { "fake-ssh" } else { "fake-container" });
    let quoted = native::quote(base.to_str().unwrap()).unwrap();
    let script = if ssh {
        r#"#!/bin/sh
set -eu
root=ROOT
printf '<%s>\n' "$@" >> "$root/argv"
socket=''; control=''; master=no; previous=''
for arg in "$@"; do
  case "$previous" in -S) socket=$arg;; -O) control=$arg;; esac
  [ "$arg" != '-M' ] || master=yes
  previous=$arg
done
if [ "$master" = yes ]; then
  printf '%s' "$$" > "$socket"
  trap 'rm -f "$socket"; exit 0' TERM INT EXIT
  while :; do sleep 1; done
fi
if [ "$control" = check ]; then [ -f "$socket" ] && kill -0 "$(cat "$socket")" 2>/dev/null; exit $?; fi
if [ "$control" = exit ]; then
  if [ -f "$socket" ]; then kill -TERM "$(cat "$socket")"; rm -f "$socket"; fi
  exit 0
fi
[ -f "$socket" ] || exit 255
printf 'remote command completed'
"#
    } else {
        r#"#!/bin/sh
set -eu
root=ROOT
printf '<%s>\n' "$@" >> "$root/argv"
[ "$1" = container ] || exit 99
verb=$2; shift 2
case "$verb" in
 inspect)
  if [ -f "$root/unavailable" ]; then printf 'daemon unavailable' >&2; exit 1; fi
  if [ ! -f "$root/exists" ]; then printf 'Error: No such container' >&2; exit 1; fi
  running=false; [ ! -f "$root/running" ] || running=true
  labels='{}'; [ ! -f "$root/labels" ] || labels=$(cat "$root/labels")
  printf '{"Id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","Config":{"Labels":%s},"State":{"Running":%s}}' "$labels" "$running"
  ;;
 create)
  owner=''; profile=''; operation=''; previous=''
  for arg in "$@"; do
    if [ "$previous" = '--label' ]; then
      case "$arg" in com.qurkos.xylo.installation=*) owner=${arg#*=};; com.qurkos.xylo.environment=*) profile=${arg#*=};; com.qurkos.xylo.create-operation=*) operation=${arg#*=};; esac
    fi
    previous=$arg
  done
  printf '{"com.qurkos.xylo.installation":"%s","com.qurkos.xylo.environment":"%s","com.qurkos.xylo.create-operation":"%s"}' "$owner" "$profile" "$operation" > "$root/labels"
  touch "$root/exists"
  printf aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  ;;
 start) touch "$root/running";;
 stop) rm -f "$root/running";;
 rm) rm -f "$root/exists" "$root/labels";;
 exec)
  printf 'executed\n' >> "$root/executions"
  case "$*" in *'__hang__'*) printf 'stdout before interruption'; printf 'stderr before interruption' >&2; touch "$root/dispatched"; sleep 20;; *'__fail__'*) printf 'command failed' >&2; exit 7;; *'__large__'*) i=0; while [ "$i" -lt 3000 ]; do printf 'line-with-output-0123456789\n'; i=$((i+1)); done;; *) printf 'native argv executed';; esac
  ;;
 *) exit 98;;
esac
"#
    };
    executable(&client, &script.replace("ROOT", &quoted));
    Fixture {
        root,
        config: CapabilityConfig {
            workspace: base.clone(),
            artifacts: base.join("artifacts"),
            management_root: Some(base.join("management")),
            ..Default::default()
        },
        client,
    }
}
#[cfg(unix)]
fn profile(fixture: &Fixture, managed: bool) -> EnvironmentProfile {
    EnvironmentProfile {
        id: Uuid::new_v4(),
        name: "Test environment".into(),
        target: EnvironmentTarget::Container {
            executable: fixture.client.clone(),
            runtime: ContainerRuntime::Docker,
            container: if managed {
                ContainerTarget::Managed {
                    image: "local:test".into(),
                    command: vec!["sleep".into(), "infinity".into()],
                }
            } else {
                ContainerTarget::Existing {
                    target: "explicit-existing".into(),
                }
            },
        },
    }
}
#[cfg(unix)]
async fn management(config: &CapabilityConfig, name: &str, args: Value) -> Value {
    execute_management(config, name, &args, CancellationToken::new())
        .await
        .unwrap()
}
#[cfg(unix)]
async fn save_profile(fixture: &Fixture, profile: &EnvironmentProfile) -> EnvironmentRecord {
    let receipt: EnvironmentReceipt = serde_json::from_value(
        management(
            &fixture.config,
            "helper_environment_save",
            json!({"operation_id":Uuid::new_v4(),"expected_revision":0,"profile":profile}),
        )
        .await,
    )
    .unwrap();
    assert_eq!(receipt.state, EnvironmentOperationState::Succeeded);
    receipt.environment.unwrap()
}
#[cfg(unix)]
async fn action_request(
    fixture: &Fixture,
    record: &EnvironmentRecord,
    name: &str,
) -> EnvironmentReceipt {
    serde_json::from_value(management(&fixture.config,name,json!({"environment_id":record.profile.id,"operation_id":Uuid::new_v4(),"expected_revision":record.revision})).await).unwrap()
}
#[cfg(unix)]
#[tokio::test]
async fn profiles_are_durable_cas_bound_and_management_is_not_an_agent_tool() {
    let fixture = fixture(false);
    let profile = profile(&fixture, false);
    let record = save_profile(&fixture, &profile).await;
    assert_eq!(record.revision, 1);
    assert!(
        !fixture.root.path().join("argv").exists(),
        "saving must never launch a native client"
    );
    let list = management(&fixture.config, "helper_environment_list", json!({})).await;
    assert_eq!(list["environments"][0]["profile"]["id"], json!(profile.id));
    let stale = execute_management(
        &fixture.config,
        "helper_environment_save",
        &json!({"operation_id":Uuid::new_v4(),"expected_revision":0,"profile":profile}),
        CancellationToken::new(),
    )
    .await;
    assert!(stale.unwrap_err().to_string().contains("changed"));
    assert!(
        definitions()
            .iter()
            .all(|tool| !tool.name.starts_with("helper_"))
    );
    assert_eq!(definitions().len(), 3);
    assert!(
        definitions()
            .iter()
            .find(|tool| tool.name == "environment_execute")
            .unwrap()
            .writes
    );
    assert!(
        execute_tool(
            &fixture.config,
            "helper_environment_remove",
            &json!({}),
            CancellationToken::new()
        )
        .await
        .is_err()
    );
    let absent = execute_management(
        &fixture.config,
        "helper_environment_operation",
        &json!({"operation_id":Uuid::new_v4()}),
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(absent.to_string().contains("absence does not authorize"));
}
#[cfg(unix)]
#[tokio::test]
async fn managed_lifecycle_checks_labels_and_never_forces_or_removes_unrelated_resources() {
    let fixture = fixture(false);
    let mut record = save_profile(&fixture, &profile(&fixture, true)).await;
    for action in [
        "helper_environment_create",
        "helper_environment_start",
        "helper_environment_connect",
    ] {
        let receipt = action_request(&fixture, &record, action).await;
        assert_eq!(
            receipt.state,
            EnvironmentOperationState::Succeeded,
            "{}",
            receipt.message
        );
        record = receipt.environment.unwrap();
    }
    assert!(record.connected);
    let removal = action_request(&fixture, &record, "helper_environment_remove").await;
    assert_eq!(removal.state, EnvironmentOperationState::Failed);
    fs::write(fixture.root.path().join("labels"), "{}").unwrap();
    let refused = action_request(&fixture, &record, "helper_environment_stop").await;
    assert_eq!(refused.state, EnvironmentOperationState::Failed);
    let argv = fs::read_to_string(fixture.root.path().join("argv")).unwrap();
    assert!(!argv.contains("<stop>"));
    assert!(
        argv.contains("<--pull=never>") && !argv.contains("<--force>") && !argv.contains("<-v>")
    );
    let manager = Manager::open(&fixture.config, &CancellationToken::new())
        .await
        .unwrap();
    let installation = manager
        .installation(&CancellationToken::new())
        .await
        .unwrap();
    fs::write(fixture.root.path().join("labels"),json!({native::OWNER:installation.to_string(),native::PROFILE:record.profile.id.to_string(),native::CREATED:Uuid::new_v4().to_string()}).to_string()).unwrap();
    for action in [
        "helper_environment_stop",
        "helper_environment_remove_container",
        "helper_environment_remove",
    ] {
        let receipt = action_request(&fixture, &record, action).await;
        assert_eq!(
            receipt.state,
            EnvironmentOperationState::Succeeded,
            "{}",
            receipt.message
        );
        if let Some(next) = receipt.environment {
            record = next;
        }
    }
    assert!(!fixture.root.path().join("exists").exists());
    assert!(
        management(&fixture.config, "helper_environment_list", json!({})).await["environments"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
#[cfg(unix)]
async fn connected_existing(fixture: &Fixture) -> EnvironmentRecord {
    fs::write(fixture.root.path().join("exists"), "").unwrap();
    fs::write(fixture.root.path().join("running"), "").unwrap();
    let record = save_profile(fixture, &profile(fixture, false)).await;
    action_request(fixture, &record, "helper_environment_connect")
        .await
        .environment
        .unwrap()
}
#[cfg(unix)]
#[tokio::test]
async fn execute_is_revision_pinned_bounded_and_success_is_reused_without_a_second_process() {
    let fixture = fixture(false);
    let record = connected_existing(&fixture).await;
    let args = json!({"environment_id":record.profile.id,"operation_id":Uuid::new_v4(),"expected_revision":record.revision,"command":["printf","a'$(touch should-not-exist)"]});
    for _ in 0..2 {
        let observation = execute_tool(
            &fixture.config,
            "environment_execute",
            &args,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let receipt: EnvironmentReceipt = serde_json::from_str(&observation.summary).unwrap();
        assert_eq!(receipt.state, EnvironmentOperationState::Succeeded);
    }
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("executions"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert!(!fixture.root.path().join("should-not-exist").exists());
    for extra in [
        json!({"executable":"/bin/sh"}),
        json!({"target":"unrelated"}),
    ] {
        let mut request = args.clone();
        request
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(
            execute_tool(
                &fixture.config,
                "environment_execute",
                &request,
                CancellationToken::new()
            )
            .await
            .is_err()
        );
    }
    let mut stale = args.clone();
    stale["operation_id"] = json!(Uuid::new_v4());
    stale["expected_revision"] = json!(record.revision - 1);
    assert!(
        execute_tool(
            &fixture.config,
            "environment_execute",
            &stale,
            CancellationToken::new()
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("changed after command approval")
    );
    let mut large = args;
    large["operation_id"] = json!(Uuid::new_v4());
    large["command"] = json!(["__large__"]);
    let result = execute_tool(
        &fixture.config,
        "environment_execute",
        &large,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let receipt: EnvironmentReceipt = serde_json::from_str(&result.summary).unwrap();
    let output = receipt.output.unwrap();
    assert!(output.truncated);
    assert_eq!(result.artifacts.len(), 2);
    for artifact in &output.artifacts {
        assert!(artifact.complete);
        assert_eq!(artifact.reason, EnvironmentCaptureReason::Eof);
        let bytes = fs::read(&artifact.path).unwrap();
        assert_eq!(artifact.stored_bytes, bytes.len() as u64);
        assert_eq!(artifact.sha256, format!("{:x}", Sha256::digest(&bytes)));
    }
    assert!(output.artifacts[0].stored_bytes > output.stdout.len() as u64);
    assert!(result.summary.len() < fixture.config.max_output_bytes);
    let refused = action_request(&fixture, &record, "helper_environment_stop").await;
    assert_eq!(
        refused.state,
        EnvironmentOperationState::Failed,
        "existing containers are execution-only"
    );
}
#[cfg(unix)]
#[tokio::test]
async fn cancellation_preserves_unknown_after_restart_and_blocks_automatic_replay() {
    let fixture = fixture(false);
    let record = connected_existing(&fixture).await;
    let operation = Uuid::new_v4();
    let request = json!({"environment_id":record.profile.id,"operation_id":operation,"expected_revision":record.revision,"command":["__hang__"]});
    let cancel = CancellationToken::new();
    let call = execute_tool(
        &fixture.config,
        "environment_execute",
        &request,
        cancel.clone(),
    );
    let stop = async {
        for _ in 0..200 {
            if fixture.root.path().join("dispatched").exists() {
                cancel.cancel();
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("fake native execution never started")
    };
    let (result, _) = tokio::join!(call, stop);
    let receipt: EnvironmentReceipt = serde_json::from_str(&result.unwrap().summary).unwrap();
    assert_eq!(receipt.state, EnvironmentOperationState::OutcomeUnknown);
    // Fresh manager reads the same disk receipts, without any in-memory service.
    let listed = management(&fixture.config, "helper_environment_list", json!({})).await;
    assert!(
        listed["operations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["operation_id"] == json!(operation))
    );
    let retry = execute_tool(
        &fixture.config,
        "environment_execute",
        &request,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        serde_json::from_str::<EnvironmentReceipt>(&retry.summary)
            .unwrap()
            .state,
        EnvironmentOperationState::OutcomeUnknown
    );
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("executions"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let mut another = request;
    another["operation_id"] = json!(Uuid::new_v4());
    another["command"] = json!(["true"]);
    assert!(
        execute_tool(
            &fixture.config,
            "environment_execute",
            &another,
            CancellationToken::new()
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("uncertain outcome")
    );
    let resolved=management(&fixture.config,"helper_environment_resolve",json!({"operation_id":operation,"succeeded":false,"note":"Inspected fake target marker; command effects reviewed."})).await;
    assert_eq!(resolved["state"], "failed");
    execute_tool(
        &fixture.config,
        "environment_execute",
        &another,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("executions"))
            .unwrap()
            .lines()
            .count(),
        2
    );
}
#[cfg(unix)]
#[tokio::test]
async fn foreground_ssh_master_survives_connect_and_is_shared_but_only_owner_shutdown_stops_it() {
    let fixture = fixture(true);
    let known = fixture.root.path().join("known hosts");
    fs::write(&known, "example.invalid ssh-ed25519 verified-fixture-key\n").unwrap();
    let profile = EnvironmentProfile {
        id: Uuid::new_v4(),
        name: "SSH fixture".into(),
        target: EnvironmentTarget::Ssh {
            executable: fixture.client.clone(),
            host: "example.invalid".into(),
            user: "user".into(),
            port: 2222,
            known_hosts: known,
            identity_file: None,
        },
    };
    let record = save_profile(&fixture, &profile).await;
    let connected = action_request(&fixture, &record, "helper_environment_connect").await;
    assert_eq!(
        connected.state,
        EnvironmentOperationState::Succeeded,
        "{}",
        connected.message
    );
    let record = connected.environment.unwrap();
    assert_eq!(active_sessions(&fixture.config), 1);
    let manager = Manager::open(&fixture.config, &CancellationToken::new())
        .await
        .unwrap();
    let socket = native::socket(&manager, profile.id, &CancellationToken::new())
        .await
        .unwrap();
    assert!(socket.as_os_str().as_encoded_bytes().len() <= 100);
    let pid = fs::read_to_string(&socket).unwrap().parse::<i32>().unwrap();
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        0,
        "foreground native master must really survive the connect call"
    );
    let args = json!({"environment_id":profile.id,"operation_id":Uuid::new_v4(),"expected_revision":record.revision,"command":["printf","a'$(touch injected)"]});
    let output = execute_tool(
        &fixture.config,
        "environment_execute",
        &args,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        serde_json::from_str::<EnvironmentReceipt>(&output.summary)
            .unwrap()
            .state,
        EnvironmentOperationState::Succeeded
    );
    let log = fs::read_to_string(fixture.root.path().join("argv")).unwrap();
    assert!(
        log.contains("<-oProxyCommand=false>")
            && log.contains("<-oStrictHostKeyChecking=yes>")
            && log.contains("<-oBatchMode=yes>")
    );
    assert!(log.contains("exec 'printf' 'a'\\''$(touch injected)'"));
    let mut other = fixture.config.clone();
    other.management_root = Some(fixture.root.path().join("other-management"));
    shutdown(&other).await.unwrap();
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        0,
        "another service root must not stop this master"
    );
    // Storage corruption must not prevent process cleanup.
    fs::write(manager.root.join("profiles.json"), "broken").unwrap();
    shutdown(&fixture.config).await.unwrap();
    assert_eq!(active_sessions(&fixture.config), 0);
    assert_ne!(unsafe { libc::kill(pid, 0) }, 0);
    let _ = fs::remove_file(socket);
}
#[cfg(unix)]
#[tokio::test]
async fn live_profile_lease_prevents_stale_recovery_and_cancellation_while_waiting_has_no_effect() {
    let fixture = fixture(false);
    let record = connected_existing(&fixture).await;
    let manager = Manager::open(&fixture.config, &CancellationToken::new())
        .await
        .unwrap();
    let guard = manager
        .profile_lease(record.profile.id, &CancellationToken::new())
        .await
        .unwrap();
    let operation = Operation {
        created_at: now_ms(),
        digest: "fixture".into(),
        expected_revision: record.revision,
        profile: record.profile.clone(),
        previous_container_id: record.container_id.clone(),
        receipt: EnvironmentReceipt {
            operation_id: Uuid::new_v4(),
            environment_id: record.profile.id,
            action: EnvironmentAction::Execute,
            state: EnvironmentOperationState::Running,
            environment: Some(record.clone()),
            output: None,
            message: "Running".into(),
            created_at_ms: None,
            command_label: None,
        },
    };
    manager
        .write_operation(&operation, &CancellationToken::new())
        .await
        .unwrap();
    let status = management(
        &fixture.config,
        "helper_environment_operation",
        json!({"operation_id":operation.receipt.operation_id}),
    )
    .await;
    assert_eq!(status["state"], "running");
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(
        manager
            .profile_lease(record.profile.id, &cancel)
            .await
            .is_err()
    );
    drop(guard);
    let recovered = management(
        &fixture.config,
        "helper_environment_operation",
        json!({"operation_id":operation.receipt.operation_id}),
    )
    .await;
    assert_eq!(recovered["state"], "outcome_unknown");
}
#[tokio::test]
async fn unused_shutdown_does_not_initialize_storage_and_missing_management_is_explicit() {
    let root = tempfile::tempdir().unwrap();
    let config = CapabilityConfig {
        management_root: Some(root.path().join("unused")),
        ..Default::default()
    };
    shutdown(&config).await.unwrap();
    assert!(!root.path().join("unused").exists());
    assert!(
        execute_management(
            &CapabilityConfig::default(),
            "helper_environment_list",
            &json!({}),
            CancellationToken::new()
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("not configured")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn terminal_archival_preserves_exact_results_beyond_the_active_receipt_limit() {
    let fixture = fixture(false);
    let record = connected_existing(&fixture).await;
    let operation_id = Uuid::new_v4();
    let args = json!({"environment_id":record.profile.id,"operation_id":operation_id,"expected_revision":record.revision,"command":["true"]});
    let original = execute_tool(
        &fixture.config,
        "environment_execute",
        &args,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let manager = Manager::open(&fixture.config, &CancellationToken::new())
        .await
        .unwrap();
    let template = manager.operation(operation_id).unwrap().unwrap();
    let archival_started = std::time::Instant::now();
    for index in 0..1030u128 {
        let mut operation = template.clone();
        operation.created_at = template.created_at + index as i64 + 1;
        operation.receipt.operation_id = Uuid::from_u128(index + 1);
        // Synthetic archival fillers are legacy preview-only receipts, not
        // aliases of another operation's immutable output artifact identity.
        operation.receipt.output.as_mut().unwrap().artifacts.clear();
        manager
            .write_operation(&operation, &CancellationToken::new())
            .await
            .unwrap();
    }
    eprintln!(
        "environment archival workload:1030 additional terminal operations in {:.3}s",
        archival_started.elapsed().as_secs_f64()
    );
    let mut uncertain = template.clone();
    uncertain.receipt.operation_id = Uuid::new_v4();
    uncertain.receipt.output.as_mut().unwrap().artifacts.clear();
    uncertain.receipt.state = EnvironmentOperationState::OutcomeUnknown;
    manager
        .write_operation(&uncertain, &CancellationToken::new())
        .await
        .unwrap();
    let result = manager
        .maintain(1024, &CancellationToken::new())
        .await
        .unwrap();
    assert!(result["archived"].as_u64().unwrap() > 0);
    assert_eq!(
        manager.all_operations().unwrap().len(),
        1,
        "uncertain write must remain in the active journal"
    );
    let reused = execute_tool(
        &fixture.config,
        "environment_execute",
        &args,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        reused.summary, original.summary,
        "archiving must preserve the original complete successful result"
    );
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("executions"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let recent = management(
        &fixture.config,
        "helper_environment_operations",
        json!({"environment_id":record.profile.id}),
    )
    .await;
    assert!(recent["operations"].as_array().unwrap().len() <= 129);
    assert_eq!(
        manager
            .operation(uncertain.receipt.operation_id)
            .unwrap()
            .unwrap()
            .receipt
            .state,
        EnvironmentOperationState::OutcomeUnknown
    );
    let shard = manager
        .root
        .join("history")
        .join(&operation_id.simple().to_string()[..2]);
    fs::write(
        shard.join(format!("{operation_id}.json.gz")),
        b"tampered archive",
    )
    .unwrap();
    assert!(
        manager
            .operation(operation_id)
            .err()
            .expect("tampered archive must fail")
            .to_string()
            .contains("hash mismatch")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn lost_create_ack_is_reconciled_by_owned_labels_without_repeating_create() {
    let fixture = fixture(false);
    let original = save_profile(&fixture, &profile(&fixture, true)).await;
    let receipt = action_request(&fixture, &original, "helper_environment_create").await;
    assert_eq!(receipt.state, EnvironmentOperationState::Succeeded);
    let manager = Manager::open(&fixture.config, &CancellationToken::new())
        .await
        .unwrap();
    let mut operation = manager.operation(receipt.operation_id).unwrap().unwrap();
    operation.receipt.state = EnvironmentOperationState::Running;
    // Model a crash after the server created the labelled resource but before
    // the application committed its profile and terminal receipt.
    manager
        .commit(
            original.clone(),
            false,
            Uuid::new_v4(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    manager
        .write_operation(&operation, &CancellationToken::new())
        .await
        .unwrap();
    let recovered = management(
        &fixture.config,
        "helper_environment_operation",
        json!({"operation_id":receipt.operation_id}),
    )
    .await;
    assert_eq!(recovered["state"], "succeeded");
    assert_eq!(
        recovered["environment"]["container_id"],
        json!("a".repeat(64))
    );
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("argv"))
            .unwrap()
            .lines()
            .filter(|line| *line == "<create>")
            .count(),
        1
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_replaced_client_symlink_cannot_redirect_a_saved_profile() {
    use std::os::unix::fs::symlink;
    let fixture = fixture(false);
    let record = connected_existing(&fixture).await;
    let replacement = fixture.root.path().join("replacement");
    executable(&replacement, "#!/bin/sh\nexit 99\n");
    fs::remove_file(&fixture.client).unwrap();
    symlink(&replacement, &fixture.client).unwrap();
    let error=execute_tool(&fixture.config,"environment_execute",&json!({"environment_id":record.profile.id,"operation_id":Uuid::new_v4(),"expected_revision":record.revision,"command":["true"]}),CancellationToken::new()).await.unwrap_err();
    assert!(error.to_string().contains("client path changed"));
    assert!(!fixture.root.path().join("executions").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn missing_receipt_resolution_fences_a_real_delayed_admission_and_rejects_wrong_context() {
    let fixture = fixture(false);
    let record = save_profile(&fixture, &profile(&fixture, false)).await;
    let manager = Manager::open(&fixture.config, &CancellationToken::new())
        .await
        .unwrap();
    let profile_guard = manager
        .profile_lease(record.profile.id, &CancellationToken::new())
        .await
        .unwrap();
    let operation_id = Uuid::new_v4();
    let mut edited = record.profile.clone();
    edited.name = "Retained form".into();
    let request =
        json!({"operation_id":operation_id,"expected_revision":record.revision,"profile":edited});
    let cancel = CancellationToken::new();
    let saving = execute_management(
        &fixture.config,
        "helper_environment_save",
        &request,
        cancel.clone(),
    );
    let observe = async {
        let mut started = false;
        for _ in 0..200 {
            if manager.try_operation_lease(operation_id).unwrap().is_none() {
                started = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            started,
            "actual save must hold its operation lease before waiting for the profile"
        );
        assert!(manager.operation(operation_id).unwrap().is_none());
        let status = management(
            &fixture.config,
            "helper_environment_operation",
            json!({"operation_id":operation_id,"environment_id":record.profile.id,"action":"save"}),
        )
        .await;
        assert_eq!(
            status["state"], "prepared",
            "a live admitted request cannot be declared undispatched"
        );
        assert!(manager.cancelled(operation_id).unwrap().is_none());
        cancel.cancel();
    };
    let (result, _) = tokio::join!(saving, observe);
    assert!(result.is_err());
    let cancelled = management(
        &fixture.config,
        "helper_environment_operation",
        json!({"operation_id":operation_id,"environment_id":record.profile.id,"action":"save"}),
    )
    .await;
    assert_eq!(cancelled["state"], "cancelled");
    assert!(manager.cancelled(operation_id).unwrap().is_some());
    drop(profile_guard);
    let late = management(&fixture.config, "helper_environment_save", request).await;
    assert_eq!(
        late["state"], "cancelled",
        "a delayed original request must obey the durable fence"
    );
    assert_eq!(
        manager
            .record(record.profile.id, &CancellationToken::new())
            .await
            .unwrap()
            .profile
            .name,
        record.profile.name
    );
    for context in [
        json!({"environment_id":Uuid::new_v4(),"action":"save"}),
        json!({"environment_id":record.profile.id,"action":"connect"}),
    ] {
        let mut args = context;
        args["operation_id"] = json!(operation_id);
        assert!(
            execute_management(
                &fixture.config,
                "helper_environment_operation",
                &args,
                CancellationToken::new()
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("another environment or action")
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn pre_dispatch_cas_error_can_be_authoritatively_cancelled_without_losing_the_form() {
    let fixture = fixture(false);
    let record = save_profile(&fixture, &profile(&fixture, false)).await;
    let operation = Uuid::new_v4();
    let request = json!({"operation_id":operation,"expected_revision":0,"profile":record.profile});
    assert!(
        execute_management(
            &fixture.config,
            "helper_environment_save",
            &request,
            CancellationToken::new()
        )
        .await
        .is_err()
    );
    let cancelled = management(
        &fixture.config,
        "helper_environment_operation",
        json!({"operation_id":operation,"environment_id":record.profile.id,"action":"save"}),
    )
    .await;
    assert_eq!(cancelled["state"], "cancelled");
    let late = management(&fixture.config, "helper_environment_save", request).await;
    assert_eq!(late["state"], "cancelled");
    assert!(!fixture.root.path().join("argv").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn outer_journal_recovery_requires_an_exact_terminal_receipt_and_never_dispatches() {
    let fixture = fixture(false);
    let record = connected_existing(&fixture).await;
    let operation_id = Uuid::new_v4();
    let args = json!({"environment_id":record.profile.id,"operation_id":operation_id,"expected_revision":record.revision,"command":["__hang__"]});
    let cancel = CancellationToken::new();
    let command = execute_tool(
        &fixture.config,
        "environment_execute",
        &args,
        cancel.clone(),
    );
    let inspect = async {
        for _ in 0..200 {
            if fixture.root.path().join("dispatched").exists() {
                assert!(
                    recover_execution(&fixture.config, &args, CancellationToken::new())
                        .await
                        .unwrap()
                        .is_none(),
                    "a live native command cannot be recovered as complete"
                );
                cancel.cancel();
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("fixture command did not enter its active phase");
    };
    let (result, _) = tokio::join!(command, inspect);
    assert_eq!(
        serde_json::from_str::<EnvironmentReceipt>(&result.unwrap().summary)
            .unwrap()
            .state,
        EnvironmentOperationState::OutcomeUnknown
    );
    assert!(
        recover_execution(&fixture.config, &args, CancellationToken::new())
            .await
            .unwrap()
            .is_none()
    );
    let mut wrong = args.clone();
    wrong["command"] = json!(["different"]);
    assert!(
        recover_execution(&fixture.config, &wrong, CancellationToken::new())
            .await
            .unwrap_err()
            .to_string()
            .contains("does not match")
    );
    wrong = args.clone();
    wrong["environment_id"] = json!(Uuid::new_v4());
    assert!(
        recover_execution(&fixture.config, &wrong, CancellationToken::new())
            .await
            .is_err()
    );
    let absent = Uuid::new_v4();
    wrong = args.clone();
    wrong["operation_id"] = json!(absent);
    assert!(
        recover_execution(&fixture.config, &wrong, CancellationToken::new())
            .await
            .unwrap()
            .is_none()
    );
    let manager = Manager::existing(&fixture.config).unwrap().unwrap();
    assert!(
        !manager
            .root
            .join("operation-leases")
            .join(format!("{absent}.lock"))
            .exists()
    );
    assert!(manager.cancelled(absent).unwrap().is_none());
    management(&fixture.config,"helper_environment_resolve",json!({"operation_id":operation_id,"succeeded":true,"note":"Inspected the fixture's actual execution marker."})).await;
    let argv_before = fs::read(fixture.root.path().join("argv")).unwrap();
    let recovered = recover_execution(&fixture.config, &args, CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    assert_ne!(recovered.status, "failed");
    assert_eq!(
        serde_json::from_str::<EnvironmentReceipt>(&recovered.summary)
            .unwrap()
            .state,
        EnvironmentOperationState::Succeeded
    );
    assert_eq!(
        fs::read(fixture.root.path().join("argv")).unwrap(),
        argv_before,
        "recovery must not even launch a connection probe"
    );
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("executions"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let failed_id = Uuid::new_v4();
    let failed = json!({"environment_id":record.profile.id,"operation_id":failed_id,"expected_revision":record.revision,"command":["__fail__"]});
    execute_tool(
        &fixture.config,
        "environment_execute",
        &failed,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let failed_result = recover_execution(&fixture.config, &failed, CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failed_result.status, "failed");
    assert_eq!(
        serde_json::from_str::<EnvironmentReceipt>(&failed_result.summary)
            .unwrap()
            .state,
        EnvironmentOperationState::Failed
    );
    let mut cancelled_request: EnvironmentExecuteRequest =
        serde_json::from_value(failed.clone()).unwrap();
    cancelled_request.operation_id = Uuid::new_v4();
    let mut cancelled = manager.operation(failed_id).unwrap().unwrap();
    cancelled.digest = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(EnvironmentAction::Execute, &cancelled_request)).unwrap()
        )
    );
    cancelled.receipt.operation_id = cancelled_request.operation_id;
    cancelled.receipt.output.as_mut().unwrap().artifacts.clear();
    cancelled.receipt.state = EnvironmentOperationState::Cancelled;
    manager
        .write_operation(&cancelled, &CancellationToken::new())
        .await
        .unwrap();
    let known_cancelled = recover_execution(
        &fixture.config,
        &serde_json::to_value(cancelled_request).unwrap(),
        CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(known_cancelled.status, "failed");
    assert_eq!(
        serde_json::from_str::<EnvironmentReceipt>(&known_cancelled.summary)
            .unwrap()
            .state,
        EnvironmentOperationState::Cancelled
    );
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("executions"))
            .unwrap()
            .lines()
            .count(),
        2
    );
}

#[tokio::test]
async fn unused_execution_recovery_never_initializes_environment_storage() {
    let root = tempfile::tempdir().unwrap();
    let config = CapabilityConfig {
        management_root: Some(root.path().join("unused")),
        ..Default::default()
    };
    let args = json!({"environment_id":Uuid::new_v4(),"operation_id":Uuid::new_v4(),"expected_revision":1,"command":["true"]});
    assert!(
        recover_execution(&config, &args, CancellationToken::new())
            .await
            .unwrap()
            .is_none()
    );
    assert!(!root.path().join("unused").exists());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_caller_cancels_local_client_and_retains_output_until_explicit_recovery() {
    let fixture = fixture(false);
    let record = connected_existing(&fixture).await;
    let id = Uuid::new_v4();
    let args = json!({"environment_id":record.profile.id,"operation_id":id,"expected_revision":record.revision,"command":["__hang__"]});
    let config = fixture.config.clone();
    let request = args.clone();
    let call = tokio::spawn(async move {
        execute_tool(
            &config,
            "environment_execute",
            &request,
            CancellationToken::new(),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while !fixture.root.path().join("dispatched").exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("Fixture must reach native dispatch");
    call.abort();
    assert!(call.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(8), async {
        while active_sessions(&fixture.config) != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Retained cleanup must finish after cancellation");
    let receipt: EnvironmentReceipt = serde_json::from_value(
        management(
            &fixture.config,
            "helper_environment_operation",
            json!({"operation_id":id}),
        )
        .await,
    )
    .unwrap();
    assert_eq!(receipt.state, EnvironmentOperationState::OutcomeUnknown);
    let output = receipt.output.unwrap();
    assert_eq!(output.artifacts.len(), 2);
    assert!(
        output
            .artifacts
            .iter()
            .all(|artifact| !artifact.complete && artifact.stored_bytes > 0)
    );
    let retry = execute_tool(
        &fixture.config,
        "environment_execute",
        &args,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(retry.artifacts.len(), 2);
    assert_eq!(
        fs::read_to_string(fixture.root.path().join("executions"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}
