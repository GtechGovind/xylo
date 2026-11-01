//! OTLP export reaches a local collector and excludes conversation contents.
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    process::Command,
    time::{Duration, Instant},
};

#[test]
fn opt_in_otlp_exports_spans_tokens_cost_and_tool_latency_without_content() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let collector = std::thread::spawn(move || {
        let mut received = Vec::new();
        let mut deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut bytes = Vec::new();
                    let mut buffer = [0; 4096];
                    loop {
                        let length = stream.read(&mut buffer).unwrap();
                        assert!(length > 0);
                        bytes.extend_from_slice(&buffer[..length]);
                        if let Some(header_end) =
                            bytes.windows(4).position(|chunk| chunk == b"\r\n\r\n")
                        {
                            let headers = String::from_utf8_lossy(&bytes[..header_end]);
                            assert!(headers.to_lowercase().contains("x-xylo-auth: prefix+a+b"));
                            let length = headers
                                .lines()
                                .find_map(|line| {
                                    line.to_lowercase()
                                        .strip_prefix("content-length: ")
                                        .and_then(|length| length.parse::<usize>().ok())
                                })
                                .unwrap();
                            if bytes.len() >= header_end + 4 + length {
                                received.push((
                                    headers.lines().next().unwrap().to_owned(),
                                    serde_json::from_slice::<Value>(
                                        &bytes[header_end + 4..header_end + 4 + length],
                                    )
                                    .unwrap(),
                                ));
                                break;
                            }
                        }
                    }
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                        )
                        .unwrap();
                    deadline = Instant::now() + Duration::from_millis(250);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(error) => panic!("{error}"),
            }
        }
        received
    });
    let root = tempfile::tempdir().unwrap();
    let script = root.path().join("script.json");
    std::fs::write(&script,serde_json::to_vec(&json!({"steps":[{"step":"reply","tool_calls":[{"id":"list","name":"files_list","arguments":"{\"path\":\".\"}"}],"finish":"tool_calls"},{"step":"reply","text":"PRIVATE_ANSWER"}]})).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_xylo-cli"))
        .env("XYLO_EVAL_SCRIPT", script)
        .env("OTEL_TRACES_EXPORTER", "otlp")
        .env("OTEL_METRICS_EXPORTER", "otlp")
        .env("OTEL_EXPORTER_OTLP_HEADERS", "x-xylo-auth=prefix+a%2Bb")
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint)
        .env("XYLO_OTEL_INPUT_USD_PER_MILLION", "1")
        .env("XYLO_OTEL_OUTPUT_USD_PER_MILLION", "2")
        .args([
            "-p",
            "PRIVATE_PROMPT",
            "--debug",
            "--output-format",
            "json",
            "--data-dir",
        ])
        .arg(root.path().join("profile"))
        .arg("--cwd")
        .arg(root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let received = collector.join().unwrap();
    let body = serde_json::to_string(&received).unwrap();
    assert!(body.contains("resourceSpans"), "{body}");
    assert!(body.contains("conversation.run"), "{body}");
    for metric in ["xylo.tokens", "xylo.cost", "xylo.tool.duration"] {
        assert!(body.contains(metric), "missing {metric}: {body}");
    }
    assert!(!body.contains("PRIVATE_ANSWER") && !body.contains("PRIVATE_PROMPT"));
    assert!(root.path().join("profile/logs").is_dir());
}
