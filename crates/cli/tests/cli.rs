//! Exercises the public executable with the real engine and scripted provider.
use serde_json::{Value, json};
use std::{
    io::Write,
    process::{Command, Stdio},
};

fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let script = root.path().join("script.json");
    std::fs::write(&script, serde_json::to_vec(&json!({"steps":[
        {"step":"reply","text":"","tool_calls":[{"id":"list","name":"files_list","arguments":"{\"path\":\".\"}"}],"finish":"tool_calls"},
        {"step":"reply","text":"Listed the workspace.","tool_calls":[],"finish":"stop"}
    ]})).unwrap()).unwrap();
    (root, script)
}

#[test]
fn json_print_returns_the_answer_and_conversation_identity() {
    let (root, script) = fixture();
    let run = Command::new(env!("CARGO_BIN_EXE_xylo-cli"))
        .env("XYLO_EVAL_SCRIPT", script)
        .args(["-p", "list files", "--output-format", "json", "--cwd"])
        .arg(root.path())
        .arg("--data-dir")
        .arg(root.path().join("profile"))
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let output: Value = serde_json::from_slice(&run.stdout).unwrap();
    assert_eq!(output["text"], "Listed the workspace.");
    assert_eq!(output["exit_code"], 0);
    assert!(output["session_id"].as_str().is_some());
}

#[test]
fn rejects_conflicting_and_unknown_options_without_opening_profile() {
    for args in [
        vec!["--continue", "--resume", "bad"],
        vec!["--output-format", "xml"],
        vec!["--max-turns", "0"],
        vec!["--unknown"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_xylo-cli"))
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
    }
}

#[test]
fn versioned_input_rejects_an_unsupported_version() {
    let root = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_xylo-cli"))
        .args([
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--data-dir",
        ])
        .arg(root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"version\":999,\"payload\":{\"command\":\"run\",\"prompt\":\"hello\"}}\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("version"));
}

#[test]
fn stream_input_eof_finishes_a_noninteractive_run_and_exports_history() {
    let (root, script) = fixture();
    let export = root.path().join("export.md");
    let mut child = Command::new(env!("CARGO_BIN_EXE_xylo-cli"))
        .env("XYLO_EVAL_SCRIPT", script)
        .args([
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--data-dir",
        ])
        .arg(root.path().join("profile"))
        .arg("--cwd")
        .arg(root.path())
        .arg("--export")
        .arg(&export)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"version\":1,\"payload\":{\"command\":\"run\",\"prompt\":\"list files\"}}\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let frames = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<xylo_core::stdio::Event>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(
        frames
            .iter()
            .enumerate()
            .all(|(index, frame)| frame.sequence == index as u64 + 1)
    );
    assert_eq!(frames.last().unwrap().exit_code, Some(0));
    assert!(frames.iter().any(|frame| matches!(&frame.payload, xylo_core::EngineEvent::ToolFinished { name, success: true, .. } if name == "files_list")));
    assert!(export.exists());
    assert_eq!(
        std::fs::read_to_string(export.with_extension("jsonl"))
            .unwrap()
            .lines()
            .count(),
        2
    );
}

#[test]
fn stdin_prompt_and_continue_use_the_saved_conversation() {
    let (root, script) = fixture();
    let mut child = Command::new(env!("CARGO_BIN_EXE_xylo-cli"))
        .env("XYLO_EVAL_SCRIPT", &script)
        .args(["--output-format", "json", "--data-dir"])
        .arg(root.path().join("profile"))
        .arg("--cwd")
        .arg(root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"list files")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let first: Value = serde_json::from_slice(&output.stdout).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_xylo-cli"))
        .env("XYLO_EVAL_SCRIPT", script)
        .args([
            "-p",
            "again",
            "--continue",
            "--output-format",
            "json",
            "--data-dir",
        ])
        .arg(root.path().join("profile"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let second: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(first["session_id"], second["session_id"]);
}

#[test]
fn relative_data_directory_and_provider_error_have_a_structured_failure() {
    let root = tempfile::tempdir().unwrap();
    let script = root.path().join("failure.json");
    std::fs::write(&script,serde_json::to_vec(&json!({"steps":[{"step":"fail","kind":"permanent","message":"Scripted provider rejected the request"}]})).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_xylo-cli"))
        .current_dir(root.path())
        .env("XYLO_EVAL_SCRIPT", script)
        .args([
            "-p",
            "fail clearly",
            "--output-format",
            "json",
            "--data-dir",
            "profile",
        ])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(body["exit_code"], 1);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("Scripted provider rejected")
    );
    assert!(root.path().join("profile/logs").is_dir());
}
