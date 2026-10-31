use super::{ToolBody, ToolPresentation};
use serde_json::json;
fn prepare(name: &str, input: Value, summary: Value) -> ToolPresentation {
    ToolPresentation::prepare(
        name,
        &input,
        Some(&json!({"status":"success","summary":summary}).to_string()),
        Some(true),
    )
}
use serde_json::Value;
#[test]
fn portable_tools_show_skill_and_component_names_without_raw_envelopes() {
    let skill = prepare(
        "extension_skill",
        json!({"id":"extension-abcd:code-review"}),
        json!({"skill":"code-review","instructions":"# Code review\nReview the diff.","description":"Reviews the current changes"}),
    );
    assert_eq!(skill.verb, "Using skill");
    assert_eq!(skill.target, "Code review");
    assert_eq!(
        skill.tooltip.as_deref(),
        Some("Reviews the current changes")
    );
    assert!(
        !skill.expandable(),
        "A skill's instructions are for the model, not an expandable body"
    );
    let resource = prepare(
        "extension_resource",
        json!({"package":"code-review","path":"references/format.md"}),
        json!({"content":"# Format"}),
    );
    assert_eq!(resource.verb, "Read skill file");
    assert_eq!(resource.target, "references/format.md");
    assert!(!resource.expandable());
    let component = prepare(
        "extension_component",
        json!({"component":"mcpServers","operation":"call"}),
        json!({"results":[{"entry":"search","output":{"content":[{"type":"text","text":"Three matches found"}]}}]}),
    );
    assert_eq!(component.verb, "Run plugin");
    assert_eq!(component.meta.as_deref(), Some("call"));
    assert!(
        matches!(component.body, ToolBody::Message(ref text) if text == "search: Three matches found")
    );
    let hook = prepare(
        "extension_component",
        json!({"component":"hooks","operation":"event"}),
        json!({"results":[{"entry":"After tools","output":{"stdout":"Checks passed","stderr":""}}]}),
    );
    assert!(matches!(hook.body, ToolBody::Message(ref text) if text.contains("Checks passed")));
    let lsp = prepare(
        "extension_component",
        json!({"component":"lspServers","operation":"request"}),
        json!({"results":[{"entry":"rust","output":{"result":{"contents":{"kind":"markdown","value":"A workspace value"}}}}]}),
    );
    assert!(matches!(lsp.body, ToolBody::Message(ref text) if text.contains("A workspace value")));
}
#[test]
fn directory_listing_is_names_and_types_without_envelopes() {
    let shown = prepare(
        "files_list",
        json!({"path":"crates"}),
        json!({"entries":["core/","Cargo.toml"]}),
    );
    assert_eq!(shown.verb, "List");
    assert_eq!(shown.target, "crates");
    let ToolBody::Files { entries, remaining } = shown.body else {
        panic!("Expected semantic file rows")
    };
    assert!(entries[0].directory);
    assert_eq!(entries[0].name, "core");
    assert_eq!(remaining, 0);
}
#[test]
fn read_preserves_real_line_numbers_and_bounds_large_unicode_content() {
    let content = (700..1700)
        .map(|line| format!("{line:>6}\tlet café = {line};\n"))
        .collect::<String>();
    let shown = prepare(
        "file_read",
        json!({"path":"src/main.rs"}),
        json!({"path":"src/main.rs","start_line":700,"end_line":1699,"total_lines":2000,"content":content}),
    );
    assert_eq!(shown.meta.as_deref(), Some("700–1699 · 2000 lines"));
    let ToolBody::Excerpt { lines, remaining } = shown.body else {
        panic!("Expected bounded excerpt")
    };
    assert_eq!(lines[0].number, Some(700));
    assert_eq!(lines[0].text, "let café = 700;");
    assert!(lines.len() <= 24);
    assert_eq!(remaining, 976);
}
#[test]
fn search_groups_matches_by_file_and_keeps_colons_in_content() {
    let shown = prepare(
        "search_project",
        json!({"query":"fn"}),
        json!({"matches":3,"files":2,"results":"src/a.rs:12:fn a() { x: 1 }\nsrc/b.rs:9:fn b() {}\nsrc/a.rs:20:fn c() {}\n"}),
    );
    let ToolBody::Search { groups, remaining } = shown.body else {
        panic!("Expected grouped matches")
    };
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].matches.len(), 2);
    assert_eq!(groups[0].matches[0].number, Some(12));
    assert!(groups[0].matches[0].text.contains("x: 1"));
    assert_eq!(remaining, 0);
}
#[test]
fn commands_show_actual_exit_and_output_while_errors_are_readable() {
    let shown = prepare(
        "run_command",
        json!({"program":"cargo","args":["check"]}),
        json!({"exit_code":1,"stdout":"Checking xylo","stderr":"error: missing import"}),
    );
    let ToolBody::Command {
        command,
        exit,
        stderr,
        ..
    } = shown.body
    else {
        panic!("Expected command output")
    };
    assert_eq!(command, "cargo check");
    assert_eq!(exit, Some(1));
    assert_eq!(stderr[0].text, "error: missing import");
    let error = ToolPresentation::prepare(
        "files_list",
        &json!({"path":"missing"}),
        Some("{\"status\":\"failed\",\"error\":\"Directory does not exist\"}"),
        Some(false),
    );
    assert_eq!(error.error.as_deref(), Some("Directory does not exist"));
    assert!(!error.error.unwrap().starts_with('{'));
}
#[test]
fn delegate_result_is_readable_markdown_and_unknown_json_never_renders() {
    let delegate = prepare(
        "agent_delegate",
        json!({"task":"Review navigation"}),
        json!({"response":"## Findings\nUse a sidebar."}),
    );
    assert!(
        matches!(delegate.body,ToolBody::Markdown(ref text) if text.starts_with("## Findings"))
    );
    let unknown = prepare(
        "custom_tool",
        json!({"internal":"argument"}),
        json!({"opaque":{"protocol":true}}),
    );
    assert!(matches!(unknown.body, ToolBody::Message(ref text) if text == "Done"));
    let chatty = prepare(
        "custom_tool",
        json!({}),
        json!({"message":"one\ntwo\nthree\nfour\nfive\nsix\nseven"}),
    );
    assert!(
        matches!(chatty.body, ToolBody::Message(ref text) if text.starts_with("one") && !text.contains("seven"))
    );
}
#[test]
fn search_context_stays_with_its_file_and_pending_tools_have_no_fake_counts() {
    let shown = prepare(
        "search_project",
        json!({"query":"main","context":1}),
        json!({"matches":1,"files":1,"results":"src/a-b.rs-10-before\nsrc/a-b.rs:11:main\nsrc/a-b.rs-12-after\n--\n"}),
    );
    let ToolBody::Search { groups, .. } = shown.body else {
        panic!("Expected grouped context")
    };
    assert_eq!(groups.len(), 1);
    assert_eq!(
        groups[0]
            .matches
            .iter()
            .map(|line| line.number)
            .collect::<Vec<_>>(),
        vec![Some(10), Some(11), Some(12)]
    );
    let pending = ToolPresentation::prepare("files_list", &json!({"path":"src"}), None, None);
    assert!(pending.meta.is_none());
}
#[test]
fn a_multi_line_command_row_shows_its_first_line_and_the_detail_keeps_all_of_it() {
    let script = "python3 -c from pathlib import Path\np=Path('crates/desktop/src')\nfiles…";
    let shown = prepare(
        "shell_command",
        json!({ "command": script }),
        json!({"exit_code":0,"stdout":"","stderr":""}),
    );
    assert_eq!(shown.target, "python3 -c from pathlib import Path …");
    assert!(!shown.target.contains('\n'));
    let ToolBody::Command { command, .. } = shown.body else {
        panic!("Expected command output")
    };
    assert_eq!(command, script);
    let single = prepare(
        "shell_command",
        json!({ "command": "cargo test" }),
        json!({"exit_code":0}),
    );
    assert_eq!(single.target, "cargo test");
}
#[test]
fn every_row_target_is_one_line() {
    let search = prepare(
        "search_project",
        json!({ "query": "fn spawn\n  | struct HostClient\t|\r\nimpl HostClient" }),
        json!({"matches":0,"files":0,"results":""}),
    );
    assert_eq!(
        search.target,
        "fn spawn | struct HostClient | impl HostClient"
    );
    let task = prepare(
        "agent_delegate",
        json!({ "task": "Review navigation\n\nThen report" }),
        json!({"result":"done"}),
    );
    assert_eq!(task.target, "Review navigation Then report");
    assert_eq!(super::one_line("  a \n b  "), "a b");
}
