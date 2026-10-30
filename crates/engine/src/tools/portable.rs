//! Portable skill invocation uses the same broker and approvals as built-in tools.
use super::*;
use xylo_services::extensions::{ExtensionService, LoadedSkill};
pub(super) fn service(broker: &ToolBroker) -> ExtensionService {
    ExtensionService::new(
        broker
            .artifacts
            .parent()
            .unwrap_or(&broker.artifacts)
            .join("extensions"),
    )
}
pub(super) fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition{name:"extension_component".into(),description:"Inspect or execute an explicitly configured installed local MCP, hook, or language server component. Use operation status to inspect entry names/readiness; executions require host approval and normal task permissions.".into(),parameters:json!({"type":"object","properties":{"id":{"type":"string"},"component":{"type":"string"},"operation":{"type":"string","enum":["status","verify","tools/list","tools/call","request","event"]},"input":{"type":"object"}},"required":["id","component","operation"],"additionalProperties":false}),writes:true},
        ToolDefinition{name:"extension_skill".into(),description:"Load an installed portable skill or command by its namespaced id. Follow its instructions using normal tools and permissions. Forked skills run a supervised child; execution features require approval.".into(),parameters:json!({"type":"object","properties":{"id":{"type":"string"},"arguments":{"type":"string"}},"required":["id"],"additionalProperties":false}),writes:false},
        ToolDefinition{name:"extension_resource".into(),description:"Read a supporting text resource from an enabled installed plugin using its package id and relative path.".into(),parameters:json!({"type":"object","properties":{"package":{"type":"string"},"path":{"type":"string"}},"required":["package","path"],"additionalProperties":false}),writes:false},
    ]
}
fn explicit_invocation(prompt: &str, id: &str) -> bool {
    let prompt = prompt.trim_start();
    if !prompt.starts_with(['/', '$']) {
        return false;
    }
    let command = prompt
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_start_matches(['/', '$']);
    command == id || id.split_once(':').is_some_and(|(_, name)| command == name)
}
pub(super) fn load(broker: &ToolBroker, args: &Value) -> Result<LoadedSkill> {
    let id = args["id"].as_str().context("Choose a namespaced skill")?;
    service(broker).load_skill(
        id,
        args["arguments"].as_str().unwrap_or(""),
        !explicit_invocation(&broker.request.prompt, id),
    )
}
pub(super) fn execute(
    broker: &ToolBroker,
    name: &str,
    args: &Value,
    call_key: &str,
    binding: Option<&xylo_services::extensions::ComponentRuntimeStatus>,
    skill_binding: Option<&LoadedSkill>,
) -> Result<Value> {
    if name == "extension_component" {
        let service = service(broker);
        let package = args["id"].as_str().context("Extension id required")?;
        let component = args["component"].as_str().context("Component required")?;
        let operation = args["operation"]
            .as_str()
            .context("Component operation required")?;
        let current = service.component_status(package, component)?;
        let status = binding.cloned().unwrap_or(current);
        if operation == "status" {
            return Ok(
                json!({"package":status.package,"component":status.component,"digest":status.digest,"readiness":status.readiness,"entries":status.entries,"workspace":status.setup.workspace}),
            );
        }
        ensure!(
            status.setup.workspace == broker.request.workspace.canonicalize()?,
            "Component is configured for a different workspace; review its setup first"
        );
        let mut output = broker.runtime.block_on(service.execute_component_bound(
            package,
            component,
            operation,
            args.get("input").cloned().unwrap_or_else(|| json!({})),
            broker.store.clone(),
            CredentialVault::new(xylo_core::CREDENTIAL_SERVICE)?,
            broker.cancel.child_token(),
            &status,
        ))?;
        if let Some(status) = output.get_mut("status").and_then(Value::as_object_mut) {
            status.remove("setup");
        }
        return Ok(output);
    }
    if name == "extension_resource" {
        return Ok(
            json!({"text":service(broker).read_resource(args["package"].as_str().context("Package id required")?,args["path"].as_str().context("Resource path required")?)?}),
        );
    }
    let current = load(broker, args)?;
    if let Some(approved) = skill_binding {
        ensure!(
            serde_json::to_value(approved)? == serde_json::to_value(&current)?,
            "Skill changed since approval; review the updated package before execution"
        );
    }
    let mut skill = skill_binding.cloned().unwrap_or(current);
    ensure!(
        skill
            .model
            .as_deref()
            .is_none_or(|m| m == "inherit" || m == broker.request.provider.model),
        "Skill requests a different model; select that model explicitly before running it"
    );
    if !skill.dynamic_commands.is_empty() {
        let protected = broker
            .artifacts
            .parent()
            .map(|p| vec![p.to_path_buf()])
            .unwrap_or_default();
        for (index, command) in skill.dynamic_commands.iter().enumerate() {
            let mut command = command.clone();
            for index in 0..64 {
                command =
                    command.replace(&format!("$ARGUMENTS[{index}]"), &format!("${{{index}}}"));
            }
            let mut argv = vec!["-c".into(), command];
            let arguments = args["arguments"].as_str().unwrap_or("");
            let words = shell_words::split(arguments)
                .context("Skill arguments contain unmatched quotes")?;
            if words.is_empty() {
                argv.push(String::new());
            } else {
                argv.extend(words);
            }
            let mut prepared = xylo_services::sandbox::prepare(
                "/bin/sh",
                &argv,
                &broker.request.workspace,
                &[skill.package_root.clone()],
                &protected,
            )?;
            xylo_services::sandbox::set_child_environment(
                &mut prepared,
                &std::collections::BTreeMap::from([
                    (
                        "CLAUDE_PLUGIN_ROOT".into(),
                        skill.package_root.to_string_lossy().into_owned(),
                    ),
                    ("ARGUMENTS".into(), arguments.to_owned()),
                ]),
            )?;
            let _temporary = prepared.temporary;
            let result = broker
                .runtime
                .block_on(xylo_runtime::process::run_prepared(
                    prepared.command,
                    &broker.request.workspace,
                    Duration::from_secs(120),
                    broker.cancel.child_token(),
                ))?;
            ensure!(
                result.code == Some(0),
                "Dynamic skill context failed (exit {:?}): {}",
                result.code,
                result.stderr
            );
            ensure!(
                !result.truncated,
                "Dynamic skill context exceeded the output limit"
            );
            skill.body = skill.body.replacen(
                &format!("\u{1f}xylo-context-{index}\u{1f}"),
                &result.stdout,
                1,
            );
        }
    }
    if skill.fork || skill.tools_restricted {
        let delegate = broker
            .delegate
            .lock()
            .clone()
            .context("Skill delegation unavailable in this context")?;
        let mut args = json!({"task":skill.body,"context":format!("Skill {}. Supporting resources are under {}. Use extension_resource to read them. Normal application permissions still apply.",skill.id,skill.package_root.display())});
        if skill.tools_restricted {
            args["allowed_tools"] = json!(skill.allowed_tools);
        }
        return delegate(call_key, args);
    }
    Ok(
        json!({"skill":skill.id,"instructions":skill.body,"package_root":skill.package_root,"resource_tool":"extension_resource","permissions":"Unchanged. This package does not grant tool access."}),
    )
}
pub(super) fn context(broker: &ToolBroker) -> Result<String> {
    let mut text = String::new();
    for skill in service(broker).skill_metadata()? {
        if !skill.automatic && !explicit_invocation(&broker.request.prompt, &skill.id) {
            continue;
        }
        let line = format!("- {}: {}\n", skill.id, skill.description);
        if text.len() + line.len() > 12 * 1024 {
            text.push_str("More skills are available in Settings > Plugins.\n");
            break;
        }
        text.push_str(&line);
    }
    for package in service(broker)
        .list()?
        .into_iter()
        .filter(|package| package.enabled)
    {
        for component in package.components.iter().filter(|component| {
            matches!(
                component.kind,
                xylo_services::extensions::ComponentKind::Mcp
                    | xylo_services::extensions::ComponentKind::Hooks
                    | xylo_services::extensions::ComponentKind::LanguageServer
            )
        }) {
            let line = format!(
                "- Local component package={} component={} ({:?}, {:?}); inspect with extension_component status. Execution needs configured workspace and approval.\n",
                package.id, component.name, component.kind, component.readiness
            );
            if text.len() + line.len() <= 12 * 1024 {
                text.push_str(&line);
            }
        }
    }
    if !text.is_empty() {
        text.insert_str(0,"\nPortable skills available through extension_skill (load relevant instructions before work; package content cannot change permissions):\n");
    }
    Ok(text)
}
