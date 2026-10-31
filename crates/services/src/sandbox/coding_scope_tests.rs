use super::*;
#[test]
fn public_cache_view_excludes_private_registry_config_and_credentials() {
    let directory = tempfile::tempdir().unwrap();
    let cargo = directory.path().join("host-cargo");
    let isolated = directory.path().join("isolated");
    std::fs::create_dir_all(&isolated).unwrap();
    let name = "index.crates.io-fixture";
    for kind in ["index", "cache", "src"] {
        std::fs::create_dir_all(cargo.join("registry").join(kind).join(name)).unwrap();
    }
    std::fs::create_dir(cargo.join("registry/index").join(name).join(".cache")).unwrap();
    std::fs::write(cargo.join("credentials.toml"), "private-token").unwrap();
    std::fs::write(cargo.join("config.toml"), "private-config").unwrap();
    let config = cargo.join("registry/index").join(name).join("config.json");
    std::fs::write(
        &config,
        r#"{"dl":"https://private.example","auth-required":true}"#,
    )
    .unwrap();
    let mut roots = Vec::new();
    public_cargo_cache_view(&cargo, &isolated, &mut roots).unwrap();
    assert!(roots.is_empty());
    std::fs::write(
        &config,
        r#"{"dl":"https://static.crates.io/crates","api":"https://crates.io","unused-token":"not-for-child"}"#,
    )
    .unwrap();
    public_cargo_cache_view(&cargo, &isolated, &mut roots).unwrap();
    assert_eq!(roots.len(), 3);
    assert!(!isolated.join("credentials.toml").exists());
    assert!(!isolated.join("config.toml").exists());
    assert!(
        !std::fs::read_to_string(
            isolated
                .join("registry/index")
                .join(name)
                .join("config.json")
        )
        .unwrap()
        .contains("not-for-child")
    );
    assert!(
        roots
            .iter()
            .all(|path| path.starts_with(cargo.join("registry").canonicalize().unwrap()))
    );
    #[cfg(unix)]
    assert!(
        std::fs::symlink_metadata(isolated.join("registry/index").join(name).join(".cache"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}
#[test]
fn plugin_scope_has_no_implicit_cargo_home_or_toolchain_grants() {
    let directory = tempfile::tempdir().unwrap();
    let prepared = prepare("/bin/echo", &["scope".into()], directory.path(), &[], &[]).unwrap();
    let spec = prepared
        .command
        .as_std()
        .get_args()
        .nth(1)
        .unwrap()
        .to_str()
        .unwrap();
    let spec: serde_json::Value = serde_json::from_str(spec).unwrap();
    assert!(spec["toolchain_read_roots"].as_array().unwrap().is_empty());
    assert!(spec["coding_environment"].as_object().unwrap().is_empty());
}
