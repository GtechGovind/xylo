use super::{CallRun, merge_identical, tool_icon};
use crate::icons::AppIcon;
use crate::state::ToolCard;
use uuid::Uuid;

fn call(name: &str, path: &str, success: Option<bool>) -> ToolCard {
    let mut card = ToolCard::new(
        Uuid::new_v4(),
        name.into(),
        serde_json::json!({ "path": path }),
        None,
    );
    card.success = success;
    card.refresh_presentation();
    card
}

#[test]
fn consecutive_identical_finished_calls_share_one_row_with_a_count() {
    let tools = [
        call("file_read", "src/main.rs", Some(true)),
        call("file_read", "src/xylo.rs", Some(true)),
        call("file_read", "src/xylo.rs", Some(true)),
        call("file_read", "src/main.rs", Some(true)),
    ];
    assert_eq!(
        merge_identical(&tools, &[0, 1, 2, 3]),
        vec![
            CallRun {
                first: 0,
                last: 0,
                count: 1,
                members: vec![0]
            },
            CallRun {
                first: 1,
                last: 2,
                count: 2,
                members: vec![1, 2]
            },
            CallRun {
                first: 3,
                last: 3,
                count: 1,
                members: vec![3]
            },
        ]
    );
}

#[test]
fn running_and_failed_calls_keep_their_own_rows() {
    let tools = [
        call("file_read", "a.rs", Some(true)),
        call("file_read", "a.rs", None),
        call("file_read", "a.rs", Some(false)),
        call("file_read", "a.rs", Some(false)),
    ];
    assert_eq!(merge_identical(&tools, &[0, 1, 2, 3]).len(), 4);
    let different_tools = [
        call("file_read", "a.rs", Some(true)),
        call("files_list", "a.rs", Some(true)),
    ];
    assert_eq!(merge_identical(&different_tools, &[0, 1]).len(), 2);
}

#[test]
fn each_kind_of_call_has_a_semantic_icon() {
    assert_eq!(tool_icon("file_read"), AppIcon::ToolRead);
    assert_eq!(tool_icon("file_edit"), AppIcon::ToolEdit);
    assert_eq!(tool_icon("shell_command"), AppIcon::Terminal);
    assert_eq!(tool_icon("search_project"), AppIcon::Search);
    assert_eq!(tool_icon("agent_delegate"), AppIcon::Agent);
    assert_eq!(tool_icon("something_new"), AppIcon::Tool);
}

#[test]
fn grouped_calls_keep_distinct_ranges_and_noncontiguous_members() {
    let mut first = call("file_read", "a.rs", Some(true));
    first.input["start_line"] = serde_json::json!(1);
    let mut last = call("file_read", "a.rs", Some(true));
    last.input["start_line"] = serde_json::json!(200);
    let tools = [first, call("file_read", "helper.rs", Some(true)), last];
    let runs = merge_identical(&tools, &[0, 2]);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].members, vec![0, 2]);
    assert_eq!(tools[runs[0].members[0]].input["start_line"], 1);
    assert_eq!(tools[runs[0].members[1]].input["start_line"], 200);
}
