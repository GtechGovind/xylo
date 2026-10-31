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
        trusted: false,
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

#[test]
fn trusting_a_server_keeps_its_connection_identity_and_catalog() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("xylo.db")).unwrap();
    let vault = CredentialVault::unlocked(directory.path(), VaultKey::from_bytes([9; 32])).unwrap();
    let broker = ConnectorBroker::new(store, vault);
    let config = ConnectorConfig {
        id: Uuid::new_v4(),
        name: "Trust fixture".into(),
        transport: "http".into(),
        endpoint: "https://tools.example.com/mcp".into(),
        command: None,
        args: Vec::new(),
        credential_ref: None,
        trusted: false,
    };
    let tool = ToolDefinition {
        name: "lookup".into(),
        description: "Look up".into(),
        parameters: json!({"type":"object"}),
        writes: false,
    };
    broker
        .cache_tools_for_tests(&config, std::slice::from_ref(&tool))
        .unwrap();
    let trusted = ConnectorConfig {
        trusted: true,
        ..config.clone()
    };
    assert_eq!(
        ConnectorBroker::fingerprint(&config, None).unwrap(),
        ConnectorBroker::fingerprint(&trusted, None).unwrap()
    );
    assert_eq!(broker.cached_tools(&trusted).unwrap()[0].name, "lookup");
}

/// How the loopback MCP fixture answers `initialize`.
#[derive(Clone, Copy)]
enum InitializeReply {
    /// A normal JSON reply; `tools/list` is then answered over SSE.
    Normal,
    /// A chunked JSON body with no `Content-Length` that runs past the cap.
    ChunkedOversized,
    /// An honest `Content-Length` above the cap.
    DeclaredOversized,
}

/// Serves streamable-HTTP MCP on loopback, one request per connection.
async fn http_fixture(
    reply: InitializeReply,
) -> (ConnectorBroker, ConnectorConfig, tempfile::TempDir) {
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let Some((request_head, body)) = read_http_request(&mut socket).await else {
                    return;
                };
                let request: Value = serde_json::from_slice(&body).unwrap_or_default();
                let id = request["id"].clone();
                let head = |status: &str, extra: &str| {
                    format!("HTTP/1.1 {status}\r\nConnection: close\r\n{extra}\r\n")
                };
                let json = |value: Value| {
                    let body = value.to_string();
                    format!(
                        "{}{body}",
                        head(
                            "200 OK",
                            &format!(
                                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                                body.len()
                            )
                        )
                    )
                };
                let response = if !request_head.starts_with(b"POST") {
                    head("405 Method Not Allowed", "Content-Length: 0\r\n")
                } else {
                    match (request["method"].as_str(), reply) {
                        (Some("initialize"), InitializeReply::Normal) => json(json!({
                            "jsonrpc": "2.0", "id": id, "result": {
                                "protocolVersion": request["params"]["protocolVersion"],
                                "capabilities": {"tools": {}},
                                "serverInfo": {"name": "fixture", "version": "1"},
                            }
                        })),
                        (Some("initialize"), InitializeReply::ChunkedOversized) => {
                            let _ = socket
                                .write_all(
                                    head(
                                        "200 OK",
                                        "Content-Type: application/json\r\nTransfer-Encoding: chunked\r\n",
                                    )
                                    .as_bytes(),
                                )
                                .await;
                            // Up to 64 MiB, so a missing cap shows up as an
                            // unparseable 64 MiB reply instead of the cap error.
                            let chunk = vec![b' '; 64 * 1024];
                            let framed =
                                [format!("{:x}\r\n", chunk.len()).as_bytes(), &chunk, b"\r\n"]
                                    .concat();
                            for _ in 0..1024 {
                                if socket.write_all(&framed).await.is_err() {
                                    return;
                                }
                            }
                            let _ = socket.write_all(b"0\r\n\r\n").await;
                            return;
                        }
                        (Some("initialize"), InitializeReply::DeclaredOversized) => head(
                            "200 OK",
                            "Content-Type: application/json\r\nContent-Length: 9437184\r\n",
                        ),
                        (Some("tools/list"), _) => {
                            let event = json!({
                                "jsonrpc": "2.0", "id": id, "result": {"tools": [{
                                    "name": "lookup",
                                    "description": "Look up",
                                    "inputSchema": {"type": "object"},
                                }]}
                            });
                            // CRLF line endings exercise the SSE parser.
                            let body = format!(": keepalive\r\nid: 1\r\ndata: {event}\r\n\r\n");
                            format!(
                                "{}{body}",
                                head(
                                    "200 OK",
                                    &format!(
                                        "Content-Type: text/event-stream\r\nContent-Length: {}\r\n",
                                        body.len()
                                    )
                                )
                            )
                        }
                        _ => head("202 Accepted", "Content-Length: 0\r\n"),
                    }
                };
                let _ = socket.write_all(response.as_bytes()).await;
                // Keep a declared-but-unsent body pending so only the header
                // check can end the exchange quickly.
                tokio::time::sleep(Duration::from_secs(5)).await;
            });
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("xylo.db")).unwrap();
    let vault = CredentialVault::unlocked(directory.path(), VaultKey::from_bytes([9; 32])).unwrap();
    let config = ConnectorConfig {
        id: Uuid::new_v4(),
        name: "HTTP fixture".into(),
        transport: "http".into(),
        endpoint: format!("http://{address}/mcp"),
        command: None,
        args: Vec::new(),
        credential_ref: None,
        trusted: false,
    };
    (ConnectorBroker::new(store, vault), config, directory)
}

