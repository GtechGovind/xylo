use super::*;
#[test]
fn launcher_path_excludes_relative_project_and_package_directories() {
    let project = tempfile::tempdir().unwrap();
    let package = tempfile::tempdir().unwrap();
    let system = tempfile::tempdir().unwrap();
    let project = project.path().canonicalize().unwrap();
    let package = package.path().canonicalize().unwrap();
    let system = system.path().canonicalize().unwrap();
    let value = std::env::join_paths([
        Path::new("."),
        project.as_path(),
        package.as_path(),
        system.as_path(),
    ])
    .unwrap();
    let actual = launcher_path(Some(&value), &project, &[package]).unwrap();
    assert_eq!(
        std::env::split_paths(&actual).collect::<Vec<_>>(),
        vec![system]
    );
}
#[test]
fn launcher_cannot_live_in_the_mutable_project() {
    let node = runtime_node().unwrap();
    let result = prepare(
        "/bin/echo",
        &["not executed".into()],
        node.parent().unwrap(),
        &[],
        &[],
    );
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("outside the mutable project")
    );
}
#[test]
fn node_versions_are_parsed_and_compared() {
    assert_eq!(parse_node_version("v22.12.0\n"), Some((22, 12)));
    assert_eq!(parse_node_version("v26.0.0"), Some((26, 0)));
    assert_eq!(parse_node_version("22.12.0"), None);
    assert_eq!(parse_node_version("vX.1"), None);
    assert!(Some((22, 11)) < Some(MINIMUM_NODE));
    assert!((23, 0) >= MINIMUM_NODE);
}

#[cfg(unix)]
#[test]
fn discovered_node_is_trusted_only_when_user_owned_and_not_shared_writable() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().canonicalize().unwrap();
    let installed = home.join(".nvm/versions/node/v24.0.0/bin");
    std::fs::create_dir_all(&installed).unwrap();
    let node = installed.join("node");
    std::fs::write(&node, "#!/bin/sh\n").unwrap();
    for path in [
        home.join(".nvm"),
        home.join(".nvm/versions"),
        home.join(".nvm/versions/node"),
        home.join(".nvm/versions/node/v24.0.0"),
        installed.clone(),
        node.clone(),
    ] {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    assert!(
        trusted_system_location(&node, Some(&home)).is_ok(),
        "A user-owned 0755 Node under HOME is accepted"
    );
    std::fs::set_permissions(&installed, std::fs::Permissions::from_mode(0o775)).unwrap();
    let error = trusted_system_location(&node, Some(&home)).unwrap_err();
    assert!(
        error.to_string().contains("group-writable")
            && error.to_string().contains(installed.to_str().unwrap()),
        "{error:#}"
    );
    let unavailable =
        SandboxUnavailable(format!("Node.js {} was rejected: {error}", node.display()));
    assert!(unavailable.to_string().starts_with(UNAVAILABLE_MARKER));
}

struct Finished {
    output: std::process::Output,
    start: Option<StartFailure>,
    denials: Vec<String>,
}

async fn sandbox_output(mut prepared: PreparedSandbox) -> Finished {
    prepared
        .command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        prepared.command.output(),
    )
    .await
    .unwrap()
    .unwrap();
    Finished {
        output,
        start: read_start_status(&prepared.status_file),
        denials: read_network_denials(&prepared.status_file),
    }
}

