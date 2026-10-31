use super::*;
#[cfg(unix)]
#[tokio::test]
async fn capture_is_bounded_and_timeout_terminates() {
    let root = tempfile::tempdir().unwrap();
    let spec = ProcessSpec {
        executable: PathBuf::from("/bin/sh"),
        args: vec!["-c".into(), "yes data | head -c 65536".into()],
        cwd: root.path().into(),
        env: BTreeMap::new(),
        timeout: Duration::from_secs(5),
        output_limit: 1024,
    };
    let result = run(spec.clone(), CancellationToken::new()).await.unwrap();
    assert!(result.success && result.truncated);
    assert_eq!(result.stdout.len(), 1024);
    let spec = ProcessSpec {
        args: vec!["-c".into(), "sleep 30".into()],
        timeout: Duration::from_millis(50),
        ..spec
    };
    assert!(
        run(spec, CancellationToken::new())
            .await
            .unwrap_err()
            .to_string()
            .contains("exceeded")
    );
}
#[cfg(unix)]
#[tokio::test]
async fn helpers_see_only_allowlisted_and_explicit_variables() {
    let root = tempfile::tempdir().unwrap();
    let result = run(
        ProcessSpec {
            executable: PathBuf::from("/usr/bin/env"),
            args: vec![],
            cwd: root.path().into(),
            env: BTreeMap::from([("XYLO_EXPLICIT".to_owned(), "1".to_owned())]),
            timeout: Duration::from_secs(5),
            output_limit: 1024 * 1024,
        },
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let names: Vec<_> = result
        .stdout
        .lines()
        .filter_map(|line| line.split_once('=').map(|(name, _)| name))
        .collect();
    assert!(names.contains(&"XYLO_EXPLICIT"));
    for name in names {
        assert!(
            BASELINE.contains(&name) || name.starts_with("LC_") || name == "XYLO_EXPLICIT",
            "Native helper inherited {name}"
        );
    }
}
#[test]
fn relative_helpers_are_rejected() {
    assert!(validate_executable(std::path::Path::new("ssh")).is_err());
}
