//! Lifecycle hooks share the action journal and pass structured controls to the loop.
use super::*;
use sha2::Digest;
use xylo_services::{
    hooks::{self, HookOutput},
    settings_files::{PermissionDecision, SettingsFiles},
};

#[derive(Default)]
pub(super) struct HookState {
    pub pre: Option<HookOutput>,
    pub permission: bool,
    pub requested: Option<Value>,
}

impl ToolBroker {
    pub(super) fn validate_updated_input(&self, name: &str, input: &Value) -> Result<()> {
        if let Some(schema) = self.compiled_parameters(name) {
            crate::agent_loop::validate_hook_input(name, input, &schema)?;
        }
        if name == "connector_call" {
            let catalog = self
                .connector_catalog(input)
                .context("Hook updatedInput names an undiscovered connector")?;
            let tool = input["tool"]
                .as_str()
                .context("Hook updatedInput requires a connector tool")?;
            let (_, schema) = catalog
                .iter()
                .find(|(name, _)| name == tool)
                .context("Hook updatedInput names an unknown connector tool")?;
            ensure!(
                schema.validator().is_some(),
                "Hook updatedInput target has an invalid schema"
            );
            crate::agent_loop::validate_hook_input(tool, &input["arguments"], schema)?;
        }
        Ok(())
    }

