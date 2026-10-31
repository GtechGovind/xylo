//! Application-owned durable tool and permission boundary.
//!
//! Every tool call is a journaled action with an idempotency key. Before an
//! intent exists the broker classifies the call (external metadata cannot
//! supply its own permission flag), reads the live permission mode and asks
//! [`permissions::decide`] whether it runs, needs the user's decision or is
//! refused. Writes to one file are serialized process-wide from preview to
//! write, so concurrent edits of one file in a batch both land and each
//! approval shows the change against current content. Child brokers created
//! for delegated work share the journal, the mode and the turn's approved
//! rules, and stamp their events with the parent `agent_delegate` action.
mod builtin;
mod code;
mod image_generation;
mod permissions;
mod portable;
mod risk;
use crate::prompts;
use anyhow::{Context, Result, bail, ensure};
pub(crate) use code::shell_name;
use parking_lot::Mutex;
pub use permissions::{ApprovalGate, PermissionCell, ToolSession};
use permissions::{Call, Origin, Reach, RuleSet, Verdict};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, mpsc},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use xylo_capabilities::{CapabilityConfig, CapabilityService};
use xylo_core::{
    ConnectorConfig, EngineEvent, MemoryRecord, PermissionMode, ToolDefinition, TurnPhase,
    TurnRequest,
};
use xylo_services::{
    ActionDecision, ActionIntent, ConnectorBroker, ConnectorLaunchScope, JournalAction,
    LearningService, NativeConnectors, SqliteMemory, Store,
};

/// A request to run a delegated task in a child agent.
pub struct DelegateCall {
    /// Durable journal key of the parent `agent_delegate` call; namespaces the child's actions.
    pub parent_key: String,
    /// The parent `agent_delegate` action; child tool events carry it as `parent_id`.
    pub parent_action_id: Uuid,
    /// The model's `agent_delegate` arguments.
    pub arguments: Value,
}

/// Runs a delegated task and returns the child's result.
pub type Delegate = Arc<dyn Fn(&DelegateCall) -> Result<Value> + Send + Sync>;

/// Tools offered to the model directly in every mode that allows them; the
/// rest stay discoverable through `search_tools` and `call_tool`.
const CORE_TOOLS: [&str; 19] = [
    "extension_skill",
    "extension_resource",
    "file_read",
    "file_edit",
    "file_write",
    "files_list",
    "find_files",
    "search_project",
    "git_status",
    "git_diff",
    "run_command",
    "shell_command",
    "todo_write",
    "agent_delegate",
    "memory_search",
    "memory_remember",
    "skill_read",
    "search_tools",
    "call_tool",
];

/// Largest approval or start presentation; long arguments are excerpted so an
/// event can never approach the worker's protocol line limit.
const PRESENTATION_EVENT_BYTES: usize = 256 * 1024;

