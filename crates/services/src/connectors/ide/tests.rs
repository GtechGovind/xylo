//! Discovery fixtures never use the real home or IDE credentials.
use super::*;
use serde_json::json;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let discovery = dir.path().join(".xylo/ide");
    std::fs::create_dir_all(&discovery).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&discovery, std::fs::Permissions::from_mode(0o700)).unwrap();
    let lock = discovery.join("test.json");
    std::fs::write(&lock, json!({"version":1,"pid":std::process::id(),"name":"VS Code","endpoint":"ws://127.0.0.1:35791/mcp","token":"a".repeat(64),"workspaces":[root.canonicalize().unwrap()]}).to_string()).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600)).unwrap();
    (dir, root, lock)
}
#[test]
fn discovery_is_private_scoped_and_does_not_expose_token() {
    let (dir, root, _) = fixture();
    let found = discover(dir.path(), &root).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].connector.name, "VS Code");
    assert_eq!(found[0].connector.transport, "ide");
    assert!(!format!("{:?}", found[0]).contains(&"a".repeat(64)));
    let sibling = dir.path().join("project-other");
    std::fs::create_dir(&sibling).unwrap();
    assert!(discover(dir.path(), &sibling).unwrap().is_empty());
}
#[test]
fn discovery_rejects_remote_endpoints_and_bad_tokens() {
    let (dir, root, lock) = fixture();
    for (endpoint, token) in [
        ("ws://example.com:35791/mcp", "a".repeat(64)),
        ("ws://127.0.0.1:35791/mcp?token=bad", "a".repeat(64)),
        ("ws://127.0.0.1:35791/mcp", "short".into()),
    ] {
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&lock).unwrap()).unwrap();
        value["endpoint"] = json!(endpoint);
        value["token"] = json!(token);
        std::fs::write(&lock, value.to_string()).unwrap();
        assert!(discover(dir.path(), &root).unwrap().is_empty());
    }
}
#[cfg(unix)]
#[test]
fn discovery_rejects_public_or_redirected_records() {
    let (dir, root, lock) = fixture();
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(discover(dir.path(), &root).unwrap().is_empty());
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600)).unwrap();
    let actual = dir.path().join("outside.json");
    std::fs::rename(&lock, &actual).unwrap();
    std::os::unix::fs::symlink(&actual, &lock).unwrap();
    assert!(discover(dir.path(), &root).unwrap().is_empty());
}
