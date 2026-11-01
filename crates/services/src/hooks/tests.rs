//! Real hook descendants must be gone before the next tool can run.
use super::*;

#[tokio::test]
async fn completed_hook_closes_inherited_pipes_and_fences_descendants() {
    let root = tempfile::tempdir().unwrap();
    let child = root.path().join("child.pid");
    let hook = CommandHook {
        command: "sleep 300 & printf '%s' \"$!\" > child.pid; printf '{}'".into(),
        timeout: std::time::Duration::from_secs(3),
    };
    let result = run(
        &hook,
        &serde_json::json!({}),
        root.path(),
        &[],
        CancellationToken::new(),
    )
    .await;
    assert_eq!(result.unwrap(), serde_json::json!({}));
    let pid = std::fs::read_to_string(&child).unwrap();
    let output = std::process::Command::new("/bin/ps")
        .args(["-p", pid.trim(), "-o", "stat="])
        .output()
        .unwrap();
    let state = String::from_utf8_lossy(&output.stdout);
    assert!(
        state.trim().is_empty() || state.trim().starts_with('Z'),
        "hook returned with a live descendant: {state}"
    );
}

#[tokio::test]
async fn guarded_hooks_preserve_private_input_and_deny_exit_code() {
    let root = tempfile::tempdir().unwrap();
    let mut hook = CommandHook {
        command: "cat".into(),
        timeout: Duration::from_secs(3),
    };
    let input = serde_json::json!({"hookSpecificOutput":{"additionalContext":"private input"}});
    assert_eq!(
        run(&hook, &input, root.path(), &[], CancellationToken::new())
            .await
            .unwrap(),
        input
    );
    hook.command = "exit 2".into();
    assert_eq!(
        run(
            &hook,
            &serde_json::json!({}),
            root.path(),
            &[],
            CancellationToken::new()
        )
        .await
        .unwrap(),
        serde_json::json!({"decision":"deny"})
    );
}