pub struct ToolBroker {
    request: TurnRequest,
    store: Store,
    memory: SqliteMemory,
    capabilities: CapabilityService,
    capability_config: CapabilityConfig,
    definitions: Vec<ToolDefinition>,
    /// Tools usable in this turn (configured image provider, existing
    /// connectors); the live mode filters them again at every read.
    advertised: Vec<ToolDefinition>,
    allowed_tools: Option<std::collections::HashSet<String>>,
    session: ToolSession,
    /// "Always allow" rule keys in force for this turn, shared with children.
    rules: RuleSet,
    /// The `agent_delegate` action this child broker works for.
    parent_id: Option<Uuid>,
    events: tokio::sync::mpsc::Sender<EngineEvent>,
    cancel: CancellationToken,
    artifacts: PathBuf,
    runtime: tokio::runtime::Handle,
    resources: xylo_resources::ResourceManager,
    delegate: Mutex<Option<Delegate>>,
    connectors: ConnectorBroker,
    native: NativeConnectors,
    image_generation: image_generation::Configuration,
}
impl ToolBroker {
    pub fn new(
        request: TurnRequest,
        store: Store,
        config: CapabilityConfig,
        session: ToolSession,
        events: tokio::sync::mpsc::Sender<EngineEvent>,
        cancel: CancellationToken,
    ) -> Result<Self> {
        let artifacts = config.artifacts.clone();
        let capabilities = CapabilityService::new(config.clone())?;
        let image_generation = image_generation::Configuration::load(&artifacts);
        let mut definitions = builtin::definitions();
        definitions.extend(portable::definitions());
        definitions.push(image_generation.definition());
        definitions.extend(capabilities.tool_definitions());
        let has_connectors = !store.connectors()?.is_empty();
        let advertised = definitions
            .iter()
            .filter(|definition| {
                (definition.name != "image_generate" || image_generation.is_configured())
                    && (!matches!(
                        definition.name.as_str(),
                        "connector_call" | "connector_tools"
                    ) || has_connectors)
            })
            .cloned()
            .collect();
        let vault = session.vault().clone();
        let connector_scope = ConnectorLaunchScope {
            workspace: request.workspace.clone(),
            protected_paths: artifacts
                .parent()
                .map(|path| vec![path.to_owned()])
                .unwrap_or_default(),
            ..Default::default()
        };
        Ok(Self {
            memory: SqliteMemory::configured(
                store.clone(),
                &artifacts
                    .parent()
                    .context("Artifacts directory has no application data parent")?
                    .join("embedding.json"),
            )
            .unwrap_or_else(|_| {
                tracing::warn!(
                    "Optional embedding configuration unavailable; using lexical memory"
                );
                SqliteMemory::new(store.clone())
            }),
            rules: Arc::new(Mutex::new(request.approved_rules.iter().cloned().collect())),
            request,
            connectors: session.connectors(&store).with_local_scope(connector_scope),
            native: NativeConnectors::new(vault)?,
            image_generation,
            store,
            capabilities,
            capability_config: config,
            definitions,
            advertised,
            allowed_tools: None,
            session,
            parent_id: None,
            events,
            cancel,
            artifacts,
            runtime: tokio::runtime::Handle::current(),
            resources: xylo_resources::ResourceManager::new(),
            delegate: Mutex::new(None),
        })
    }
    /// A child shares durable identity, admission, the live mode and the
    /// turn's approved rules, but owns cancellation and connector processes.
    /// Cancelling it never cancels its parent or siblings. Its tool events
    /// carry `parent_action_id` as `parent_id`.
    pub(crate) fn scoped_child(&self, parent_action_id: Uuid) -> Result<Self> {
        let mut child = Self::new(
            self.request.clone(),
            self.store.clone(),
            self.capability_config.clone(),
            self.session.clone(),
            self.events.clone(),
            self.cancel.child_token(),
        )?;
        child.memory = self.memory.clone();
        child.resources = self.resources.clone();
        child.advertised = self.advertised.clone();
        child.rules = self.rules.clone();
        child.parent_id = Some(parent_action_id);
        Ok(child)
    }
    pub(crate) fn restrict_tools(&mut self, requested: &[String]) -> Result<()> {
        let mut allowed = std::collections::HashSet::new();
        for name in requested {
            let names: &[&str] = match name.as_str() {
                "Read" => &["file_read", "extension_resource"],
                "Write" => &["file_write"],
                "Edit" => &["file_edit"],
                "Glob" => &["find_files", "files_list"],
                "Grep" => &["search_project"],
                "Bash" => &["shell_command", "run_command"],
                name if self
                    .definitions
                    .iter()
                    .any(|definition| definition.name == name) =>
                {
                    &[name]
                }
                _ => bail!(
                    "Unsupported child tool constraint: {name}. Use an exact host tool name or supported alias; parameter constraints are not widened."
                ),
            };
            allowed.extend(names.iter().map(|name| name.to_string()));
        }
        self.definitions
            .retain(|definition| allowed.contains(&definition.name));
        self.advertised
            .retain(|definition| allowed.contains(&definition.name));
        self.allowed_tools = Some(allowed);
        Ok(())
    }
    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub fn definitions(&self) -> &[ToolDefinition] {
        &self.definitions
    }
    /// The permission mode in force right now.
    pub fn mode(&self) -> PermissionMode {
        self.session.permissions().get()
    }
    /// Whether the current mode lets the model use `definition` at all.
    fn usable(&self, definition: &ToolDefinition) -> bool {
        self.mode().allows_writes() || !definition.writes
    }
    /// Initial context and memory tools share one lazy resident embedding model.
    pub(crate) fn project_facts(&self) -> Result<Vec<MemoryRecord>> {
        self.memory.recall_sync(
            &self.request.workspace.to_string_lossy(),
            &self.request.prompt,
            8,
        )
    }
    /// The compact skills index for the system prompt: one `name: description`
    /// line per built-in skill, installed portable skill, local plugin
    /// component and relevant learned procedure. Bodies, resources and
    /// procedure steps load on demand through `extension_skill`,
    /// `extension_resource` and `skill_read`. Retrieval is advisory: it never
    /// executes a learned step or relaxes action permissions.
    pub(crate) fn project_skill_context(&self) -> Result<String> {
        let mut entries: Vec<(String, String)> = prompts::bundled_skills()
            .iter()
            .map(|skill| (skill.name.to_owned(), skill.description.to_owned()))
            .collect();
        entries.extend(portable::index(self)?);
        let learning = LearningService::new(self.store.clone());
        let learned = learning.relevant_skills(
            &self.request.workspace.to_string_lossy(),
            &self.request.prompt,
            2,
        )?;
        entries.extend(learned.iter().map(|skill| {
            (
                skill.skill.name.clone(),
                format!(
                    "Learned procedure: {}. Read its steps with skill_read before reusing them",
                    skill.title
                ),
            )
        }));
        if !learned.is_empty() {
            learning.record_retrieval(self.request.session_id, self.request.turn_id, &learned)?;
        }
        Ok(prompts::skills_index(entries.iter().map(
            |(name, description)| (name.as_str(), description.as_str()),
        )))
    }
    pub fn set_delegate(&self, handler: Delegate) {
        *self.delegate.lock() = Some(handler);
    }
    /// The tools to offer the model directly under the current permission
    /// mode: a stable core set plus capabilities the user configured
    /// (browser, local inference, speech, a saved image provider and existing
    /// connectors). Call it again before each model request so a mode change
    /// is reflected; everything else stays discoverable through `search_tools`.
    pub fn initial_definitions(&self) -> Vec<ToolDefinition> {
        let config = &self.capability_config;
        let has_connectors = self
            .advertised
            .iter()
            .any(|definition| definition.name == "connector_call");
        let configured = |name: &str| {
            (name.starts_with("browser_") && config.chromium.is_some())
                || (name.starts_with("local_inference_") && config.llama.is_some())
                || (name.starts_with("speech_")
                    && (config.whisper.is_some() || config.sherpa.is_some()))
                || name == "image_generate"
                || (name.starts_with("connector_") && has_connectors)
        };
        self.advertised
            .iter()
            .filter(|definition| {
                self.usable(definition)
                    && (CORE_TOOLS.contains(&definition.name.as_str())
                        || configured(&definition.name))
            })
            .cloned()
            .collect()
    }

