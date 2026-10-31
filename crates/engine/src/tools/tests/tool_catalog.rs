//! Ranked tool search, connector catalogs and scheduling classification at
//! the real broker boundary.
use super::super::*;
use super::fixture;
use crate::{
    agent_loop::{Access, ToolExecutor},
    broker_tools::BrokerTools,
};

/// A broker over `existing`'s store that sees the connectors saved since,
/// with its event receiver.
fn reopened(existing: &ToolBroker) -> (Arc<ToolBroker>, tokio::sync::mpsc::Receiver<EngineEvent>) {
    let (events, receiver) = tokio::sync::mpsc::channel(256);
    let broker = Arc::new(
        ToolBroker::new(
            existing.request.clone(),
            existing.store.clone(),
            existing.capability_config.clone(),
            existing.session.clone(),
            events,
            CancellationToken::new(),
        )
        .unwrap(),
    );
    (broker, receiver)
}

fn connector(name: &str, transport: &str) -> ConnectorConfig {
    ConnectorConfig {
        id: Uuid::new_v4(),
        name: name.into(),
        transport: transport.into(),
        endpoint: "http://127.0.0.1:1".into(),
        command: (transport == "stdio").then(|| "/usr/bin/true".into()),
        args: Vec::new(),
        credential_ref: None,
        trusted: false,
    }
}

fn mcp_tool(name: &str, description: &str, writes: bool) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: description.into(),
        parameters: json!({"type":"object","properties":{"title":{"type":"string"}},"required":["title"]}),
        writes,
    }
}

fn search(broker: &Arc<ToolBroker>, query: &str) -> Value {
    let result = broker
        .execute(
            &format!("search-{}", Uuid::new_v4()),
            "search_tools",
            json!({ "query": query }),
        )
        .unwrap();
    let value = serde_json::from_str::<Value>(&result).unwrap();
    assert!(value["summary"]["tools"].is_array(), "{value}");
    value["summary"].clone()
}

