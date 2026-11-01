//! Project server identities and authority bindings.
use super::*;
#[test]
fn project_discovery_is_inert_and_changed_content_has_new_authority() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(".mcp.json");
    std::fs::write(
        &path,
        r#"{"mcpServers":{"remote":{"type":"sse","url":"https://example.com/sse"}}}"#,
    )
    .unwrap();
    let first = project_connectors(temp.path()).unwrap().remove(0);
    assert_eq!(first.config.transport, "sse");
    assert!(!first.config.trusted);
    std::fs::write(
        &path,
        r#"{"mcpServers":{"remote":{"type":"http","url":"https://other.example.com/mcp"}}}"#,
    )
    .unwrap();
    let second = project_connectors(temp.path()).unwrap().remove(0);
    assert_eq!(first.config.id, second.config.id);
    assert_ne!(first.approval_key(), second.approval_key());
}

#[test]
fn project_discovery_prompt_slugs_match_engine_connector_names() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join(".mcp.json"),
        r#"{"mcpServers":{"  Docs--Service  ":{"url":"https://example.com/mcp"}}}"#,
    )
    .unwrap();
    let entry = project_connectors(temp.path()).unwrap().remove(0);
    let commands =
        mcp_prompt_commands(&entry.config, &serde_json::json!([{"name":"review--code"}]));
    assert_eq!(commands[0].command, "/mcp__docs_service__review_code");
}