    /// Runs a lifecycle event at a role-safe boundary. Callers interpret stop controls.
    pub(crate) fn lifecycle(
        &self,
        namespace: Option<&str>,
        event: &str,
        mut input: Value,
    ) -> Result<HookOutput> {
        let _root = self.workspace.gate.read_recursive();
        if namespace.is_none() && event == "UserPromptSubmit" {
            input["prompt"] = json!(self.request.prompt);
        }
        let key = format!(
            "lifecycle:{event}:{}",
            hex::encode(sha2::Sha256::digest(serde_json::to_vec(&input)?))
        );
        let output = self.event_hooks(namespace, &key, event, "", &Value::Null, Some(&input))?;
        if event == "Notification" {
            self.hook_notes.lock().extend(
                output
                    .context
                    .iter()
                    .cloned()
                    .map(|text| (xylo_core::NoteKind::Hook, text)),
            );
        }
        Ok(output)
    }
    pub(super) fn run_event_hooks(
        &self,
        namespace: Option<&str>,
        call_id: &str,
        event: &str,
        tool_name: &str,
        arguments: &Value,
        result: Option<&Value>,
    ) -> Result<HookOutput> {
        let output = self.event_hooks(namespace, call_id, event, tool_name, arguments, result)?;
        if output.stop_reason.is_some()
            || event == "PostToolUse" && output.decision == Some(PermissionDecision::Deny)
        {
            *self.hook_stop.lock() = Some(
                output
                    .stop_reason
                    .clone()
                    .unwrap_or_else(|| "Post-tool hook denied continuation".into()),
            );
        }
        self.hook_notes.lock().extend(
            output
                .context
                .iter()
                .cloned()
                .map(|text| (xylo_core::NoteKind::Hook, text)),
        );
        Ok(output)
    }
    fn event_hooks(
        &self,
        namespace: Option<&str>,
        call_id: &str,
        event: &str,
        tool_name: &str,
        arguments: &Value,
        result: Option<&Value>,
    ) -> Result<HookOutput> {
        let root = self.workspace_root();
        if root != self.request.workspace {
            return self
                .rebound(root)?
                .event_hooks(namespace, call_id, event, tool_name, arguments, result);
        }
        let settings = SettingsFiles::for_workspace(&self.request.workspace).load()?;
        if settings.value.get("disableAllHooks") == Some(&Value::Bool(true)) {
            return Ok(HookOutput::default());
        }
        let mut input = json!({"event":event,"hook_event_name":event,"tool_name":tool_name,"tool_input":arguments,"tool_response":result,"session_id":self.request.session_id,"cwd":self.request.workspace});
        if tool_name.is_empty()
            && let Some(fields) = result.and_then(Value::as_object)
        {
            for (key, value) in fields {
                input[key] = value.clone();
            }
        }
        let alias = if tool_name == "connector_call" {
            self.connector(arguments).ok().map(|config| {
                format!(
                    "mcp__{}__{}",
                    connector_slug(&config.name),
                    arguments["tool"].as_str().unwrap_or_default()
                )
            })
        } else {
            None
        };
        if let Some(alias) = &alias {
            input["tool_alias"] = json!(alias);
        }
        let commands = hooks::configured_alias(&settings, event, tool_name, alias.as_deref())?;
        let plugin_hooks =
            portable::service(self).matching_hooks(event, &input, &self.request.workspace)?;
        if commands.is_empty() && plugin_hooks.is_empty() {
            return Ok(HookOutput::default());
        }
        ensure!(
            !self
                .store
                .unresolved_actions(self.request.session_id)?
                .iter()
                .any(|action| action.state == "outcome_unknown"
                    && (action.intent.name == "settings_hook"
                        || action.intent.name == "extension_component"
                            && action
                                .intent
                                .arguments
                                .get("operation")
                                .and_then(Value::as_str)
                                == Some("event"))),
            "A previous hook outcome is unknown; reconcile its action before further hook execution"
        );
        let mut output = HookOutput::default();
        for (index, command) in commands.into_iter().enumerate() {
            let key = Self::journal_key(
                namespace,
                &format!("{call_id}:settings-hook:{event}:{index}"),
            );
            let args = json!({"command":command.command,"input":input});
            let value = if let Some(previous) =
                self.store
                    .action_for_key(self.request.session_id, self.request.turn_id, &key)?
            {
                ensure!(
                    previous.intent.arguments == args,
                    "Hook configuration changed during replay; start a fresh turn"
                );
                ensure!(
                    previous.state == "succeeded",
                    "Previous hook did not complete; reconcile its outcome before retrying"
                );
                previous.result.context("Hook result missing")?
            } else {
                let intent = ActionIntent {
                    id: Uuid::new_v4(),
                    session_id: self.request.session_id,
                    turn_id: self.request.turn_id,
                    name: "settings_hook".into(),
                    arguments: args,
                    writes: true,
                    idempotency_key: key,
                };
                ensure!(
                    matches!(self.store.begin_action(&intent)?, ActionDecision::Execute),
                    "Hook is already in progress"
                );
                self.store.mark_action_running(intent.id)?;
                let protected = self
                    .artifacts
                    .parent()
                    .map(|p| vec![p.to_owned()])
                    .unwrap_or_default();
                match self.runtime.block_on(hooks::run(
                    &command,
                    &input,
                    &self.request.workspace,
                    &protected,
                    self.cancel.child_token(),
                )) {
                    Ok(value) => {
                        self.store.finish_action(intent.id, &value)?;
                        value
                    }
                    Err(error) => {
                        self.store
                            .mark_action_unknown(intent.id, &format!("{error:#}"))?;
                        return Err(error);
                    }
                }
            };
            let control = HookOutput::parse(&value)?;
            if let Some(updated) = &control.updated_input {
                input["tool_input"] = updated.clone();
            }
            output.merge(control);
        }
        for (index, hook) in plugin_hooks.into_iter().enumerate() {
            let hook_id = format!("{call_id}:hook:{event}:{index}");
            let args = json!({"id":hook.package,"component":hook.component,"operation":"event","input":input});
            self.execute_scoped(
                namespace,
                &hook_id,
                "extension_component",
                args,
                Origin::Hook,
            )?;
            let actual = self
                .store
                .action_for_key(
                    self.request.session_id,
                    self.request.turn_id,
                    &Self::journal_key(namespace, &hook_id),
                )?
                .and_then(|a| a.result)
                .context("Hook outcome was not recorded")?;
            for entry in actual
                .get("results")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let mut control = HookOutput::parse(&entry["output"])?;
                // Preserved controls outrank any secret redaction of plugin output.
                if let Some(decision) = entry
                    .pointer("/hook_control/decision")
                    .filter(|v| !v.is_null())
                {
                    control.decision = Some(serde_json::from_value(decision.clone())?);
                }
                if let Some(reason) = entry
                    .pointer("/hook_control/stop_reason")
                    .and_then(Value::as_str)
                {
                    control.stop_reason = Some(reason.into());
                }
                if let Some(reason) = entry
                    .pointer("/hook_control/block_reason")
                    .and_then(Value::as_str)
                {
                    control.block_reason = Some(reason.into());
                }
                if let Some(updated) = &control.updated_input {
                    input["tool_input"] = updated.clone();
                }
                output.merge(control);
            }
        }
        Ok(output)
    }
    pub(super) fn enforce_hook(output: &HookOutput) -> Result<()> {
        if let Some(reason) = &output.stop_reason {
            bail!("{reason}");
        }
        if output.decision == Some(PermissionDecision::Deny) {
            bail!(
                "{}",
                output
                    .block_reason
                    .as_deref()
                    .unwrap_or("Configured hook denied this operation")
            );
        }
        Ok(())
    }
}
