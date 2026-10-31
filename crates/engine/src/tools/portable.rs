//! Skills and plugin components. Built-in skills (compiled into the engine)
//! resolve by name ahead of installed portable packages; installed skills and
//! components use the same broker, journal and permissions as built-in tools.
use super::*;
use xylo_services::extensions::{ExtensionService, LoadedSkill};

/// The built-in skill an `extension_skill` call names, if any.
pub(super) fn bundled(args: &Value) -> Option<&'static prompts::BundledSkill> {
    args.get("id")
        .and_then(Value::as_str)
        .and_then(prompts::bundled_skill)
}
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
        ToolDefinition{name:"extension_component".into(),description:"Inspect or execute an explicitly configured installed local MCP or language server component. Use operation status to inspect entry names/readiness; executions follow the task's permissions.".into(),parameters:json!({"type":"object","properties":{"id":{"type":"string"},"component":{"type":"string"},"operation":{"type":"string","enum":["status","verify","tools/list","tools/call","request","event"]},"input":{"type":"object"}},"required":["id","component","operation"],"additionalProperties":false}),writes:true},
        ToolDefinition{name:"extension_skill".into(),description:"Load a skill's instructions by its id: a built-in skill name or an installed package:name. Follow them using normal tools and permissions. Forked skills run a supervised child.".into(),parameters:json!({"type":"object","properties":{"id":{"type":"string"},"arguments":{"type":"string"}},"required":["id"],"additionalProperties":false}),writes:false},
        ToolDefinition{name:"extension_resource".into(),description:"Read a supporting text resource of a skill or enabled plugin: package is the built-in skill name or the installed package id, path is relative to it.".into(),parameters:json!({"type":"object","properties":{"package":{"type":"string"},"path":{"type":"string"}},"required":["package","path"],"additionalProperties":false}),writes:false},
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
    action_id: uuid::Uuid,
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
            broker.session.vault().clone(),
            broker.cancel.child_token(),
            &status,
        ))?;
        if let Some(status) = output.get_mut("status").and_then(Value::as_object_mut) {
            status.remove("setup");
        }
        return Ok(output);
    }
    if name == "extension_resource" {
        let package = args["package"].as_str().context("Package id required")?;
        let path = args["path"].as_str().context("Resource path required")?;
        if let Some(skill) = prompts::bundled_skill(package) {
            let resource = skill.resource(path).with_context(|| {
                let available: Vec<_> = skill
                    .resources
                    .iter()
                    .map(|resource| resource.path)
                    .collect();
                format!("Skill {package} has no resource {path}; available: {available:?}")
            })?;
            return Ok(json!({"text":resource.content}));
        }
        return Ok(json!({"text":service(broker).read_resource(package, path)?}));
    }
    if let Some(skill) = bundled(args) {
        let resources: Vec<_> = skill
            .resources
            .iter()
            .map(|resource| resource.path)
            .collect();
        return Ok(
            json!({"skill":skill.name,"description":skill.description,"instructions":skill.body,"resources":resources,"resource_tool":"extension_resource","permissions":prompts::skill_permissions_note()}),
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
            let result = broker
                .runtime
                .block_on(xylo_runtime::process::run_prepared_sandboxed(
                    prepared,
                    &broker.request.workspace,
                    Duration::from_secs(120),
                    broker.cancel.child_token(),
                ))?
                .result;
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
        let mut args = json!({"task":skill.body,"context":prompts::forked_skill_context(&skill.id, &skill.package_root.display().to_string())});
        if skill.tools_restricted {
            args["allowed_tools"] = json!(skill.allowed_tools);
        }
        return delegate(&super::DelegateCall {
            parent_key: call_key.to_owned(),
            parent_action_id: action_id,
            arguments: args,
        });
    }
    Ok(
        json!({"skill":skill.id,"description":skill.description,"instructions":skill.body,"package_root":skill.package_root,"resource_tool":"extension_resource","permissions":prompts::skill_permissions_note()}),
    )
}
/// Index entries for installed portable skills that apply to this turn and
/// for local MCP and language server components the model can inspect.
pub(super) fn index(broker: &ToolBroker) -> Result<Vec<(String, String)>> {
    let service = service(broker);
    let mut entries: Vec<(String, String)> = service
        .skill_metadata()?
        .into_iter()
        .filter(|skill| skill.automatic || explicit_invocation(&broker.request.prompt, &skill.id))
        .map(|skill| (skill.id, skill.description))
        .collect();
    for package in service
        .list()?
        .into_iter()
        .filter(|package| package.enabled)
    {
        for component in package.components.iter().filter(|component| {
            matches!(
                component.kind,
                xylo_services::extensions::ComponentKind::Mcp
                    | xylo_services::extensions::ComponentKind::LanguageServer
            )
        }) {
            entries.push((
                format!("{}:{}", package.id, component.name),
                format!(
                    "Not a skill: an installed {:?} component ({:?}). Use extension_component with id {} and component {}, starting with operation status",
                    component.kind, component.readiness, package.id, component.name
                ),
            ));
        }
    }
    Ok(entries)
}