/// Reads one HTTP/1.1 request with a `Content-Length` body.
async fn read_http_request(socket: &mut tokio::net::TcpStream) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut buffer = Vec::new();
    let header_end = loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
        let mut chunk = [0; 4096];
        let read = socket
            .read(&mut chunk)
            .await
            .ok()
            .filter(|read| *read > 0)?;
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_ascii_lowercase();
    let length = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buffer.split_off(header_end);
    while body.len() < length {
        let mut chunk = [0; 4096];
        let read = socket
            .read(&mut chunk)
            .await
            .ok()
            .filter(|read| *read > 0)?;
        body.extend_from_slice(&chunk[..read]);
    }
    Some((buffer, body))
}

#[tokio::test]
async fn remote_initialize_rejects_a_chunked_body_over_the_cap() {
    let (broker, config, _directory) = http_fixture(InitializeReply::ChunkedOversized).await;
    let started = Instant::now();
    let error = broker
        .tools(&config, &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("exceeds the 8 MiB size limit"),
        "{error:#}"
    );
    assert!(started.elapsed() < Duration::from_secs(15));
}

#[tokio::test]
async fn remote_initialize_rejects_a_declared_length_over_the_cap() {
    let (broker, config, _directory) = http_fixture(InitializeReply::DeclaredOversized).await;
    let started = Instant::now();
    let error = broker
        .tools(&config, &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("exceeds the 8 MiB size limit"),
        "{error:#}"
    );
    // The body is never sent, so only the up-front length check finishes fast.
    assert!(started.elapsed() < Duration::from_secs(4));
}

#[tokio::test]
async fn remote_server_within_the_cap_lists_tools_over_json_and_sse() {
    let (broker, config, _directory) = http_fixture(InitializeReply::Normal).await;
    let tools = broker
        .tools(&config, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "lookup");
    broker.shutdown().await;
}

#[test]
fn sse_parser_bounds_unterminated_lines_and_events() {
    let mut parser = SseParser::new(16);
    assert!(parser.feed(b"data: 0123456789abcdef").is_err());
    let mut parser = SseParser::new(16);
    parser.feed(b"data: 0123456\n").unwrap();
    assert!(parser.feed(b"data: 789abcdef\n").is_err());
}

#[test]
fn sse_parser_joins_data_and_handles_split_crlf() {
    let mut parser = SseParser::new(1024);
    parser.feed(b"\xEF\xBB\xBFid: 7\r").unwrap();
    parser
        .feed(b"\ndata: a\r\ndata: b\r\n:comment\r\nretry: 5\r\n\r")
        .unwrap();
    parser.feed(b"\ndata: c\n").unwrap();
    let event = parser.ready.pop_front().unwrap();
    assert_eq!(event.id.as_deref(), Some("7"));
    assert_eq!(event.data.as_deref(), Some("a\nb"));
    assert_eq!(event.retry, Some(5));
    // Unterminated events are never emitted.
    assert!(parser.ready.is_empty());
}
