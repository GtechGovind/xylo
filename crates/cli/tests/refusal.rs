//! Policy refusal must be visible to automation and must never change a file.
use serde_json::json;
use std::process::Command;

#[test]
fn read_only_and_tool_deny_list_return_refused_without_writing() {
    for (flag, value) in [
        ("--permission-mode", "read_only"),
        ("--disallowed-tools", "file_write"),
        ("--allowed-tools", "Read"),
    ] {
        let root = tempfile::tempdir().unwrap();
        let script = root.path().join("script.json");
        std::fs::write(&script, serde_json::to_vec(&json!({"steps":[
            {"step":"reply","text":"","tool_calls":[{"id":"write","name":"file_write","arguments":"{\"path\":\"blocked.txt\",\"content\":\"unsafe\"}"}],"finish":"tool_calls"},
            {"step":"reply","text":"The write was refused.","tool_calls":[],"finish":"stop"}
        ]})).unwrap()).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_xylo-cli"))
            .env("XYLO_EVAL_SCRIPT", script)
            .args([
                "-p",
                "write a file",
                "--output-format",
                "json",
                flag,
                value,
                "--data-dir",
            ])
            .arg(root.path().join("profile"))
            .arg("--cwd")
            .arg(root.path())
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(2),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!root.path().join("blocked.txt").exists());
        let body: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(body["exit_code"], 2);
    }
}

#[test]
fn noninteractive_approval_is_denied_with_a_clear_error() {
    let root = tempfile::tempdir().unwrap();
    let script = root.path().join("script.json");
    let path = root.path().join(".env");
    std::fs::write(&script,serde_json::to_vec(&json!({"steps":[{"step":"reply","tool_calls":[{"id":"write","name":"file_write","arguments":serde_json::to_string(&json!({"path":".env","content":"blocked"})).unwrap()}],"finish":"tool_calls"},{"step":"reply","text":"Denied."}]})).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_xylo-cli"))
        .env("XYLO_EVAL_SCRIPT", script)
        .args([
            "-p",
            "write workflow",
            "--permission-mode",
            "ask_for_approval",
            "--output-format",
            "json",
            "--data-dir",
        ])
        .arg(root.path().join("profile"))
        .arg("--cwd")
        .arg(root.path())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!path.exists());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Refused"));
}
