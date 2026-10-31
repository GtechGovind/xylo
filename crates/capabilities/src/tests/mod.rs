use super::*;
#[test]
fn output_names_cannot_escape_artifacts() {
    let cfg = CapabilityConfig::default();
    assert!(output_path(&cfg, "../escape", "txt").is_err());
    assert!(output_path(&cfg, "foo/bar", "txt").is_err());
}
#[test]
fn only_saved_environments_execute_remote_commands() {
    let root = tempfile::tempdir().unwrap();
    let service = CapabilityService::new(CapabilityConfig {
        workspace: root.path().canonicalize().unwrap(),
        artifacts: root.path().join("artifacts"),
        ..Default::default()
    })
    .unwrap();
    let tools = service.tool_definitions();
    assert!(tools.iter().all(|tool| tool.name != "remote_execute"));
    assert!(tools.iter().any(|tool| tool.name == "environment_execute"));
}

/// Provider tool APIs (the OpenAI Responses API in particular) reject
/// arrays without `items` and do not resolve JSON Schema references.
#[test]
fn every_tool_schema_is_self_contained_and_types_its_arrays() {
    fn check(tool: &str, path: &str, value: &Value) {
        match value {
            Value::Object(object) => {
                for key in ["$schema", "$ref", "$defs", "definitions"] {
                    assert!(
                        !object.contains_key(key),
                        "{tool}: {path} uses unsupported {key}"
                    );
                }
                let is_array = match object.get("type") {
                    Some(Value::String(kind)) => kind == "array",
                    Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind == "array"),
                    _ => false,
                };
                assert!(
                    !is_array || object.contains_key("items"),
                    "{tool}: array at {path} has no items"
                );
                for (key, child) in object {
                    check(tool, &format!("{path}/{key}"), child);
                }
            }
            Value::Array(items) => {
                for (index, child) in items.iter().enumerate() {
                    check(tool, &format!("{path}/{index}"), child);
                }
            }
            _ => {}
        }
    }
    let root = tempfile::tempdir().unwrap();
    let service = CapabilityService::new(CapabilityConfig {
        workspace: root.path().canonicalize().unwrap(),
        artifacts: root.path().join("artifacts"),
        ..Default::default()
    })
    .unwrap();
    let tools = service.tool_definitions();
    assert!(!tools.is_empty());
    for tool in tools {
        check(&tool.name, "", &tool.parameters);
    }
}

#[test]
fn configuration_does_not_claim_availability() {
    assert_eq!(asset_status(None), "configuration_needed");
    assert_eq!(
        asset_status(Some(Path::new("/nonexistent-test-asset"))),
        "asset_missing"
    );
}

#[test]
fn private_management_files_are_not_part_of_the_plugin_input_scope() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().canonicalize().unwrap();
    let private = workspace.join("native-management");
    std::fs::create_dir(&private).unwrap();
    std::fs::write(private.join("catalog.json"), "{}").unwrap();
    std::fs::write(workspace.join("notes.txt"), "Project notes").unwrap();
    let service = CapabilityService::new(CapabilityConfig {
        workspace: workspace.clone(),
        artifacts: workspace.join("artifacts"),
        management_root: Some(private),
        ..Default::default()
    })
    .unwrap();
    assert!(open_input(&service.config, "notes.txt").is_ok());
    assert!(open_input(&service.config, "native-management/catalog.json").is_err());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(
            workspace.join("native-management/catalog.json"),
            workspace.join("alias.json"),
        )
        .unwrap();
        assert!(open_input(&service.config, "alias.json").is_err());
    }
}

#[test]
fn management_storage_cannot_overlap_the_artifact_root() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().canonicalize().unwrap();
    for private in [workspace.clone(), workspace.join("artifacts/private")] {
        assert!(
            CapabilityService::new(CapabilityConfig {
                workspace: workspace.clone(),
                artifacts: workspace.join("artifacts"),
                management_root: Some(private),
                ..Default::default()
            })
            .is_err()
        );
    }
    #[cfg(unix)]
    {
        let other = tempfile::tempdir().unwrap();
        let link = workspace.join("linked-management");
        std::os::unix::fs::symlink(other.path(), &link).unwrap();
        assert!(
            CapabilityService::new(CapabilityConfig {
                workspace: workspace.clone(),
                artifacts: workspace.join("artifacts"),
                management_root: Some(link),
                ..Default::default()
            })
            .is_err()
        );
    }
}

#[tokio::test]
async fn ordinary_tools_cannot_dispatch_application_management_controls() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().canonicalize().unwrap();
    let service = CapabilityService::new(CapabilityConfig {
        workspace: workspace.clone(),
        artifacts: workspace.join("artifacts"),
        management_root: Some(workspace.join("native-management")),
        ..Default::default()
    })
    .unwrap();
    for name in ["helper_plugin_install", "helper_environment_save"] {
        assert!(
            service
                .tool_definitions()
                .iter()
                .all(|tool| tool.name != name)
        );
        let error = service
            .execute(name, &json!({}), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("Unknown capability tool"),
            "Management control {name} was rejected at the wrong boundary: {error:#}"
        );
    }
}

#[tokio::test]
async fn a_helper_shutdown_never_needs_the_model_activity_lease() {
    let root = tempfile::tempdir().unwrap();
    let config = CapabilityConfig {
        workspace: root.path().canonicalize().unwrap(),
        artifacts: root.path().join("artifacts"),
        management_root: Some(root.path().join("management")),
        ..Default::default()
    };
    let parent = CapabilityService::new(config.clone()).unwrap();
    let helper = CapabilityService::new(config).unwrap();
    // Another helper (or the parent) is in the middle of a configuration
    // change and holds the exclusive lease.
    let _held = parent.models.idle_activity_lease().unwrap();
    assert!(
        helper.shutdown().await.is_err(),
        "a full shutdown contends for the lease"
    );
    helper.shutdown_helper_sessions().await.unwrap();
}
