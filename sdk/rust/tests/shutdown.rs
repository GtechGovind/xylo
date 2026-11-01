//! Shutdown must make progress after a caller abandons a borrowed event stream.
#![cfg(unix)]

use std::{
    collections::HashMap,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};
use xylo_sdk::{Session, SessionOptions};

async fn peer(body: &str) -> (tempfile::TempDir, Session) {
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("peer.sh");
    let started = r#"{"version":1,"session_id":"00000000-0000-0000-0000-000000000001","turn_id":"00000000-0000-0000-0000-000000000002","sequence":1,"payload":{"event":"started"}}"#;
    std::fs::write(
        &executable,
        format!("#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$XYLO_SDK_PEER_PID\"\nprintf '%s\\n' '{started}'\n{body}\n"),
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let session = Session::new(SessionOptions {
        executable,
        environment: HashMap::from([(
            "XYLO_SDK_PEER_PID".into(),
            root.path().join("peer.pid").to_string_lossy().into_owned(),
        )]),
        ..Default::default()
    })
    .await
    .unwrap();
    (root, session)
}

#[tokio::test]
async fn close_drains_a_full_output_pipe_after_an_unfinished_stream_is_dropped() {
    let (root, mut session) = peer(
        r#"
# Fill stdout before looking at either run or shutdown on stdin.
dd if=/dev/zero bs=65536 count=64 2>/dev/null
IFS= read -r run || exit 10
IFS= read -r shutdown || exit 11
case "$shutdown" in
  *'"command":"shutdown"'*) exit 0 ;;
  *) exit 12 ;;
esac
"#,
    )
    .await;
    let control = session.control();
    {
        let mut events = session.stream("start").await.unwrap();
        assert!(events.next().await.unwrap().is_some());
    }
    let status = tokio::time::timeout(Duration::from_secs(3), session.close())
        .await
        .expect("Close must drain the pipe so the peer can read shutdown")
        .unwrap();
    assert_eq!(status, 0);
    assert_reaped(root.path());
    assert!(
        control.cancel().await.is_err(),
        "Closed controls must stay closed"
    );
}

#[tokio::test]
async fn close_kills_and_reaps_a_peer_that_ignores_shutdown() {
    let (root, mut session) = peer("exec sleep 60").await;
    {
        let mut events = session.stream("start").await.unwrap();
        assert!(events.next().await.unwrap().is_some());
    }
    let status = tokio::time::timeout(Duration::from_secs(12), session.close())
        .await
        .expect("Close must bound the graceful wait and reap a stopped peer")
        .unwrap();
    assert_eq!(status, 1);
    assert_reaped(root.path());
}

fn assert_reaped(root: &Path) {
    let pid = std::fs::read_to_string(root.join("peer.pid")).unwrap();
    let pid: u32 = pid.trim().parse().unwrap();
    let alive = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(
        !alive,
        "The owned CLI process must be stopped and reaped before close returns"
    );
}
