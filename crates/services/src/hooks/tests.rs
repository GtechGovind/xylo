//! Real hook descendants must be gone before the next tool can run.
use super::*;

#[tokio::test]
async fn completed_hook_closes_inherited_pipes_and_fences_descendants() {
    let root = tempfile::tempdir().unwrap();
    // Linux sandbox PIDs are namespace-local, so checking $! with host ps
    // can inspect an unrelated host process. Identify this exact executable.
    let child = root.path().canonicalize().unwrap().join("hook-sleeper");
    std::fs::copy("/bin/sleep", &child).unwrap();
    let child_name = child.to_str().unwrap();
    let hook = CommandHook {
        command: format!("{} 300 & printf '{{}}'", shell_words::quote(child_name)),
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
    let output = std::process::Command::new("/bin/ps")
        .args(["-axww", "-o", "stat=,args="])
        .output()
        .unwrap();
    assert!(output.status.success());
    let processes = String::from_utf8_lossy(&output.stdout);
    assert!(
        !processes
            .lines()
            .any(|line| line.contains(child_name) && !line.trim_start().starts_with('Z')),
        "hook returned with its descendant executable still running"
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
