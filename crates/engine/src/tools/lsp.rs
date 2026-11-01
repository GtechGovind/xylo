//! Agent-facing language server requests and diagnostics after disk edits.
use super::{
    ToolBroker,
    permissions::{self, Call, Origin, Verdict},
};
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
/// Automatic observations may use only separately granted native authority.
/// A file edit's approval never authorizes a native process or network access.
fn automatic_authorized(broker: &ToolBroker, path: &Path) -> bool {
    if broker
        .allowed_tools
        .as_ref()
        .is_some_and(|allowed| !allowed.contains("lsp"))
    {
        return false;
    }
    let arguments = json!({"path":path,"operation":"diagnostics"});
    let reach = broker.reach("lsp", &arguments);
    let call = Call {
        name: "lsp",
        arguments: &arguments,
        writes: true,
        origin: Origin::Model,
        reach: &reach,
        workspace: &broker.request.workspace,
        cwd: None,
        network: None,
        untrusted_input: false,
    };
    let Ok((verdict, explicit_rule)) = permissions::configured(broker.mode(), &call) else {
        return false;
    };
    match verdict {
        Verdict::Allow => true,
        Verdict::Refuse(_) => false,
        Verdict::Ask(_) => permissions::approval_rule_for(&call).is_some_and(|(key, _)| {
            !explicit_rule
                && (broker.rules.lock().contains(&key)
                    || broker
                        .store
                        .native_execution_granted(&broker.request.workspace, &key)
                        .unwrap_or(false))
        }),
    }
}

/// Diagnostics are advisory after a successful write; failure never obscures it.
pub(super) fn after_write(broker: &ToolBroker, path: &Path, result: &mut Value) {
    if !automatic_authorized(broker, path) {
        result["diagnostics_unavailable"] = json!(
            "Native language-server execution requires separate approval. Use lsp for this file and choose Always allow for its separate native execution and network approval to enable automatic diagnostics"
        );
        return;
    }
    // The enclosing edit observation includes server-controlled text, even if
    // startup fails or the server supplies only uncorrelated diagnostics.
    result["lsp_observation"] = json!(true);
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
            result["diagnostics_freshness"] = value["freshness"].clone();
            if let Some(reason) = value.get("unavailable_reason") {
                result["diagnostics_unavailable"] = reason.clone();
            }
        }
        Ok(Err(error)) => result["diagnostics_unavailable"] = json!(error.to_string()),
        Err(_) => {
            result["diagnostics"] = Value::Null;
            result["diagnostics_freshness"] = json!("unavailable");
            result["diagnostics_unavailable"] =
                json!("Current-revision diagnostics were not available within 1.5 seconds");
            result["diagnostics_timed_out"] = json!(true);
        }
    }
}

#[cfg(test)]
mod tests;
