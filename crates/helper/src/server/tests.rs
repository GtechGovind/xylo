use super::*;
#[test]
fn unreadable_configuration_is_deleted_and_defaults_apply() {
    let data = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let path = data.path().join("capabilities.json");
    std::fs::write(&path, br#"{"max_output_bytes":"many"}"#).unwrap();
    let loaded = load_configuration(data.path(), workspace.path()).unwrap();
    assert!(!path.exists());
    assert_eq!(
        loaded.max_output_bytes,
        CapabilityConfig::default().max_output_bytes
    );
}
#[test]
fn configuration_cannot_replace_authorized_roots_and_save_is_atomic() {
    let data = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let config = CapabilityConfig {
        workspace: PathBuf::from("/untrusted"),
        artifacts: PathBuf::from("/untrusted"),
        management_root: Some(PathBuf::from("/untrusted/management")),
        ..Default::default()
    };
    save_configuration(data.path(), &config).unwrap();
    let loaded = load_configuration(data.path(), workspace.path()).unwrap();
    assert_eq!(loaded.workspace, workspace.path());
    assert_eq!(loaded.artifacts, data.path().join("artifacts"));
    assert_eq!(
        loaded.management_root,
        Some(data.path().join("native-management"))
    );
    assert!(std::fs::read_dir(data.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")
    }));
}

#[test]
fn configuration_patch_cannot_redirect_management_storage() {
    let data = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let redirected = workspace.path().join("redirected");
    replace_configuration(
        data.path(),
        workspace.path(),
        Some((json!({"management_root": redirected}), true)),
    )
    .unwrap();
    let loaded = load_configuration(data.path(), workspace.path()).unwrap();
    assert_eq!(
        loaded.management_root,
        Some(data.path().join("native-management"))
    );
    assert!(!redirected.exists());
    assert!(data.path().join("native-management").is_dir());
}
