use super::*;
use parking_lot::Mutex;
use serde_json::json;

/// An executor with real-looking schemas that records what it ran.
struct Fake {
    tools: Vec<(&'static str, Value, Access)>,
    connector: Option<Vec<(String, Arc<CompiledSchema>)>>,
    ran: Mutex<Vec<(String, Value)>>,
}

impl Fake {
    fn new() -> Self {
        let object = |properties: Value, required: &[&str]| json!({"type":"object","properties":properties,"required":required,"additionalProperties":false});
        Self {
            tools: vec![
                (
                    "file_read",
                    object(
                        json!({"path":{"type":"string"},"offset":{"type":"integer","minimum":1},"limit":{"type":"integer"}}),
                        &["path"],
                    ),
                    Access::ReadOnly,
                ),
                (
                    "file_edit",
                    object(
                        json!({"path":{"type":"string"},"old_string":{"type":"string"},"new_string":{"type":"string"},"replace_all":{"type":"boolean"}}),
                        &["path", "old_string", "new_string"],
                    ),
                    Access::Writes,
                ),
                (
                    "todo_write",
                    object(
                        json!({"todos":{"type":"array","items":{"type":"object","properties":{"content":{"type":"string"},"status":{"type":"string","enum":["pending","in_progress","completed"]}},"required":["content","status"],"additionalProperties":false}}}),
                        &["todos"],
                    ),
                    Access::Writes,
                ),
                (
                    "memory_search",
                    object(
                        json!({"query":{"type":"string"},"scope":{"type":"string","default":"project"}}),
                        &["query", "scope"],
                    ),
                    Access::ReadOnly,
                ),
                (
                    "call_tool",
                    object(
                        json!({"name":{"type":"string"},"arguments":{"type":"object"}}),
                        &["name", "arguments"],
                    ),
                    Access::Writes,
                ),
                (
                    "connector_call",
                    object(
                        json!({"connector_id":{"type":"string"},"tool":{"type":"string"},"arguments":{"type":"object"}}),
                        &["connector_id", "tool", "arguments"],
                    ),
                    Access::Writes,
                ),
            ],
            connector: Some(vec![(
                "create_issue".into(),
                Arc::new(CompiledSchema::new(
                    json!({"type":"object","properties":{"title":{"type":"string"},"labels":{"type":"array","items":{"type":"string"}}},"required":["title"]}),
                )),
            )]),
            ran: Mutex::new(Vec::new()),
        }
    }
}

impl ToolExecutor for Fake {
    fn schemas(&self) -> Vec<ToolSchema> {
        self.tools
            .iter()
            .map(|(name, parameters, _)| ToolSchema {
                name: (*name).into(),
                description: "fixture".into(),
                parameters: parameters.clone(),
            })
            .collect()
    }
    fn deferred_count(&self) -> usize {
        0
    }
    fn access(&self, name: &str, _: &Value) -> Option<Access> {
        self.tools
            .iter()
            .find(|(tool, _, _)| *tool == name)
            .map(|(_, _, access)| *access)
    }
    fn execute(&self, _: &str, name: &str, arguments: Value) -> anyhow::Result<String> {
        self.ran.lock().push((name.into(), arguments));
        Ok("ok".into())
    }
    fn parameters(&self, name: &str) -> Option<Arc<CompiledSchema>> {
        self.tools
            .iter()
            .find(|(tool, _, _)| *tool == name)
            .map(|(_, parameters, _)| Arc::new(CompiledSchema::new(parameters.clone())))
    }
    fn connector_tools(&self, _: &Value) -> Option<Vec<(String, Arc<CompiledSchema>)>> {
        self.connector.clone()
    }
}

fn prepared(name: &str, arguments: &str) -> Prepared {
    prepare(&ToolCall::new("c1", name, arguments), &Fake::new())
}

fn ran(prepared: Prepared) -> (String, Value) {
    match prepared {
        Prepared::Run {
            name, arguments, ..
        } => (name, arguments),
        Prepared::Reject(error) => panic!("rejected: {error}"),
    }
}

fn rejected(prepared: Prepared) -> String {
    match prepared {
        Prepared::Reject(error) => serde_json::from_str::<Value>(&error).unwrap()["summary"]
            .as_str()
            .unwrap()
            .to_owned(),
        Prepared::Run { arguments, .. } => panic!("ran with {arguments}"),
    }
}

#[test]
fn lenient_parsing_recovers_common_wrappings_of_a_json_object() {
    let cases = [
        ("", json!({})),
        ("null", json!({})),
        ("```json\n{\"path\":\"a\"}\n```", json!({"path":"a"})),
        (
            "Here is the call: {\"path\":\"a\"} done",
            json!({"path":"a"}),
        ),
        ("{\"path\":\"a\",}", json!({"path":"a"})),
        ("{'path': 'it\\'s \"a\"'}", json!({"path":"it's \"a\""})),
        (
            "{path: \"a\", flag: True, other: None}",
            json!({"path":"a","flag":true,"other":null}),
        ),
        ("\"{\\\"path\\\":\\\"a\\\"}\"", json!({"path":"a"})),
        (
            "{\"text\":\"keep 'quotes', commas,}\"}",
            json!({"text":"keep 'quotes', commas,}"}),
        ),
    ];
    for (raw, expected) in cases {
        assert_eq!(arguments::parse(raw).unwrap(), expected, "{raw}");
    }
    assert!(
        arguments::parse("{not json")
            .unwrap_err()
            .contains("not valid JSON")
    );
    assert!(
        arguments::parse("[1]")
            .unwrap_err()
            .contains("must be a JSON object")
    );
}

#[test]
fn unambiguous_type_slips_are_repaired_before_dispatch() {
    let (name, arguments) = ran(prepared(
        "file_read",
        r#"{"path":"a.rs","offset":"5","limit":20.0}"#,
    ));
    assert_eq!(name, "file_read");
    assert_eq!(arguments, json!({"path":"a.rs","offset":5,"limit":20}));
    let (_, arguments) = ran(prepared(
        "file_edit",
        r#"{"path":"a","oldString":"x","new_string":12,"replace_all":"TRUE"}"#,
    ));
    assert_eq!(
        arguments,
        json!({"path":"a","old_string":"x","new_string":"12","replace_all":true})
    );
}

#[test]
fn stringified_json_enum_case_nulls_wrappers_and_defaults_are_repaired() {
    let (_, arguments) = ran(prepared(
        "todo_write",
        r#"{"todos":"[{\"content\":\"a\",\"status\":\"In-Progress\"}]"}"#,
    ));
    assert_eq!(
        arguments,
        json!({"todos":[{"content":"a","status":"in_progress"}]})
    );
    let (_, arguments) = ran(prepared(
        "file_read",
        r#"{"arguments":{"path":"a","limit":null}}"#,
    ));
    assert_eq!(arguments, json!({"path":"a"}));
    let (_, arguments) = ran(prepared("memory_search", r#"{"query":"q"}"#));
    assert_eq!(arguments, json!({"query":"q","scope":"project"}));
}

#[test]
fn repair_is_idempotent_so_resumed_calls_match_the_journal() {
    let fake = Fake::new();
    let raw = r#"{'todos': '[{"content":"a","status":"COMPLETED"}]',}"#;
    let first = ran(prepare(&ToolCall::new("c", "todo_write", raw), &fake)).1;
    let again = ran(prepare(
        &ToolCall::new("c", "todo_write", first.to_string()),
        &fake,
    ))
    .1;
    assert_eq!(first, again);
}

#[test]
fn schema_violations_name_the_field_and_what_was_expected() {
    let error = rejected(prepared("file_read", r#"{"path":"a","offset":"soon"}"#));
    assert!(error.contains("`offset`"), "{error}");
    assert!(error.contains("integer"), "{error}");
    assert!(error.contains("Expected arguments: "), "{error}");
    assert!(error.contains("path: string (required)"), "{error}");
    let error = rejected(prepared("file_read", r#"{"paht":"a"}"#));
    assert!(error.contains("unknown field `paht`"), "{error}");
    assert!(error.contains("did you mean `path`"), "{error}");
    assert!(error.contains("\"path\" is a required property"), "{error}");
    let error = rejected(prepared("file_read", r#"{"path":"a","offset":0}"#));
    assert!(
        error.contains("`offset`") && error.contains("minimum"),
        "{error}"
    );
    let error = rejected(prepared(
        "todo_write",
        r#"{"todos":[{"content":"a","status":"later"}]}"#,
    ));
    assert!(error.contains("`todos/0/status`"), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_calls_never_reach_the_executor_where_approval_happens() {
    let fake = Arc::new(Fake::new());
    let calls = [
        ("w1", "file_edit", r#"{"path":"a","old_string":"x"}"#),
        (
            "w2",
            "file_edit",
            r#"{"path":"a","old_string":"x","new_string":"y","mode":"fast"}"#,
        ),
        (
            "w3",
            "file_edit",
            r#"{"path":"a","old_string":"x","new_string":"y"}"#,
        ),
    ];
    let prepared = calls
        .iter()
        .map(|(id, name, raw)| {
            (
                (*id).to_owned(),
                prepare(&ToolCall::new(*id, *name, *raw), fake.as_ref()),
            )
        })
        .collect();
    let results = run_batch(fake.clone(), prepared).await;
    assert_eq!(fake.ran.lock().len(), 1, "only the valid call ran");
    assert!(results[0].content.contains("new_string"));
    assert!(results[1].content.contains("unknown field `mode`"));
    assert_eq!(results[2].content, "ok");
}

#[test]
fn unknown_tools_get_the_nearest_names() {
    let error = rejected(prepared("read_file", "{}"));
    assert!(error.contains("Did you mean `file_read`"), "{error}");
    assert!(error.contains("search_tools"), "{error}");
    let error = rejected(prepared("teleport", "{}"));
    assert!(!error.contains("Did you mean"), "{error}");
    let error = rejected(prepared("github__create_issue", "{}"));
    assert!(error.contains("connector_call"), "{error}");
}

#[test]
fn call_tool_targets_are_resolved_repaired_and_validated() {
    let (name, arguments) = ran(prepared(
        "call_tool",
        r#"{"name":"read","arguments":"{\"path\":\"a\",\"offset\":\"2\"}"}"#,
    ));
    assert_eq!(name, "call_tool");
    assert_eq!(
        arguments,
        json!({"name":"file_read","arguments":{"path":"a","offset":2}})
    );
    let error = rejected(prepared(
        "call_tool",
        r#"{"name":"file_read","arguments":{}}"#,
    ));
    assert!(error.starts_with("call_tool `file_read`"), "{error}");
    let error = rejected(prepared(
        "call_tool",
        r#"{"name":"fiel_read","arguments":{}}"#,
    ));
    assert!(error.contains("Did you mean `file_read`"), "{error}");
    let error = rejected(prepared(
        "call_tool",
        r#"{"name":"call_tool","arguments":{}}"#,
    ));
    assert!(error.contains("cannot run call_tool"), "{error}");
}

#[test]
fn connector_calls_are_checked_against_the_discovered_schema() {
    let (_, arguments) = ran(prepared(
        "connector_call",
        r#"{"connector_id":"c","tool":"create_issue","arguments":{"title":"t","labels":"[\"bug\"]"}}"#,
    ));
    assert_eq!(
        arguments["arguments"],
        json!({"title":"t","labels":["bug"]})
    );
    let error = rejected(prepared(
        "connector_call",
        r#"{"connector_id":"c","tool":"create_issue","arguments":{}}"#,
    ));
    assert!(
        error.contains("`create_issue`") && error.contains("title"),
        "{error}"
    );
    let error = rejected(prepared(
        "connector_call",
        r#"{"connector_id":"c","tool":"create_isue","arguments":{}}"#,
    ));
    assert!(error.contains("Did you mean `create_issue`"), "{error}");
}

#[test]
fn schemas_that_do_not_compile_are_not_enforced() {
    let broken = json!({"type":"object","properties":{"a":{"type":"no-such-type"}}});
    let broken = CompiledSchema::new(broken);
    assert!(broken.validator().is_none());
    assert!(arguments::validate(&json!({"a":1}), &broken).is_ok());
}

#[test]
fn record_writes_the_call_as_it_will_run() {
    let fake = Fake::new();
    let calls = vec![
        ToolCall::new("a", "read", r#"{"path":"x","offset":"3"}"#),
        ToolCall::new("b", "file_read", r#"{"path":"y"}"#),
        ToolCall::new("c", "file_read", "{broken"),
    ];
    let mut assistant = Message::assistant_with_tool_calls("", calls.clone());
    for call in &calls {
        let ready = prepare(call, &fake);
        record(&mut assistant, call, &ready);
    }
    assert_eq!(assistant.tool_calls[0].name, "file_read");
    assert_eq!(
        serde_json::from_str::<Value>(&assistant.tool_calls[0].arguments).unwrap(),
        json!({"path":"x","offset":3})
    );
    assert_eq!(assistant.tool_calls[1].arguments, r#"{"path":"y"}"#);
    assert_eq!(assistant.tool_calls[2].arguments, "{broken");
}

#[test]
fn record_replays_repaired_arguments_in_the_native_content() {
    let fake = Fake::new();
    let call = ToolCall::new("a", "file_read", r#"{"path":"x","offset":"3"}"#);
    let mut assistant = Message::assistant_with_tool_calls("", vec![call.clone()]);
    assistant.native = Some(crate::model::NativeContent::Anthropic(vec![
        json!({"type":"tool_use","id":"a","name":"file_read","input":{"path":"x","offset":"3"}}),
    ]));
    let ready = prepare(&call, &fake);
    record(&mut assistant, &call, &ready);
    let Some(crate::model::NativeContent::Anthropic(items)) = &assistant.native else {
        panic!("native content lost");
    };
    assert_eq!(items[0]["input"], json!({"path":"x","offset":3}));
    assert_eq!(
        serde_json::from_str::<Value>(&assistant.tool_calls[0].arguments).unwrap(),
        items[0]["input"]
    );
}