#[tokio::test]
async fn setup_failure_is_recorded_as_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let mut prepared = prepare("/bin/echo", &["never".into()], dir.path(), &[], &[]).unwrap();
    prepared.command.env("XYLO_SANDBOX_CHILD_ENV", "[]");
    let finished = sandbox_output(prepared).await;
    assert_eq!(finished.output.status.code(), Some(UNAVAILABLE_EXIT));
    assert!(
        matches!(&finished.start, Some(StartFailure::Unavailable(reason)) if reason.contains("environment")),
        "{:?}",
        finished.start
    );
    assert!(finished.output.stdout.is_empty());
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn unspawnable_program_is_recorded_and_tool_statuses_pass_through() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing-program");
    let prepared = prepare(missing.to_str().unwrap(), &[], dir.path(), &[], &[]).unwrap();
    let finished = sandbox_output(prepared).await;
    assert_eq!(finished.output.status.code(), Some(SPAWN_FAILED_EXIT));
    assert!(
        matches!(finished.start, Some(StartFailure::SpawnFailed(_))),
        "{:?}",
        finished.start
    );
    let prepared = prepare(
        "/bin/sh",
        &["-c".into(), "exit 125".into()],
        dir.path(),
        &[],
        &[],
    )
    .unwrap();
    let finished = sandbox_output(prepared).await;
    assert_eq!(finished.output.status.code(), Some(125));
    assert_eq!(finished.start, None);
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn a_program_cannot_forge_a_start_failure() {
    let dir = tempfile::tempdir().unwrap();
    // The status file sits beside the child's TMPDIR, so the program can
    // find it; the sandbox must still refuse to read or write it.
    let forged = r#"printf ran > ran.txt
record='{"kind":"spawn_failed","reason":"forged"}'
echo 'XYLO_SANDBOX_SPAWN_FAILED: forged' >&2
echo 'XYLO_SANDBOX_UNAVAILABLE: forged' >&2
echo "$record" >&3 2>/dev/null
echo "$record" >> "$TMPDIR/../control/status" 2>/dev/null && echo wrote-status
cat "$TMPDIR/../control/status" 2>/dev/null && echo read-status
exit 127"#;
    let prepared = prepare(
        "/bin/sh",
        &["-c".into(), forged.into()],
        dir.path(),
        &[],
        &[],
    )
    .unwrap();
    let finished = sandbox_output(prepared).await;
    let stdout = String::from_utf8_lossy(&finished.output.stdout);
    assert!(dir.path().join("ran.txt").exists(), "The program ran");
    assert!(
        !stdout.contains("wrote-status") && !stdout.contains("read-status"),
        "{stdout}"
    );
    assert_eq!(finished.output.status.code(), Some(127));
    assert_eq!(
        finished.start, None,
        "Program output is never evidence that nothing ran"
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[tokio::test]
async fn offline_commands_report_blocked_hosts_and_unrestricted_commands_reach_the_proxy() {
    let dir = tempfile::tempdir().unwrap();
    let curl = [
        "-sS",
        "-m",
        "10",
        "-o",
        "/dev/null",
        "-w",
        "%{http_code}",
        "http://example.invalid/",
    ]
    .map(String::from);
    let offline =
        prepare_coding_with_network("/usr/bin/curl", &curl, dir.path(), &[], &[], Network::None)
            .unwrap();
    let finished = sandbox_output(offline).await;
    assert_eq!(finished.denials, vec!["example.invalid:80"]);
    assert_eq!(finished.start, None);
    let online = prepare_coding_with_network(
        "/usr/bin/curl",
        &curl,
        dir.path(),
        &[],
        &[],
        Network::Unrestricted,
    )
    .unwrap();
    let spec: serde_json::Value = serde_json::from_str(
        online
            .command
            .as_std()
            .get_args()
            .nth(1)
            .unwrap()
            .to_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(spec["network"], "unrestricted");
    assert!(
        spec["coding_environment"]
            .get("CARGO_NET_OFFLINE")
            .is_none()
    );
    let finished = sandbox_output(online).await;
    let stdout = String::from_utf8_lossy(&finished.output.stdout);
    let stderr = String::from_utf8_lossy(&finished.output.stderr);
    assert!(finished.denials.is_empty() && finished.start.is_none());
    assert_ne!(
        stdout.trim(),
        "403",
        "The proxy must not refuse the host: {stderr}"
    );
}

#[test]
fn sandbox_does_not_inherit_provider_environment() {
    let dir = tempfile::tempdir().unwrap();
    let prepared = prepare("/bin/echo", &["hello".into()], dir.path(), &[], &[]).unwrap();
    let vars: Vec<_> = prepared
        .command
        .as_std()
        .get_envs()
        .filter_map(|(key, value)| value.map(|_| key.to_string_lossy().into_owned()))
        .collect();
    assert!(vars.iter().all(|k| {
        [
            "PATH",
            "HOME",
            "USER",
            "LOGNAME",
            "LANG",
            "LC_ALL",
            "TMPDIR",
            "SYSTEMROOT",
        ]
        .contains(&k.as_str())
    }));
}
