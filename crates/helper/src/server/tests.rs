use super::*;

#[test]
fn legacy_chromium_file_survives_loading_and_save_drops_only_obsolete_setting() {
    let data = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let path = data.path().join("capabilities.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({
            "chromium": "/obsolete/chrome",
            "pdfium_library": "/assets/pdfium",
            "helper_idle_seconds": 79,
            "operation_timeout_seconds": 321,
            "max_output_bytes": 16384
        }))
        .unwrap(),
    )
    .unwrap();
    let loaded = load_configuration(data.path(), workspace.path()).unwrap();
    assert!(
        path.exists(),
        "legacy settings must not trigger the corrupt-file reset"
    );
    assert!(!loaded.browser_allow_private_network);
    assert_eq!(loaded.helper_idle_seconds, 79);
    assert_eq!(loaded.operation_timeout_seconds, 321);
    assert_eq!(loaded.max_output_bytes, 16384);
    assert_eq!(loaded.pdfium_library, Some(PathBuf::from("/assets/pdfium")));
    save_configuration(data.path(), &loaded).unwrap();
    let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(saved.get("chromium").is_none());
    assert_eq!(saved["browser_allow_private_network"], false);
    assert_eq!(saved["helper_idle_seconds"], 79);
    assert_eq!(saved["operation_timeout_seconds"], 321);
    assert_eq!(saved["max_output_bytes"], 16384);
    assert_eq!(saved["pdfium_library"], "/assets/pdfium");
}

#[test]
fn browser_path_and_url_controls_are_rejected_for_patch_and_replace() {
    let data = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    for key in ["chromium", "obscura", "browser_url"] {
        for patch in [true, false] {
            let mut value = json!({});
            value[key] = json!("untrusted");
            let error = replace_configuration(data.path(), workspace.path(), Some((value, patch)))
                .err()
                .expect("custom browser settings must be refused");
            let message = error.to_string();
            assert!(message.contains(key), "{message}");
            assert!(message.contains("bundled xylo-obscura"), "{message}");
            assert!(!data.path().join("capabilities.json").exists());
        }
    }
}

#[test]
fn private_network_patch_is_explicit_and_preserves_other_settings() {
    let data = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let config = CapabilityConfig {
        helper_idle_seconds: 81,
        ..Default::default()
    };
    save_configuration(data.path(), &config).unwrap();
    replace_configuration(
        data.path(),
        workspace.path(),
        Some((json!({"browser_allow_private_network": true}), true)),
    )
    .unwrap();
    let loaded = load_configuration(data.path(), workspace.path()).unwrap();
    assert!(loaded.browser_allow_private_network);
    assert_eq!(loaded.helper_idle_seconds, 81);
}

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