    pub(crate) fn journal_key(namespace: Option<&str>, call_id: &str) -> String {
        match namespace {
            None => format!("root:{call_id}"),
            Some(ns) => format!("child:{}:{ns}:{call_id}", ns.len()),
        }
    }
    pub(crate) fn journaled(&self, call_id: &str) -> Result<Option<JournalAction>> {
        self.store.action_for_key(
            self.request.session_id,
            self.request.turn_id,
            &Self::journal_key(None, call_id),
        )
    }
    pub(crate) fn operator_reviewed_action_keys(&self) -> Result<Vec<(Uuid, String)>> {
        self.store
            .operator_reviewed_action_keys(self.request.session_id, self.request.turn_id)
    }
    /// Called from a blocking tool/restore context. Native receipt lookup only
    /// observes an exact previous execution; it cannot dispatch a new command.
    pub(crate) fn recover_journaled(&self, call_id: &str) -> Result<Option<JournalAction>> {
        self.journaled(call_id)?
            .map(|action| self.recover_native_action(action))
            .transpose()
    }
    pub(crate) fn recover_native_action(&self, action: JournalAction) -> Result<JournalAction> {
        if action.state != "outcome_unknown" || action.intent.name != "environment_execute" {
            return Ok(action);
        }
        let _lease = self.store.lock_action_execution(action.intent.id)?;
        let Some(observation) = self.runtime.block_on(self.capabilities.recover_execution(
            &action.intent.name,
            &action.intent.arguments,
            self.cancel.child_token(),
        ))?
        else {
            return Ok(action);
        };
        let result = serde_json::to_value(observation)?;
        let success = !observation_failed(&result);
        let evidence = "Exact native environment operation identity, arguments and terminal receipt verified without command dispatch";
        if success {
            self.store
                .reconcile_action(action.intent.id, Some(&result), evidence)?;
        } else {
            self.store
                .reconcile_action_failure_with_result(action.intent.id, &result, evidence)?;
        }
        let presented = self.bound_result(action.intent.id, result);
        self.emit(EngineEvent::ToolFinished {
            action_id: action.intent.id,
            name: action.intent.name,
            result: serde_json::to_string(&presented)?,
            success,
            parent_id: self.parent_id,
        })?;
        self.store
            .action(action.intent.id)?
            .context("Recovered action disappeared")
    }
    pub fn execute(&self, call_id: &str, name: &str, arguments: Value) -> Result<String> {
        self.execute_scoped(None, call_id, name, arguments, Origin::Model)
    }
    /// A child has a unique stable parent-call namespace and no delegation entry,
    /// including through call_tool. It still shares policy and the durable journal.
    pub(crate) fn execute_child(
        &self,
        namespace: &str,
        call_id: &str,
        name: &str,
        arguments: Value,
    ) -> Result<String> {
        self.execute_scoped(Some(namespace), call_id, name, arguments, Origin::Model)
    }
    fn execute_scoped(
        &self,
        namespace: Option<&str>,
        call_id: &str,
        name: &str,
        arguments: Value,
        origin: Origin,
    ) -> Result<String> {
        ensure!(
            namespace.is_none() || name != "agent_delegate",
            "Child agents cannot delegate further"
        );
        if name == "call_tool" {
            let target = arguments
                .get("name")
                .and_then(Value::as_str)
                .context("Missing tool name")?;
            ensure!(
                target != "call_tool",
                "Recursive tool dispatch is not allowed"
            );
            return self.execute_scoped(
                namespace,
                call_id,
                target,
                arguments
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({})),
                origin,
            );
        }
        ensure!(
            self.allowed_tools
                .as_ref()
                .is_none_or(|allowed| allowed.contains(name)),
            "This child is not authorized to call tool '{name}'"
        );
        let key = Self::journal_key(namespace, call_id);
        if let Some(action) =
            self.store
                .action_for_key(self.request.session_id, self.request.turn_id, &key)?
        {
            return self.replay(action, name, &arguments);
        }
        ensure!(
            !self.cancel.is_cancelled(),
            "Task cancelled before tool dispatch"
        );
        // Use one immutable skill snapshot for classification, approval and
        // execution. A package update must not add executable commands after
        // a read-only classification or replace code under an old approval.
        // Built-in skills are compiled in and never run commands.
        let skill_snapshot = if name == "extension_skill" && portable::bundled(&arguments).is_none()
        {
            Some(portable::load(self, &arguments)?)
        } else {
            None
        };
        let mut definition = self.effective_definition(name, &arguments)?;
        if let Some(skill) = &skill_snapshot {
            definition.writes = !skill.dynamic_commands.is_empty();
        }
        ensure!(
            !(name.starts_with("plugin_") && arguments.get("capabilities").is_some()),
            "Plugins run only with the grants reviewed in Settings; remove `capabilities`"
        );
        let reach = self.reach(name, &arguments);
        let network = if matches!(name, "run_command" | "shell_command") {
            builtin::network_request(&arguments)?
        } else {
            None
        };
        let mode = self.mode();
        let call = Call {
            name,
            arguments: &arguments,
            writes: definition.writes,
            origin,
            reach: &reach,
            workspace: &self.request.workspace,
            network,
        };
        let verdict = permissions::decide(mode, &call);
        let rule = permissions::approval_rule(name, &arguments, &reach, &self.request.workspace);
        let ask = match verdict {
            Verdict::Refuse(message) => bail!(message),
            Verdict::Allow => None,
            Verdict::Ask(reason) => {
                let remembered = rule
                    .as_ref()
                    .is_some_and(|(key, _)| self.rules.lock().contains(key));
                (!remembered).then_some(reason)
            }
        };
        if name == "image_generate" {
            self.image_generation.validate(&arguments)?;
        }
        if name == "environment_execute" {
            // Reject malformed fresh requests before an approval or running
            // intent exists. Cached identities above remain authoritative.
            let _: xylo_core::EnvironmentExecuteRequest = serde_json::from_value(arguments.clone())
                .context("Invalid environment command request")?;
        }
        let access = builtin::Access {
            outside: matches!(reach, Reach::Outside(_)),
            sandboxed: mode.sandboxed(),
            network: network.is_some(),
        };
        let component_snapshot = if name == "extension_component" && definition.writes {
            Some(
                portable::service(self).component_status(
                    arguments["id"].as_str().context("Extension id required")?,
                    arguments["component"]
                        .as_str()
                        .context("Component required")?,
                )?,
            )
        } else {
            None
        };
        // Held from preview to write: a concurrent edit of the same file waits
        // and then plans against the content this one leaves behind.
        let _edit_lock = if matches!(name, "file_write" | "file_edit") {
            let target = builtin::write_target(self, builtin::text(&arguments, "path")?, access)?;
            Some(builtin::EditLock::acquire(&target, &self.cancel)?)
        } else {
            None
        };
        let presentation = self.presentation(
            name,
            &arguments,
            access,
            skill_snapshot.as_ref(),
            component_snapshot.as_ref(),
        )?;
        let id = Uuid::new_v4();
        // Native clients can close inherited descriptors and outlive a crashed
        // worker. Only proven in-process writes qualify for generic review.
        let qualified = matches!(
            name,
            "file_write" | "file_edit" | "memory_remember" | "skill_propose"
        );
        let execution_lease = qualified
            .then(|| self.store.action_execution_lease(id, true))
            .transpose()?;
        let _action_scope = execution_lease
            .as_ref()
            .map(|lease| xylo_resources::ActionLeaseScope::enter(lease.file()));
        let intent = ActionIntent {
            id,
            session_id: self.request.session_id,
            turn_id: self.request.turn_id,
            name: name.into(),
            arguments: arguments.clone(),
            writes: definition.writes,
            idempotency_key: key.clone(),
        };
        match self.store.begin_action(&intent)? {
            ActionDecision::Execute => {}
            ActionDecision::Reuse(result) => {
                let previous = self
                    .store
                    .action_for_key(self.request.session_id, self.request.turn_id, &key)?
                    .context("Reused action has no journal record")?;
                return Ok(serde_json::to_string(
                    &self.model_result(previous.intent.id, result),
                )?);
            }
            ActionDecision::InFlight => bail!("Action is already executing"),
            ActionDecision::OutcomeUnknown => bail!("Action outcome is unknown"),
        }
        if let Some(snapshot) = &skill_snapshot {
            self.store.record_extension_action_binding(
                id,
                &json!({"kind":"skill","snapshot":snapshot}),
            )?;
        }
        if let Some(snapshot) = &component_snapshot {
            self.store.record_extension_action_binding(
                id,
                &json!({"kind":"component","snapshot":snapshot}),
            )?;
        }
        if let Err(error) = self.emit(EngineEvent::ToolStarted {
            action_id: id,
            name: name.into(),
            arguments: presentation.clone(),
            parent_id: self.parent_id,
        }) {
            let _ = self.store.deny_action(id);
            return Err(error);
        }
        if let Some(reason) = ask {
            let refusal = if self.await_approval(id, name, presentation, reason, rule)? {
                // The mode may have changed while the user decided; an
                // approval never outranks a mode that now refuses the action.
                let current = self.mode();
                matches!(permissions::decide(current, &call), Verdict::Refuse(_))
                    .then(|| format!("Permission mode changed to {}; not run", current.label()))
            } else {
                Some("Action denied or task cancelled".to_owned())
            };
            if let Some(message) = refusal {
                self.store.deny_action(id)?;
                let _ = self.emit(EngineEvent::ToolFinished {
                    action_id: id,
                    name: name.into(),
                    result: message.clone(),
                    success: false,
                    parent_id: self.parent_id,
                });
                bail!(message)
            }
        }
        if self.cancel.is_cancelled() {
            self.store.deny_action(id)?;
            bail!("Task cancelled before execution")
        }
        self.store.mark_action_running(id)?;
        // Hooks wrap the model's calls, never their own executions.
        let hooked = origin == Origin::Model && name != "extension_component";
        // All fallible execution branches stay inside this closure, so no `?`
        // can strand a running action without recording failure/uncertainty.
        let execution = (|| -> Result<Value> {
            if hooked {
                self.run_event_hooks(namespace, call_id, "PreToolUse", name, &arguments, None)
                    .map_err(|error| {
                        code::unapplied(format!(
                            "Pre-tool hook prevented this operation: {error:#}"
                        ))
                    })?;
            }
            let result = if name == "search_tools" {
                self.search_tools(namespace, &arguments)
            } else if name == "agent_delegate" {
                // End the registration guard before the blocking callback;
                // admission and resource leases govern parallel child work.
                let handler =
                    { self.delegate.lock().clone() }.context("Subagent executor is unavailable")?;
                handler(&DelegateCall {
                    parent_key: key.clone(),
                    parent_action_id: id,
                    arguments: arguments.clone(),
                })?
            } else if name.starts_with("connector_") {
                self.execute_connector(name, &arguments)?
            } else if name == "image_generate" {
                image_generation::execute(self, id, &arguments)?
            } else if matches!(
                name,
                "extension_skill" | "extension_resource" | "extension_component"
            ) {
                portable::execute(
                    self,
                    name,
                    &arguments,
                    &key,
                    id,
                    component_snapshot.as_ref(),
                    skill_snapshot.as_ref(),
                )?
            } else if builtin::handles(name) {
                builtin::execute(self, name, &arguments, access)?
            } else {
                let operation = async {
                    let _reservation = if let Some(demand) = xylo_resources::capability_demand(name)
                    {
                        Some(
                            self.resources
                                .acquire(demand, self.cancel.child_token())
                                .await?,
                        )
                    } else {
                        None
                    };
                    // Capability execution owns the cross-process reservation;
                    // compiler and artifact closures retain it through cleanup.
                    self.capabilities
                        .execute(name, &arguments, self.cancel.child_token())
                        .await
                };
                let result = match &execution_lease {
                    Some(lease) => self
                        .runtime
                        .block_on(xylo_resources::scope_action(lease.file(), operation)),
                    None => self.runtime.block_on(operation),
                }?;
                serde_json::to_value(result)?
            };
            ensure_observation_success(&result)?;
            Ok(result)
        })();
        match execution {
            Ok(result) => {
                // Record the actual operation result BEFORE fallible artifact/UI work.
                // If persistence fails after a write, retain uncertainty, never replay it.
                if let Err(error) = self.store.finish_action(id, &result) {
                    if definition.writes {
                        let _ = self.store.mark_action_unknown(
                            id,
                            "Operation returned a result but result persistence failed",
                        );
                    }
                    return Err(error)
                        .context("Tool completed but durable result could not be confirmed");
                }
                let mut result = result;
                if hooked
                    && let Err(error) = self.run_event_hooks(
                        namespace,
                        call_id,
                        "PostToolUse",
                        name,
                        &arguments,
                        Some(&result),
                    )
                {
                    let message = format!("Tool completed; post-tool hook failed: {error:#}");
                    if let Some(fields) = result.as_object_mut() {
                        fields.insert("post_tool_hook_error".into(), json!(message));
                    } else {
                        result = json!({"status":"success","summary":result,"post_tool_hook_error":message});
                    }
                }
                let model = self.model_result(id, result.clone());
                let presented = self.presented_result(id, name, result);
                self.emit(EngineEvent::ToolFinished {
                    action_id: id,
                    name: name.into(),
                    result: serde_json::to_string(&presented)?,
                    success: true,
                    parent_id: self.parent_id,
                })?;
                Ok(serde_json::to_string(&model)?)
            }
            Err(error) => {
                if let Some(observation) = error.downcast_ref::<ObservedFailure>() {
                    let result = &observation.0;
                    let message = "Tool returned an unsuccessful or uncertain result";
                    // A local process that exited with a code ran to completion:
                    // its failure is an observed fact, not an uncertain write.
                    let completed_process = matches!(name, "run_command" | "shell_command")
                        && result.get("exit_code").is_some_and(Value::is_i64);
                    let unknown = (definition.writes && !completed_process)
                        || result.get("status").and_then(Value::as_str) == Some("outcome_unknown");
                    if unknown {
                        self.store
                            .mark_action_unknown_with_result(id, result, message)?;
                    } else {
                        self.store.fail_action_with_result(id, result, message)?;
                    }
                    let model = self.model_result(id, result.clone());
                    let presented = self.presented_result(id, name, result.clone());
                    let _ = self.emit(EngineEvent::ToolFinished {
                        action_id: id,
                        name: name.into(),
                        result: serde_json::to_string(&presented)?,
                        success: false,
                        parent_id: self.parent_id,
                    });
                    return Err(ObservedFailure(model).into());
                }
                let message = format!("{error:#}");
                let definitive_rejection = error.downcast_ref::<code::Unapplied>().is_some()
                    || error
                        .downcast_ref::<xylo_runtime::process::NotStarted>()
                        .is_some()
                    || error
                        .downcast_ref::<image_generation::DefinitiveFailure>()
                        .is_some()
                    || (name == "connector_call"
                        && error
                            .downcast_ref::<xylo_services::ConnectorNotDispatched>()
                            .is_some())
                    || (name == "environment_execute"
                        && error
                            .downcast_ref::<xylo_capabilities::environments::CommandNotDispatched>()
                            .is_some());
                if definition.writes && !definitive_rejection {
                    self.store.mark_action_unknown(id, &message)?
                } else {
                    self.store.fail_action(id, &message)?
                }
                let _ = self.emit(EngineEvent::ToolFinished {
                    action_id: id,
                    name: name.into(),
                    result: message,
                    success: false,
                    parent_id: self.parent_id,
                });
                Err(error)
            }
        }
    }
    /// The model's answer for a call identity that already has a journal entry.
    fn replay(&self, action: JournalAction, name: &str, arguments: &Value) -> Result<String> {
        ensure!(
            action.intent.name == name && &action.intent.arguments == arguments,
            "Tool call identity was reused with different arguments"
        );
        let action = self.recover_native_action(action)?;
        match action.state.as_str() {
            "succeeded" => Ok(serde_json::to_string(
                &self.model_result(
                    action.intent.id,
                    action
                        .result
                        .context("Successful action has no stored result")?,
                ),
            )?),
            "outcome_unknown" => {
                bail!("Previous outcome is unknown. Reconcile the action before continuing")
            }
            "pending" | "running" => {
                bail!("Action is already pending or executing; it will not be dispatched twice")
            }
            "denied" => bail!("Action was denied"),
            "failed" => {
                if let Some(result) = action.result {
                    return Err(ObservedFailure(self.model_result(action.intent.id, result)).into());
                }
                bail!(
                    "Previous action failed: {}",
                    action.error.as_deref().unwrap_or("No details")
                )
            }
            _ => bail!("Invalid action state"),
        }
    }
    /// Where a file or search call's path points, resolved like execution
    /// will resolve it. Unresolvable paths count as project paths: execution
    /// then reports the real error, and still refuses to leave the project.
    fn reach(&self, name: &str, arguments: &Value) -> Reach {
        let (requested, writing) = match name {
            "file_read" => (arguments.get("path").and_then(Value::as_str), false),
            "file_write" | "file_edit" => (arguments.get("path").and_then(Value::as_str), true),
            "files_list" | "find_files" | "search_project" => (
                Some(arguments.get("path").and_then(Value::as_str).unwrap_or(".")),
                false,
            ),
            _ => return Reach::Project,
        };
        match requested.map(|requested| builtin::resolve(self, requested, writing)) {
            Some(Ok(builtin::Resolved::Outside(path))) => Reach::Outside(path),
            _ => Reach::Project,
        }
    }
    /// What the start and approval events show: the real change for edits,
    /// the frozen snapshot for extensions, bounded arguments otherwise.
    fn presentation(
        &self,
        name: &str,
        arguments: &Value,
        access: builtin::Access,
        skill: Option<&xylo_services::extensions::LoadedSkill>,
        component: Option<&xylo_services::extensions::ComponentRuntimeStatus>,
    ) -> Result<Value> {
        let value = match name {
            "image_generate" => self.image_generation.approval(arguments)?,
            "extension_skill" => match skill {
                Some(snapshot) => {
                    json!({"request":arguments,"package_root":snapshot.package_root,"dynamic_commands":snapshot.dynamic_commands,"fork":snapshot.fork,"agent":snapshot.agent,"requested_model":snapshot.model,"allowed_tools":snapshot.allowed_tools})
                }
                None => arguments.clone(),
            },
            "extension_component" => match component {
                Some(snapshot) => {
                    json!({"request":arguments,"digest":snapshot.digest,"workspace":snapshot.setup.workspace,"allowed_domains":snapshot.setup.allowed_domains,"environment_names":snapshot.setup.environment.keys().chain(snapshot.setup.credential_refs.keys()).collect::<Vec<_>>(),"timeout_seconds":snapshot.setup.timeout_seconds})
                }
                None => arguments.clone(),
            },
            // Show the actual change, and reject an inapplicable edit before
            // any intent or approval exists.
            "file_write" | "file_edit" => {
                return builtin::change_preview(self, name, arguments, access);
            }
            // Installed plugins show their reviewed grants and budgets.
            _ => self
                .runtime
                .block_on(self.capabilities.approval_presentation(
                    name,
                    arguments,
                    self.cancel.child_token(),
                ))?
                .unwrap_or_else(|| arguments.clone()),
        };
        Ok(bound_presentation(value))
    }
    /// Emits the approval request and waits for the decision or cancellation.
    fn await_approval(
        &self,
        id: Uuid,
        name: &str,
        presentation: Value,
        reason: String,
        rule: Option<(String, String)>,
    ) -> Result<bool> {
        let (tx, rx) = mpsc::sync_channel(1);
        let (rule_key, label) = rule.unzip();
        let approvals = self.session.approvals();
        approvals.install(
            id,
            tx,
            rule_key.clone().map(|key| (key, self.rules.clone())),
        );
        let request = self
            .emit(EngineEvent::Phase {
                phase: TurnPhase::WaitingForApproval,
            })
            .and_then(|()| {
                self.emit(EngineEvent::ApprovalRequired {
                    action_id: id,
                    name: name.into(),
                    arguments: presentation,
                    rule: label,
                    rule_key,
                    reason: Some(reason),
                    parent_id: self.parent_id,
                })
            });
        if let Err(error) = request {
            approvals.withdraw(id);
            let _ = self.store.deny_action(id);
            return Err(error);
        }
        let allowed = loop {
            if self.cancel.is_cancelled() {
                break false;
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(allow) => break allow,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => break false,
            }
        };
        approvals.withdraw(id);
        if !approvals.waiting() {
            let phase = if self.parent_id.is_some() {
                TurnPhase::Delegating
            } else {
                TurnPhase::RunningTools
            };
            let _ = self.emit(EngineEvent::Phase { phase });
        }
        Ok(allowed && !self.cancel.is_cancelled())
    }
    fn search_tools(&self, namespace: Option<&str>, arguments: &Value) -> Value {
        let query = arguments
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_lowercase();
        let words: Vec<_> = query.split_whitespace().collect();
        let matches: Vec<_> = self
            .advertised
            .iter()
            .filter(|d| {
                self.usable(d)
                    && (namespace.is_none() || d.name != "agent_delegate")
                    && words.iter().any(|word| {
                        d.name.contains(word) || d.description.to_lowercase().contains(word)
                    })
            })
            .take(12)
            .collect();
        json!({"status":"success","summary":matches,"next_actions":["Use call_tool with the selected tool name and documented arguments"],"artifacts":[]})
    }
    fn run_event_hooks(
        &self,
        namespace: Option<&str>,
        call_id: &str,
        event: &str,
        tool_name: &str,
        arguments: &Value,
        result: Option<&Value>,
    ) -> Result<()> {
        let service = portable::service(self);
        let input = json!({"event":event,"hook_event_name":event,"tool_name":tool_name,"tool_input":arguments,"tool_response":result,"session_id":self.request.session_id,"cwd":self.request.workspace});
        let hooks = service.matching_hooks(event, &input, &self.request.workspace)?;
        if hooks.is_empty() {
            return Ok(());
        }
        ensure!(
            !self
                .store
                .unresolved_actions(self.request.session_id)?
                .iter()
                .any(|action| action.state == "outcome_unknown"
                    && action.intent.name == "extension_component"
                    && action
                        .intent
                        .arguments
                        .get("operation")
                        .and_then(Value::as_str)
                        == Some("event")),
            "A previous hook outcome is unknown; reconcile its action before further hook execution"
        );
        for (index, hook) in hooks.into_iter().enumerate() {
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
                .and_then(|action| action.result)
                .context("Hook outcome was not recorded")?;
            for entry in actual
                .get("results")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                ensure!(
                    entry
                        .pointer("/hook_control/blocked")
                        .and_then(Value::as_bool)
                        != Some(true),
                    "Configured hook blocked this operation or requested separate approval"
                );
                let output = &entry["output"];
                ensure!(
                    output.pointer("/hookSpecificOutput/updatedInput").is_none()
                        && output.get("updatedInput").is_none()
                        && output
                            .pointer("/hookSpecificOutput/updatedToolInput")
                            .is_none()
                        && output
                            .pointer("/hookSpecificOutput/updatedMCPToolOutput")
                            .is_none(),
                    "Unsupported hook execution-changing directive; updatedInput/output replacement requires host validation and renewed approval"
                );
                let decision = output
                    .pointer("/hookSpecificOutput/permissionDecision")
                    .or_else(|| output.get("decision"))
                    .and_then(Value::as_str);
                ensure!(
                    output.get("continue").and_then(Value::as_bool) != Some(false)
                        && decision.is_none_or(|decision| matches!(decision, "allow" | "approve")),
                    "Configured hook blocked this operation or requested separate approval"
                );
            }
        }
        Ok(())
    }
    fn connector(&self, args: &Value) -> Result<ConnectorConfig> {
        let id = Uuid::parse_str(
            args.get("connector_id")
                .and_then(Value::as_str)
                .context("connector_id is required")?,
        )?;
        self.store
            .connectors()?
            .into_iter()
            .find(|c| c.id == id)
            .context("Connector is not configured")
    }
    fn effective_definition(&self, name: &str, args: &Value) -> Result<ToolDefinition> {
        let mut definition = self
            .definitions
            .iter()
            .find(|d| d.name == name)
            .cloned()
            .context("Unknown tool")?;
        if name == "connector_call" {
            let config = self.connector(args)?;
            let tool = args
                .get("tool")
                .and_then(Value::as_str)
                .context("Connector tool name is required")?;
            let tools = if is_mcp(&config) {
                self.connectors.cached_tools(&config)?
            } else {
                NativeConnectors::tools(&config.transport)?
            };
            let target = tools
                .into_iter()
                .find(|d| d.name == tool)
                .context("Connector tool not discovered; call connector_tools first")?;
            if !is_mcp(&config) {
                NativeConnectors::validate_arguments(
                    &target,
                    &args.get("arguments").cloned().unwrap_or_else(|| json!({})),
                )?;
            }
            // MCP servers mark read-only tools with annotations; the
            // connector layer already treats anything else as a write.
            definition.writes = target.writes;
        } else if name == "connector_tools" {
            definition.writes = self.connector(args)?.transport == "stdio";
        } else if name == "extension_component" {
            definition.writes = args.get("operation").and_then(Value::as_str) != Some("status");
        }
        Ok(definition)
    }
    fn execute_connector(&self, name: &str, args: &Value) -> Result<Value> {
        if name == "connector_list" {
            return Ok(
                json!({"status":"success","summary":self.store.connectors()?,"artifacts":[]}),
            );
        }
        let config = self.connector(args)?;
        if name == "connector_tools" {
            let tools = if is_mcp(&config) {
                self.runtime
                    .block_on(self.connectors.tools(&config, &self.cancel))?
            } else {
                NativeConnectors::tools(&config.transport)?
            };
            return Ok(json!({"status":"success","summary":tools,"artifacts":[]}));
        }
        ensure!(name == "connector_call", "Unknown connector operation");
        let tool = args
            .get("tool")
            .and_then(Value::as_str)
            .context("Connector tool name is required")?;
        let arguments = args.get("arguments").cloned().unwrap_or_else(|| json!({}));
        let result = if is_mcp(&config) {
            self.runtime
                .block_on(self.connectors.call(&config, tool, arguments, &self.cancel))?
        } else {
            self.runtime
                .block_on(self.native.call(&config, tool, arguments, &self.cancel))?
        };
        Ok(json!({"status":"success","summary":result,"artifacts":[]}))
    }
    fn emit(&self, event: EngineEvent) -> Result<()> {
        self.events
            .blocking_send(event)
            .context("Task event consumer disconnected")
    }
    pub(crate) fn bound_result(&self, id: Uuid, result: Value) -> Value {
        // Keep the event itself valid and small. Truncating an encoded JSON
        // string at the UI boundary destroys both status and artifact paths.
        const PRESENTATION_BYTES: usize = 8192;
        let status = result
            .get("status")
            .and_then(Value::as_str)
            .filter(|status| status.len() <= 64)
            .map(|status| json!(status))
            .unwrap_or_else(|| {
                if observation_failed(&result) {
                    json!("failed")
                } else {
                    json!("success")
                }
            });
        let encoded = match serde_json::to_vec(&result) {
            Ok(v) => v,
            Err(error) => {
                return json!({"status":status,"summary":"Tool result is stored in the action journal","action_id":id,"presentation_error":error.to_string()});
            }
        };
        if encoded.len() <= PRESENTATION_BYTES {
            return result;
        }
        let write = self.persist_full(id, &encoded);
        let mut references = Vec::new();
        if let Ok(path) = &write {
            references.push(serde_json::to_value(path).unwrap_or(Value::Null));
        }
        let mut presented = json!({"status":status,"summary":"Tool result recorded. Open the attached result or action journal for details.",
            "action_id":id,"next_actions":["Inspect the full result before retrying an unsuccessful or uncertain action"],
            "artifacts":[],"truncated":true});
        if let Err(error) = write {
            presented["presentation_error"] =
                Value::String(error.to_string().chars().take(256).collect());
        }
        let all_paths = result
            .get("artifacts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut omitted = 0;
        for reference in all_paths {
            if !reference.is_string() || references.contains(&reference) {
                continue;
            }
            references.push(reference);
            presented["artifacts"] = json!(references);
            if serde_json::to_vec(&presented)
                .map_or(true, |bytes| bytes.len() > PRESENTATION_BYTES - 128)
            {
                references.pop();
                omitted += 1;
            }
        }
        presented["artifacts"] = json!(references);
        if omitted > 0 {
            presented["artifact_references_in_full_result"] = json!(omitted);
        }
        presented
    }
    /// The `ToolFinished` presentation. A delegate's result is the child's
    /// final answer, so the UI receives it at the model's bound rather than
    /// the small placeholder used for other large results.
    fn presented_result(&self, id: Uuid, name: &str, result: Value) -> Value {
        if name == "agent_delegate" {
            self.model_result(id, result)
        } else {
            self.bound_result(id, result)
        }
    }
    /// Saves the complete observation once; a replay must find identical bytes.
    fn persist_full(&self, id: Uuid, encoded: &[u8]) -> Result<PathBuf> {
        self.persist_once(&format!("tool-{id}.json"), encoded)
    }
    fn persist_once(&self, name: &str, encoded: &[u8]) -> Result<PathBuf> {
        use std::io::Write;
        let path = self.artifacts.join(name);
        if let Ok(metadata) = std::fs::symlink_metadata(&path) {
            ensure!(
                metadata.is_file()
                    && !metadata.file_type().is_symlink()
                    && metadata.len() == encoded.len() as u64
                    && std::fs::read(&path)? == encoded,
                "Stored tool presentation differs from the action journal"
            );
            return Ok(path);
        }
        let mut staging = tempfile::NamedTempFile::new_in(&self.artifacts)?;
        staging.write_all(encoded)?;
        staging.as_file().sync_all()?;
        staging
            .persist_noclobber(&path)
            .map_err(|error| error.error)?;
        Ok(path)
    }
    /// What the model observes. Unlike the small UI presentation it keeps the
    /// actual content; oversized results keep both ends of every long field and
    /// point at the saved complete result, which file_read can page through.
    pub(crate) fn model_result(&self, id: Uuid, result: Value) -> Value {
        const MODEL_BYTES: usize = 64 * 1024;
        let Ok(encoded) = serde_json::to_vec(&result) else {
            return self.bound_result(id, result);
        };
        if encoded.len() <= MODEL_BYTES {
            return result;
        }
        let mut shortened = result;
        for limit in [16_384, 8192, 4096, 2048, 1024, 512, 256] {
            shorten_strings(&mut shortened, limit);
            if serde_json::to_vec(&shortened)
                .map_or(true, |bytes| bytes.len() <= MODEL_BYTES - 1024)
            {
                break;
            }
        }
        if serde_json::to_vec(&shortened).map_or(true, |bytes| bytes.len() > MODEL_BYTES) {
            let status = shortened.get("status").cloned().unwrap_or(json!("success"));
            let text = serde_json::to_string(&shortened).unwrap_or_default();
            shortened = json!({"status":status,"summary":excerpt(&text, MODEL_BYTES / 2)});
        }
        if let Some(object) = shortened.as_object_mut() {
            object.insert("truncated".into(), json!(true));
            object.insert(
                "note".into(),
                json!(format!(
                    "Result was {} bytes; long fields were shortened in the middle. Page through full_result with file_read offset/limit for the omitted part.",
                    encoded.len()
                )),
            );
            // A text rendering keeps long outputs as real lines, so the model
            // can page through them with file_read offset/limit.
            let readable = readable_text(&serde_json::from_slice(&encoded).unwrap_or(Value::Null));
            match self.persist_once(&format!("tool-{id}.txt"), readable.as_bytes()) {
                Ok(path) => {
                    object.insert("full_result".into(), json!(path));
                }
                Err(error) => {
                    object.insert(
                        "presentation_error".into(),
                        json!(error.to_string().chars().take(256).collect::<String>()),
                    );
                }
            }
        }
        shortened
    }
    /// Stops this turn's capability helpers. MCP connections belong to the
    /// worker's [`ToolSession`] and stay open for later turns.
    pub async fn shutdown(&self) -> Result<()> {
        self.capabilities.shutdown().await
    }
}
/// Keeps start and approval events far below the protocol line limit; the
/// journal and the tool itself always receive the complete arguments.
fn bound_presentation(value: Value) -> Value {
    match serde_json::to_vec(&value) {
        Ok(encoded) if encoded.len() > PRESENTATION_EVENT_BYTES => {
            let mut shortened = value;
            for limit in [16_384, 4096, 1024, 256] {
                shorten_strings(&mut shortened, limit);
                if serde_json::to_vec(&shortened)
                    .is_ok_and(|bytes| bytes.len() <= PRESENTATION_EVENT_BYTES)
                {
                    return json!({"request":shortened,"request_shortened":true});
                }
            }
            json!({"request_shortened":true,"summary":excerpt(&String::from_utf8_lossy(&encoded), PRESENTATION_EVENT_BYTES / 2)})
        }
        _ => value,
    }
}
fn is_mcp(config: &ConnectorConfig) -> bool {
    matches!(
        config.transport.as_str(),
        "stdio" | "http" | "streamable_http"
    )
}
/// Flattens a result into labelled sections with long strings written verbatim.
fn readable_text(value: &Value) -> String {
    fn walk(value: &Value, path: &str, output: &mut String) {
        match value {
            Value::Object(map) => {
                for (key, item) in map {
                    let path = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    walk(item, &path, output);
                }
            }
            Value::Array(items) if items.iter().all(|item| !item.is_object()) => {
                output.push_str(&format!("## {path}\n"));
                for item in items {
                    match item {
                        Value::String(text) => output.push_str(text),
                        other => output.push_str(&other.to_string()),
                    }
                    output.push('\n');
                }
            }
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    walk(item, &format!("{path}[{index}]"), output);
                }
            }
            Value::String(text) if text.contains('\n') || text.len() > 120 => {
                output.push_str(&format!("## {path}\n{text}"));
                if !text.ends_with('\n') {
                    output.push('\n');
                }
            }
            other => {
                let shown = match other {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                output.push_str(&format!("{path}: {shown}\n"));
            }
        }
    }
    let mut output = String::new();
    walk(value, "", &mut output);
    output
}
/// UTF-8-safe head and tail with an explicit omission marker.
fn excerpt(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_owned();
    }
    const MARKER: &str = "\n[… middle omitted; see full_result …]\n";
    let keep = budget.saturating_sub(MARKER.len()) / 2;
    let mut head = keep.min(text.len());
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len().saturating_sub(keep);
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!("{}{MARKER}{}", &text[..head], &text[tail..])
}
fn shorten_strings(value: &mut Value, limit: usize) {
    match value {
        Value::String(text) if text.len() > limit => *text = excerpt(text, limit),
        Value::Array(items) => {
            if items.len() > 400 {
                let omitted = items.len() - 400;
                items.truncate(400);
                items.push(json!(format!(
                    "[… {omitted} more items omitted; see full_result …]"
                )));
            }
            items
                .iter_mut()
                .for_each(|item| shorten_strings(item, limit));
        }
        Value::Object(map) => map
            .values_mut()
            .for_each(|item| shorten_strings(item, limit)),
        _ => {}
    }
}
#[derive(Debug)]
struct ObservedFailure(Value);
impl std::fmt::Display for ObservedFailure {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(output, "Tool reported an unsuccessful result: {}", self.0)
    }
}
impl std::error::Error for ObservedFailure {}

fn observation_failed(value: &Value) -> bool {
    value.get("finished_naturally").and_then(Value::as_bool) == Some(false)
        || value.get("success").and_then(Value::as_bool) == Some(false)
        || value.get("isError").and_then(Value::as_bool) == Some(true)
        || value
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| {
                matches!(
                    status,
                    "error"
                        | "failed"
                        | "failure"
                        | "incomplete"
                        | "cancelled"
                        | "timeout"
                        | "outcome_unknown"
                )
            })
}
fn ensure_observation_success(value: &Value) -> Result<()> {
    if observation_failed(value) {
        return Err(ObservedFailure(value.clone()).into());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
