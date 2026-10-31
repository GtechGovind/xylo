use super::*;
#[test]
fn language_server_activation_preserves_other_overrides() {
    let root = tempfile::tempdir().unwrap();
    let manager = AssetManager::new(root.path()).unwrap();
    let path = root.path().join("language-servers.json");
    write_json(&path,&json!({"metadata":"retained","servers":[{"languages":["rust","typescript"],"executable":"/existing/multi"},{"languages":["cpp"],"executable":"/existing/clangd"}]})).unwrap();
    let server = json!({"languages":["rust"],"executable":"/owned/rust-analyzer","args":[]});
    manager.save_language_server_configuration(&server).unwrap();
    let config: Value = read_json(&path, 65536).unwrap();
    assert_eq!(config["metadata"], "retained");
    assert_eq!(config["servers"][0]["languages"], json!(["typescript"]));
    assert_eq!(config["servers"][1]["executable"], "/existing/clangd");
    assert_eq!(config["servers"][2], server);
}
