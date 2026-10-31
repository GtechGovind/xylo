use super::*;
use crate::VaultKey;
use rmcp::model::ToolAnnotations;
use serde_json::json;

fn annotations(read_only: Option<bool>, destructive: Option<bool>) -> ToolAnnotations {
    let mut annotations = ToolAnnotations::default();
    annotations.read_only_hint = read_only;
    annotations.destructive_hint = destructive;
    annotations
}

#[test]
fn only_read_only_non_destructive_tools_skip_the_write_gate() {
    assert!(tool_writes(None));
    assert!(tool_writes(Some(&annotations(None, None))));
    assert!(tool_writes(Some(&annotations(Some(false), None))));
    assert!(tool_writes(Some(&annotations(Some(true), Some(true)))));
    assert!(!tool_writes(Some(&annotations(Some(true), None))));
    assert!(!tool_writes(Some(&annotations(Some(true), Some(false)))));
}

#[test]
fn stderr_capture_is_bounded_in_lines_and_bytes() {
    let mut state = StderrState::default();
    for line in 0..100 {
        state.append(format!("line {line}\n").as_bytes());
    }
    assert_eq!(state.lines, STDERR_MAX_LINES);
    assert!(String::from_utf8_lossy(&state.bytes).ends_with("line 19\n"));
    let mut state = StderrState::default();
    state.append(&vec![b'x'; 3 * STDERR_MAX_BYTES]);
    assert_eq!(state.bytes.len(), STDERR_MAX_BYTES);
}

#[cfg(unix)]
struct Fixture {
    directory: tempfile::TempDir,
    workspace: PathBuf,
    broker: ConnectorBroker,
    config: ConnectorConfig,
}

/// A sandboxed Python MCP server. It logs launches, cancellation notices and
/// stdin EOF to `events.txt` in its working directory so tests can observe
/// the lifecycle from outside.
#[cfg(unix)]
fn fixture(server_body: &str) -> Fixture {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let script = directory.path().join("fixture-mcp");
    std::fs::write(
        &script,
        format!(
            r#"#!/usr/bin/python3
import json,sys
def log(event):
    with open('events.txt','a') as events: events.write(event+'\n')
def reply(request,result):
    print(json.dumps({{'jsonrpc':'2.0','id':request['id'],'result':result}}),flush=True)
log('launch')
{server_body}
"#
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let store = Store::open(directory.path().join("xylo.db")).unwrap();
    let vault = CredentialVault::unlocked(directory.path(), VaultKey::from_bytes([9; 32])).unwrap();
    let broker = ConnectorBroker::new(store, vault).with_local_scope(ConnectorLaunchScope {
        workspace: workspace.clone(),
        read_roots: vec![script.clone()],
        ..Default::default()
    });
    let config = ConnectorConfig {
        id: Uuid::new_v4(),
        name: "Lifecycle fixture".into(),
        transport: "stdio".into(),
        endpoint: String::new(),
        command: Some(script),
        args: Vec::new(),
        credential_ref: None,
    };
    Fixture {
        directory,
        workspace,
        broker,
        config,
    }
}

#[cfg(unix)]
const SERVE_LOOP: &str = r#"
while True:
    line=sys.stdin.readline()
    if not line:
        log('eof'); break
    request=json.loads(line)
    method=request.get('method')
    if method=='notifications/cancelled':
        log('cancelled:'+str(request['params'].get('requestId'))); continue
    if 'id' not in request: continue
    if method=='initialize':
        reply(request,{'protocolVersion':request['params']['protocolVersion'],'capabilities':{'tools':{}},'serverInfo':{'name':'lifecycle','version':'1'}})
    elif method=='tools/call' and request['params']['name']=='slow':
        log('slow-call:'+str(request['id']))
    elif method=='tools/call':
        reply(request,{'content':[{'type':'text','text':'ok'}]})
"#;

#[cfg(unix)]
fn events(fixture: &Fixture) -> Vec<String> {
    std::fs::read_to_string(fixture.workspace.join("events.txt"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[cfg(unix)]
#[tokio::test]
async fn initialization_failure_reports_the_servers_stderr() {
    let fixture = fixture(
        "sys.stderr.write('fixture: missing runtime dependency libfixture\\n'); sys.stderr.flush(); sys.exit(3)",
    );
    let error = fixture
        .broker
        .call(
            &fixture.config,
            "echo",
            json!({}),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("missing runtime dependency libfixture"),
        "stderr must explain the failure: {error}"
    );
    drop(fixture.directory);
}

#[cfg(unix)]
#[tokio::test]
async fn cancelled_call_notifies_the_server_and_closes_stdin_before_kill() {
    let fixture = fixture(SERVE_LOOP);
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    // Cancel only once the server has the request, however slow the sandboxed
    // server is to start on a loaded machine.
    let log = fixture.workspace.join("events.txt");
    tokio::spawn(async move {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline
            && !std::fs::read_to_string(&log)
                .unwrap_or_default()
                .contains("slow-call:")
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        trigger.cancel();
    });
    let error = fixture
        .broker
        .call(&fixture.config, "slow", json!({}), &cancel)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error}");
    let events = events(&fixture);
    let call = events
        .iter()
        .find_map(|event| event.strip_prefix("slow-call:"))
        .expect("the call reached the server")
        .to_owned();
    assert!(
        events.contains(&format!("cancelled:{call}")),
        "the in-flight request must be withdrawn: {events:?}"
    );
    assert_eq!(
        events.last().map(String::as_str),
        Some("eof"),
        "the server must see stdin close (graceful shutdown): {events:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn connections_survive_across_broker_clones_until_idle() {
    let fixture = fixture(SERVE_LOOP);
    let broker = fixture
        .broker
        .clone()
        .with_idle_timeout(Duration::from_millis(400));
    let cancel = CancellationToken::new();
    broker
        .call(&fixture.config, "echo", json!({}), &cancel)
        .await
        .unwrap();
    // A second "turn" through another clone reuses the live connection.
    let next_turn = broker.clone();
    next_turn
        .call(&fixture.config, "echo", json!({}), &cancel)
        .await
        .unwrap();
    assert_eq!(events(&fixture), ["launch"]);
    // The reaper closes the idle connection gracefully.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !events(&fixture).contains(&"eof".to_owned()) {
        assert!(Instant::now() < deadline, "idle connection was not reaped");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    next_turn
        .call(&fixture.config, "echo", json!({}), &cancel)
        .await
        .unwrap();
    assert_eq!(events(&fixture), ["launch", "eof", "launch"]);
    next_turn.shutdown().await;
    assert_eq!(events(&fixture).last().map(String::as_str), Some("eof"));
}
