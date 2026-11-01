//! Headless tool constraints and externally implemented SDK callbacks.
use super::*;

impl ToolBroker {
    /// Installs explicit headless tool capabilities before building the prompt.
    pub(crate) fn configure_run(&mut self, options: &crate::RunOptions) -> Result<()> {
        ensure!(options.max_turns != Some(0), "--max-turns must be positive");
        for definition in &options.custom_tools {
            ensure!(
                definition.name.starts_with("sdk_")
                    && definition
                        .name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "Custom tools require an sdk_ name with letters, digits or underscores"
            );
            ensure!(
                !self
                    .definitions
                    .iter()
                    .any(|existing| existing.name == definition.name),
                "Duplicate custom tool: {}",
                definition.name
            );
            ensure!(
                definition.writes,
                "SDK tools must declare writes=true because they execute outside the sandbox"
            );
            jsonschema::validator_for(&definition.parameters)
                .context("Invalid custom tool schema")?;
            self.definitions.push(definition.clone());
            self.advertised.push(definition.clone());
        }
        self.on_refusal = options.on_refusal.clone();
        self.custom_tools = options.custom_tools.clone();
        self.custom_handler = options.custom_handler.clone();
        if let Some(allowed) = &options.allowed_tools {
            self.restrict_tools(allowed)?;
        }
        if !options.disallowed_tools.is_empty() {
            let mut excluded = Self::new(
                self.request.clone(),
                self.store.clone(),
                self.capability_config.clone(),
                self.session.clone(),
                self.events.clone(),
                self.cancel.clone(),
            )?;
            excluded.definitions.extend(options.custom_tools.clone());
            excluded.restrict_tools(&options.disallowed_tools)?;
            let denied = excluded.allowed_tools.unwrap_or_default();
            let allowed = self
                .definitions
                .iter()
                .filter(|definition| !denied.contains(&definition.name))
                .map(|definition| definition.name.clone())
                .collect::<Vec<_>>();
            self.restrict_tools(&allowed)?;
        }
        Ok(())
    }

    /// Records an explicit policy exclusion rejected before ordinary dispatch.
    pub(crate) fn observe_excluded(&self, name: &str) {
        if self.policy_excluded.contains(name)
            && let Some(refused) = &self.on_refusal
        {
            refused(&format!(
                "Tool '{name}' is not authorized: excluded by the allowed/disallowed tool policy"
            ));
        }
    }
}

/// Invokes external client code only after the broker authorization boundary.
pub(super) fn execute(
    broker: &ToolBroker,
    id: Uuid,
    name: &str,
    arguments: &Value,
) -> Result<Value> {
    broker.emit(EngineEvent::CustomToolRequested {
        action_id: id,
        name: name.into(),
        arguments: arguments.clone(),
    })?;
    broker
        .custom_handler
        .as_ref()
        .context("Custom tool handler is unavailable")?(
        id,
        name,
        arguments,
        broker.cancel.child_token(),
    )
}
