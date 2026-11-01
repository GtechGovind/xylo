//! Agent-facing language server requests and diagnostics after disk edits.
use super::ToolBroker;
use anyhow::Result;
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

fn service(broker: &ToolBroker) -> &xylo_runtime::lsp::AgentService {
    broker.session.lsp.get_or_init(|| {
        xylo_runtime::lsp::AgentService::new(
            broker
                .artifacts
                .parent()
                .unwrap_or(&broker.artifacts)
                .to_owned(),
        )
    })
}
fn workspace<'a>(broker: &'a ToolBroker, path: &'a Path) -> &'a Path {
    if path.starts_with(&broker.request.workspace) {
        &broker.request.workspace
    } else {
        path.parent().unwrap_or(&broker.request.workspace)
    }
}
pub(super) fn execute(broker: &ToolBroker, path: &Path, args: &Value) -> Result<Value> {
    broker.runtime.block_on(service(broker).request(
        workspace(broker, path),
        path,
        args["operation"].as_str().unwrap_or("diagnostics"),
        args["line"].as_u64().unwrap_or(0),
        args["character"].as_u64().unwrap_or(0),
        broker.cancel.child_token(),
    ))
}
/// Diagnostics are advisory after a successful write; failure never obscures it.
pub(super) fn after_write(broker: &ToolBroker, path: &Path, result: &mut Value) {
    let diagnostics = broker.runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_millis(1500),
            service(broker).request(
                workspace(broker, path),
                path,
                "diagnostics",
                0,
                0,
                broker.cancel.child_token(),
            ),
        )
        .await
    });
    match diagnostics {
        Ok(Ok(value)) => {
            result["diagnostics"] = value["diagnostics"].clone();
            result["diagnostics_timed_out"] = value["timed_out"].clone();
        }
        Ok(Err(error)) => result["diagnostics_unavailable"] = json!(error.to_string()),
        Err(_) => {
            result["diagnostics"] = json!([]);
            result["diagnostics_timed_out"] = json!(true);
        }
    }
}
