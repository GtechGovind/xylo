use super::*;
use crate::model::ToolCall;
use xylo_core::ProviderConfig;

fn budget() -> ContextBudget {
    ContextBudget::new(&ProviderConfig {
        model: "unrecognized-custom-model".into(),
        context_tokens: 8192,
        output_tokens: 512,
        ..Default::default()
    })
}

/// Text that tokenizes roughly as written.
fn noise(words: usize) -> String {
    (0..words).map(|i| format!("w{i:x} ")).collect()
}

fn output(words: usize) -> String {
    serde_json::json!({"status":"success","content":noise(words)}).to_string()
}

fn round(history: &mut Vec<Message>, id: &str, name: &str, arguments: &str, result: String) {
    history.push(Message::assistant_with_tool_calls(
        "",
        vec![ToolCall::new(id, name, arguments)],
    ));
    history.push(Message::tool_result(id, result));
}

fn system() -> Vec<Message> {
    vec![Message::system("Policy")]
}

#[test]
fn nothing_is_masked_below_the_threshold() {
    let mut history = vec![Message::user("Look at a.rs")];
    round(
        &mut history,
        "r1",
        "file_read",
        r#"{"path":"a.rs"}"#,
        output(200),
    );
    round(
        &mut history,
        "r2",
        "file_read",
        r#"{"path":"a.rs"}"#,
        output(200),
    );
    assert!(
        budget()
            .mask_plan(&system(), &history, &[], 512, Pressure::Normal)
            .is_none()
    );
}

#[test]
fn superseded_reads_and_aged_outputs_are_masked_in_one_durable_batch() {
    let mut history = vec![Message::user("Fix the parser")];
    round(
        &mut history,
        "read-1",
        "file_read",
        r#"{"path":"src/a.rs"}"#,
        output(400),
    );
    round(
        &mut history,
        "ls-1",
        "files_list",
        r#"{"path":"."}"#,
        output(400),
    );
    round(
        &mut history,
        "test-1",
        "shell_command",
        r#"{"command":"cargo test"}"#,
        serde_json::json!({"exit_code":101,"stdout":noise(400)}).to_string(),
    );
    round(
        &mut history,
        "big-1",
        "search_project",
        r#"{"query":"needle"}"#,
        serde_json::json!({"status":"success","truncated":true,"full_result":"/data/tool-9.txt","summary":noise(400)}).to_string(),
    );
    round(
        &mut history,
        "read-2",
        "file_read",
        r#"{"path":"src/a.rs"}"#,
        output(400),
    );
    for index in 0..4 {
        round(
            &mut history,
            &format!("x-{index}"),
            "git_status",
            "{}",
            output(60),
        );
    }
    let latest = history.len();
    round(
        &mut history,
        "read-3",
        "file_read",
        r#"{"path":"src/b.rs"}"#,
        output(400),
    );
    let original = history.clone();
    let plan = budget()
        .mask_plan(&system(), &history, &[], 512, Pressure::Normal)
        .expect("a batch worth masking");
    plan.apply(&mut history);
    let content = |id: &str| {
        history
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some(id))
            .unwrap()
            .content
            .clone()
    };
    // The earlier read is superseded by the later identical read.
    let superseded = content("read-1");
    assert!(prompts::is_masked_observation(&superseded));
    assert!(superseded.contains("file_read `src/a.rs` [read-1]"));
    assert!(superseded.contains("later read"));
    // Listings age out quickly; a saved full result is pointed to.
    assert!(prompts::is_masked_observation(&content("ls-1")));
    assert!(content("big-1").contains("/data/tool-9.txt"));
    // Failures, the latest read of a file and the newest round stay.
    assert_eq!(content("test-1"), original[6].content);
    assert_eq!(content("read-2"), original[10].content);
    assert_eq!(history[latest + 1].content, original[latest + 1].content);
    // Calls, user text and pairing are untouched.
    for (masked, before) in history.iter().zip(&original) {
        assert_eq!(masked.role, before.role);
        assert_eq!(masked.tool_calls, before.tool_calls);
        assert_eq!(masked.tool_call_id, before.tool_call_id);
        if masked.role != Role::Tool {
            assert_eq!(masked.content, before.content);
        }
    }
    // Stubs are final: the next plan has nothing new to mask.
    assert!(
        budget()
            .mask_plan(&system(), &history, &[], 512, Pressure::Overflow)
            .is_none_or(|plan| plan
                .stubs
                .iter()
                .all(|(index, _)| { !prompts::is_masked_observation(&history[*index].content) }))
    );
}

#[test]
fn reads_of_a_file_changed_later_are_out_of_date() {
    let mut history = vec![Message::user("Edit a.rs")];
    round(
        &mut history,
        "read-1",
        "file_read",
        r#"{"path":"a.rs"}"#,
        output(500),
    );
    round(
        &mut history,
        "edit-1",
        "file_edit",
        r#"{"path":"a.rs"}"#,
        output(10),
    );
    round(&mut history, "ls", "files_list", "{}", output(10));
    let plan = budget()
        .mask_plan(&system(), &history, &[], 512, Pressure::Overflow)
        .unwrap();
    assert_eq!(plan.stubs.len(), 1);
    assert!(plan.stubs[0].1.contains("changed afterwards"));
}

#[test]
fn small_batches_wait_unless_the_provider_overflowed() {
    let mut history = vec![Message::user(noise(2600))];
    round(&mut history, "ls-1", "files_list", "{}", output(300));
    for index in 0..4 {
        round(
            &mut history,
            &format!("s-{index}"),
            "git_status",
            "{}",
            output(10),
        );
    }
    let budget = budget();
    assert!(
        budget
            .mask_plan(&system(), &history, &[], 512, Pressure::Normal)
            .is_none(),
        "one small result is not worth breaking the cache"
    );
    let plan = budget
        .mask_plan(&system(), &history, &[], 512, Pressure::Overflow)
        .unwrap();
    assert_eq!(plan.stubs.len(), 1);
}
