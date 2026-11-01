//! Devcontainer discovery stays inert and constrains host paths.
use super::*;
#[test]
fn devcontainer_offer_reads_jsonc_image_folder_and_post_create() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join(".devcontainer")).unwrap();
    std::fs::write(temp.path().join(".devcontainer/devcontainer.json"), "{ // comment\n\"image\":\"ubuntu:24.04\",\"workspaceFolder\":\"/work\",\"postCreateCommand\":[\"echo\",\"ready\"],}").unwrap();
    let offer = discover(temp.path()).unwrap().unwrap();
    assert_eq!(offer.workspace_folder, "/work");
    assert_eq!(offer.post_create, ["echo", "ready"]);
    assert_eq!(offer.image.as_deref(), Some("ubuntu:24.04"));
}
#[test]
fn devcontainer_offer_rejects_outside_dockerfile_and_unsafe_options() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("repo");
    std::fs::create_dir_all(workspace.join(".devcontainer")).unwrap();
    std::fs::write(temp.path().join("Dockerfile"), "FROM ubuntu").unwrap();
    let source = workspace.join(".devcontainer/devcontainer.json");
    std::fs::write(&source, r#"{"build":{"dockerfile":"../../Dockerfile"}}"#).unwrap();
    assert!(discover(&workspace).is_err());
    std::fs::write(&source, r#"{"image":"ubuntu","privileged":true}"#).unwrap();
    assert!(discover(&workspace).is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn devcontainer_setup_runs_reviewed_commands_and_does_not_replay() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let workspace = base.join("repo");
    std::fs::create_dir_all(workspace.join(".devcontainer")).unwrap();
    std::fs::write(workspace.join(".devcontainer/devcontainer.json"),r#"{"image":"fixture:image","workspaceFolder":"/work","postCreateCommand":["echo","ready"]}"#).unwrap();
    let executable = base.join("docker");
    std::fs::write(&executable, format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nif [ \"$1\" = run ]; then printf '%s\\n' '{}'; fi\n",base.join("calls").display(), "a".repeat(64))).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    let config = CapabilityConfig {
        workspace: workspace.clone(),
        artifacts: base.join("artifacts"),
        management_root: Some(base.join("management")),
        ..Default::default()
    };
    let manager = Manager::open(&config, &Default::default()).await.unwrap();
    let offer = discover(&workspace).unwrap().unwrap();
    let request = json!({"operation_id":Uuid::new_v4(),"offer":offer,"executable":executable,"runtime":"docker"});
    assert!(
        create(&manager, &base, &request, Default::default())
            .await
            .is_err()
    );
    assert!(!base.join("calls").exists());
    let result = create(&manager, &workspace, &request, Default::default())
        .await
        .unwrap();
    assert_eq!(result["environments"].as_array().unwrap().len(), 1);
    let calls = std::fs::read_to_string(base.join("calls")).unwrap();
    assert!(calls.contains("--mount type=bind,source="));
    assert!(calls.contains("target=/work --workdir /work"));
    assert!(calls.contains("exec --workdir /work"));
    assert!(calls.contains("echo ready"));
    assert!(
        create(&manager, &workspace, &request, Default::default())
            .await
            .is_err()
    );
    assert_eq!(std::fs::read_to_string(base.join("calls")).unwrap(), calls);
}

#[test]
fn dockerfile_context_defaults_to_configuration_directory() {
    let temp = tempfile::tempdir().unwrap();
    let folder = temp.path().join(".devcontainer");
    std::fs::create_dir(&folder).unwrap();
    std::fs::write(folder.join("Dockerfile"), "FROM fixture:image").unwrap();
    let source = folder.join("devcontainer.json");
    std::fs::write(&source, r#"{"build":{"dockerfile":"Dockerfile"}}"#).unwrap();
    assert_eq!(
        discover(temp.path()).unwrap().unwrap().context,
        folder.canonicalize().unwrap()
    );
    std::fs::write(
        &source,
        r#"{"build":{"dockerfile":"Dockerfile","context":".."}}"#,
    )
    .unwrap();
    assert_eq!(
        discover(temp.path()).unwrap().unwrap().context,
        temp.path().canonicalize().unwrap()
    );
    std::fs::write(
        &source,
        r#"{"build":{"dockerfile":"Dockerfile","args":{"MODE":"development"}}}"#,
    )
    .unwrap();
    assert!(
        discover(temp.path())
            .err()
            .unwrap()
            .to_string()
            .contains("manual setup")
    );
}
