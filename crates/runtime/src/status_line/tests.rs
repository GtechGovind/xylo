use super::*;

#[tokio::test]
async fn status_command_receives_json_stdin_is_bounded_and_throttled() {
    let root = tempfile::tempdir().unwrap();
    let runner = Runner::default();
    let input = serde_json::json!({"session_id":"test","model":"fixture"});
    let output = runner
        .run(
            root.path(),
            "read -r line; printf '%s\\nignored' \"$line\"",
            &input,
            CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&output).unwrap(),
        input
    );
    assert!(
        runner
            .run(
                root.path(),
                "printf forbidden",
                &input,
                CancellationToken::new()
            )
            .await
            .unwrap()
            .is_none()
    );
    tokio::time::sleep(std::time::Duration::from_millis(310)).await;
    let output = runner
        .run(
            root.path(),
            "head -c 4096 /dev/zero | tr '\\000' x",
            &input,
            CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(output.len(), 1024);
}

#[tokio::test]
async fn status_commands_are_read_only_and_timeout_without_overlap() {
    let root = tempfile::tempdir().unwrap();
    let runner = Runner::default();
    let input = serde_json::json!({});
    let result = runner
        .run(
            root.path(),
            "printf changed > status-must-not-write",
            &input,
            CancellationToken::new(),
        )
        .await;
    assert!(result.is_err());
    assert!(!root.path().join("status-must-not-write").exists());
    tokio::time::sleep(std::time::Duration::from_millis(310)).await;
    let slow = runner.run(root.path(), "sleep 20", &input, CancellationToken::new());
    let overlapping = async {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        runner
            .run(
                root.path(),
                "printf overlap",
                &input,
                CancellationToken::new(),
            )
            .await
            .unwrap()
    };
    let (slow, other) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(slow, overlapping)
    })
    .await
    .unwrap();
    assert!(slow.is_err());
    assert!(other.is_none());
}