fn names(summary: &Value) -> Vec<String> {
    summary["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_ranks_the_most_relevant_tool_first_with_its_arguments() {
    let (_directory, broker, _events) = fixture(PermissionMode::AskForApproval);
    tokio::task::spawn_blocking(move || {
        let summary = search(&broker, "read a file");
        assert_eq!(names(&summary)[0], "file_read", "{summary}");
        assert!(summary["tools"][0]["parameters"]["properties"]["path"].is_object());
        assert!(names(&summary).len() <= SEARCH_RESULTS);
        let summary = search(&broker, "git diff of staged changes");
        assert_eq!(names(&summary)[0], "git_diff", "{summary}");
        let summary = search(&broker, "teleport");
        assert!(names(&summary).is_empty(), "{summary}");
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovered_mcp_tools_join_the_search_under_server_names() {
    let (_directory, broker, _events) = fixture(PermissionMode::AskForApproval);
    let tracker = connector("Issue Tracker", "stdio");
    let files = connector("Files", "stdio");
    let pending = connector("Never Opened", "stdio");
    for config in [&tracker, &files, &pending] {
        broker.store.save_connector(config).unwrap();
    }
    let (broker, _events) = reopened(&broker);
    tokio::task::spawn_blocking(move || {
        broker
            .connectors
            .cache_tools_for_tests(
                &tracker,
                &[mcp_tool(
                    "create_issue",
                    "Create an issue in the tracker",
                    true,
                )],
            )
            .unwrap();
        broker
            .connectors
            .cache_tools_for_tests(&files, &[mcp_tool("file_read", "Read a file", false)])
            .unwrap();
        let summary = search(&broker, "create issue");
        let found = &summary["tools"][0];
        assert_eq!(found["name"], "issue_tracker__create_issue", "{summary}");
        assert_eq!(found["connector_call"]["connector_id"], json!(tracker.id));
        assert_eq!(found["connector_call"]["tool"], "create_issue");
        assert_eq!(
            summary["undiscovered_connectors"][0]["connector_id"],
            json!(pending.id),
            "{summary}"
        );
        // Third-party tools never outrank the built-in tool they overlap.
        let summary = search(&broker, "read file");
        let order = names(&summary);
        let builtin = order.iter().position(|name| name == "file_read").unwrap();
        let mcp = order
            .iter()
            .position(|name| name == "files__file_read")
            .unwrap();
        assert!(builtin < mcp, "{summary}");
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_search_lists_only_tools_that_are_reads_by_a_trusted_classification() {
    let (_directory, broker, _events) = fixture(PermissionMode::ReadOnly);
    let tracker = ConnectorConfig {
        trusted: true,
        ..connector("Tracker", "stdio")
    };
    let stranger = connector("Stranger", "stdio");
    broker.store.save_connector(&tracker).unwrap();
    broker.store.save_connector(&stranger).unwrap();
    let (broker, _events) = reopened(&broker);
    tokio::task::spawn_blocking(move || {
        let catalog = [
            mcp_tool("create_issue", "Create an issue", true),
            mcp_tool("list_issues", "List issues", false),
        ];
        for config in [&tracker, &stranger] {
            broker
                .connectors
                .cache_tools_for_tests(config, &catalog)
                .unwrap();
        }
        assert!(!broker.mode().allows_writes());
        let found = names(&search(&broker, "issue"));
        assert_eq!(found, ["tracker__list_issues"], "{found:?}");
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_trusted_read_only_connector_tools_are_scheduled_as_reads() {
    let (_directory, broker, _events) = fixture(PermissionMode::AskForApproval);
    let gmail = connector("Mail", "gmail");
    let server = connector("Server", "stdio");
    let trusted = ConnectorConfig {
        trusted: true,
        ..connector("Own server", "stdio")
    };
    for config in [&gmail, &server, &trusted] {
        broker.store.save_connector(config).unwrap();
    }
    let (broker, _events) = reopened(&broker);
    tokio::task::spawn_blocking(move || {
        for config in [&server, &trusted] {
            broker
                .connectors
                .cache_tools_for_tests(config, &[mcp_tool("lookup", "Look something up", false)])
                .unwrap();
        }
        let tools = BrokerTools::root(broker.clone());
        let call = |id: Uuid, tool: &str| json!({"connector_id":id,"tool":tool,"arguments":{}});
        let native_read = call(gmail.id, "gmail_get_message");
        assert_eq!(
            tools.access("connector_call", &native_read),
            Some(Access::ReadOnly)
        );
        assert_eq!(
            tools.access(
                "call_tool",
                &json!({"name":"connector_call","arguments":native_read})
            ),
            Some(Access::ReadOnly)
        );
        assert_eq!(
            tools.access("connector_call", &call(gmail.id, "gmail_send_draft")),
            Some(Access::Writes)
        );
        // An MCP server's readOnlyHint is its own, untrusted claim.
        assert_eq!(
            tools.access("connector_call", &call(server.id, "lookup")),
            Some(Access::Writes)
        );
        assert_eq!(
            tools.access("connector_call", &call(trusted.id, "lookup")),
            Some(Access::ReadOnly)
        );
        assert_eq!(
            tools.access("connector_call", &call(Uuid::new_v4(), "lookup")),
            Some(Access::Writes)
        );
        // The discovered schemas are what dispatch validates against.
        let catalog = tools.connector_tools(&call(server.id, "lookup")).unwrap();
        assert_eq!(catalog[0].0, "lookup");
        assert!(
            tools
                .connector_tools(&call(Uuid::new_v4(), "lookup"))
                .is_none()
        );
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn executor_exposes_schemas_and_names_that_dispatch_checks_against() {
    let (_directory, broker, _events) = fixture(PermissionMode::AskForApproval);
    let root = BrokerTools::root(broker.clone());
    let child = BrokerTools::child(broker.clone(), "parent".into());
    assert!(root.names().contains(&"agent_delegate".to_owned()));
    assert!(!child.names().contains(&"agent_delegate".to_owned()));
    assert!(child.parameters("agent_delegate").is_none());
    let schema = root.parameters("file_read").unwrap();
    assert_eq!(schema.schema()["required"], json!(["path"]));
    // Every schema Xylo itself declares compiles, so validation always runs
    // for built-in and native connector tools.
    let native = [
        "github",
        "google_drive",
        "gmail",
        "google_calendar",
        "slack",
    ]
    .into_iter()
    .flat_map(|transport| NativeConnectors::tools(transport).unwrap());
    for definition in broker.definitions().iter().cloned().chain(native) {
        assert!(
            jsonschema::validator_for(&definition.parameters).is_ok(),
            "{} has an invalid schema",
            definition.name
        );
    }
}

#[test]
fn search_results_shrink_to_fit_the_tool_result_budget() {
    let schema = json!({"type":"object","properties":{"path":{"type":"string","description":"x".repeat(300)}},"required":["path"]});
    let result = |name: &str| json!({"name":name,"description":"d","parameters":schema});
    let mut found: Vec<Value> = (0..8)
        .map(|index| result(&format!("tool_{index}")))
        .collect();
    let all = serde_json::to_vec(&found).unwrap().len();
    fit_search_results(&mut found, all);
    assert!(
        found
            .iter()
            .all(|result| result.get("parameters").is_some())
    );
    fit_search_results(&mut found, 1500);
    assert!(serde_json::to_vec(&found).unwrap().len() <= 1500);
    assert!(
        found[0].get("parameters").is_some(),
        "the best match keeps its schema"
    );
    // Schemas give way from the weakest result up, and only as far as needed.
    let kept = found
        .iter()
        .take_while(|result| result.get("parameters").is_some())
        .count();
    assert!((1..found.len()).contains(&kept), "{kept}");
    assert!(
        found[kept..]
            .iter()
            .all(|result| result["arguments"] == "path: string (required)")
    );
    assert_eq!(found.len(), 8);
    fit_search_results(&mut found, 10);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0]["arguments"], "path: string (required)");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approve_for_me_honours_a_read_only_hint_only_from_a_trusted_server() {
    let (_directory, broker, _events) = fixture(PermissionMode::ApproveForMe);
    let stranger = connector("Stranger", "stdio");
    let own = ConnectorConfig {
        trusted: true,
        ..connector("Own server", "stdio")
    };
    for config in [&stranger, &own] {
        broker.store.save_connector(config).unwrap();
    }
    let (broker, _events) = reopened(&broker);
    tokio::task::spawn_blocking(move || {
        let verdict = |config: &ConnectorConfig| {
            broker
                .connectors
                .cache_tools_for_tests(config, &[mcp_tool("lookup", "Look something up", false)])
                .unwrap();
            let arguments =
                json!({"connector_id":config.id,"tool":"lookup","arguments":{"title":"x"}});
            let definition = broker
                .effective_definition("connector_call", &arguments)
                .unwrap();
            permissions::decide(
                PermissionMode::ApproveForMe,
                &Call {
                    name: "connector_call",
                    arguments: &arguments,
                    writes: definition.writes,
                    origin: Origin::Model,
                    reach: &Reach::Project,
                    workspace: &broker.request.workspace,
                    network: None,
                    untrusted_input: false,
                },
            )
        };
        assert!(
            matches!(verdict(&stranger), Verdict::Ask(_)),
            "an untrusted server's hint asks"
        );
        assert_eq!(
            verdict(&own),
            Verdict::Allow,
            "a trusted server's lookup runs"
        );
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_search_index_is_reused_until_what_it_covers_changes() {
    let (_directory, broker, _events) = fixture(PermissionMode::ApproveForMe);
    let tracker = ConnectorConfig {
        trusted: true,
        ..connector("Tracker", "stdio")
    };
    broker.store.save_connector(&tracker).unwrap();
    let (broker, _events) = reopened(&broker);
    tokio::task::spawn_blocking(move || {
        let cached = || broker.lookup.search.lock().clone().unwrap();
        let first = search(&broker, "create issue");
        let index = cached();
        assert_eq!(search(&broker, "create issue"), first, "same answer");
        assert!(Arc::ptr_eq(&index, &cached()), "index reused");
        // A permission change alters what search may offer.
        broker.session.permissions().set(PermissionMode::ReadOnly);
        search(&broker, "create issue");
        assert!(!Arc::ptr_eq(&index, &cached()), "rebuilt for the new mode");
        broker
            .session
            .permissions()
            .set(PermissionMode::ApproveForMe);
        // A discovery is seen once the snapshot is refreshed, as
        // connector_tools does after rediscovering a server.
        broker
            .connectors
            .cache_tools_for_tests(
                &tracker,
                &[mcp_tool("create_issue", "Create an issue", true)],
            )
            .unwrap();
        assert!(!names(&search(&broker, "create issue")).contains(&"tracker__create_issue".into()));
        broker.lookup.invalidate_connectors();
        assert_eq!(
            names(&search(&broker, "create issue"))[0],
            "tracker__create_issue"
        );
        // Compiled schemas are shared with delegated children.
        let child = broker.scoped_child(Uuid::new_v4()).unwrap();
        assert!(Arc::ptr_eq(
            &broker.compiled_parameters("file_read").unwrap(),
            &child.compiled_parameters("file_read").unwrap()
        ));
    })
    .await
    .unwrap();
}
