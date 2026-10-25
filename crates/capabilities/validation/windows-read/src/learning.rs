//! The implementation and path tests come from the production source below.
//! These field-only fixtures supply its parent type dependencies; this target
//! does not substitute for or qualify the Store or background learning service.

use serde_json::{Value, json};

struct ActionIntent {
    name: String,
    arguments: Value,
    writes: bool,
}

struct JournalAction {
    intent: ActionIntent,
    state: String,
    result: Option<Value>,
}

struct ProcedureStep {
    tool: String,
    arguments: Value,
    requires_approval: bool,
    verification: String,
}

#[path = "../../../../services/src/learning/sanitize.rs"]
mod sanitize;

#[test]
fn actual_learning_sanitizer_keeps_paths_and_approval_without_external_result_content() {
    let scope = r"C:\Users\private-person\project";
    let mut action = JournalAction {
        intent: ActionIntent {
            name: "file_read".into(),
            arguments: json!({
                "path":r"C:\Users\private-person\project\src\main.rs",
                "api_key":"should-not-survive"
            }),
            writes: false,
        },
        state: "succeeded".into(),
        result: Some(json!({"status":"completed","body":"External result must not be learned"})),
    };
    assert!(sanitize::result_verified(&action));
    let (steps, eligible) = sanitize::build_steps(std::slice::from_ref(&action), scope);
    assert!(eligible);
    assert_eq!(steps[0].tool, "file_read");
    assert_eq!(steps[0].arguments["path"], "src/main.rs");
    assert!(!steps[0].requires_approval);
    assert!(
        steps[0]
            .verification
            .contains("durable successful tool result")
    );
    assert!(!steps[0].arguments.to_string().contains("private-person"));
    assert!(
        !steps[0]
            .arguments
            .to_string()
            .contains("should-not-survive")
    );
    assert!(!steps[0].arguments.to_string().contains("External result"));

    action.intent = ActionIntent {
        name: "run_command".into(),
        arguments: json!({"program":"cargo","args":["test","--offline"]}),
        writes: true,
    };
    action.result = Some(json!({"success":true,"code":0}));
    let (steps, eligible) = sanitize::build_steps(std::slice::from_ref(&action), scope);
    assert!(eligible && steps[0].requires_approval);
    assert!(sanitize::result_verified(&action));
    action.result = Some(json!({"success":false,"code":1}));
    assert!(!sanitize::result_verified(&action));
    assert!(sanitize::terms("Rust project tests").contains(&"rust".into()));
    assert!(sanitize::terms("Authorization: bearer should-not-survive").is_empty());
    assert!(sanitize::ignored_tool("account_refresh"));
    assert!(sanitize::correction_command("Use `cargo test --offline`", scope).is_some());
}
