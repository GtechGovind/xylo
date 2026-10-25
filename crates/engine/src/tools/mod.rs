//! Application-owned durable tool and permission boundary. External tool metadata
//! cannot supply its own permission flag; native classifications come from code.
mod builtin;
mod image_generation;
use anyhow::{Context, Result, bail, ensure};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, mpsc},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workbench_capabilities::{CapabilityConfig, CapabilityService};
use workbench_core::{
    ConnectorConfig, EngineEvent, MemoryRecord, PermissionMode, ToolDefinition, TurnRequest,
};
use workbench_services::{
    ActionDecision, ActionIntent, ConnectorBroker, CredentialVault, JournalAction, LearningService,
    NativeConnectors, SqliteMemory, Store,
};

type Delegate = Arc<dyn Fn(&str, Value) -> Result<Value> + Send + Sync>;
/// Approval channels are installed before emitting a request and authorize one action.
#[derive(Clone, Default)]
pub struct ApprovalGate(Arc<Mutex<HashMap<Uuid, mpsc::SyncSender<bool>>>>);
impl ApprovalGate {
    pub fn decide(&self, id: Uuid, allow: bool) -> Result<()> {
        self.0
            .lock()
            .remove(&id)
            .context("Approval is no longer pending")?
            .send(allow)
            .context("Task stopped before receiving the decision")
    }
}
pub struct ToolBroker {
    request: TurnRequest,
    store: Store,
    memory: SqliteMemory,
    capabilities: CapabilityService,
    definitions: Vec<ToolDefinition>,
    approvals: ApprovalGate,
    events: tokio::sync::mpsc::Sender<EngineEvent>,
    cancel: CancellationToken,
    artifacts: PathBuf,
    runtime: tokio::runtime::Handle,
    resources: workbench_runtime::resources::ResourceManager,
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
        approvals: ApprovalGate,
        events: tokio::sync::mpsc::Sender<EngineEvent>,
        cancel: CancellationToken,
    ) -> Result<Self> {
        let artifacts = config.artifacts.clone();
        let capabilities = CapabilityService::new(config)?;
        let image_generation = image_generation::Configuration::load(&artifacts);
        let mut definitions = builtin::definitions();
        definitions.push(image_generation.definition());
        definitions.extend(capabilities.tool_definitions());
        let vault = CredentialVault::new(workbench_core::CREDENTIAL_SERVICE)?;
        Ok(Self {
            memory: SqliteMemory::configured(
                store.clone(),
                &artifacts
                    .parent()
                    .context("Artifacts directory has no application data parent")?
                    .join("embedding.json"),
            )?,
            request,
            connectors: ConnectorBroker::new(store.clone(), vault.clone()),
            native: NativeConnectors::new(vault)?,
            image_generation,
            store,
            capabilities,
            definitions,
            approvals,
            events,
            cancel,
            artifacts,
            runtime: tokio::runtime::Handle::current(),
            resources: workbench_runtime::resources::ResourceManager::new(),
            delegate: Mutex::new(None),
        })
    }
    pub fn definitions(&self) -> &[ToolDefinition] {
        &self.definitions
    }
    /// Initial context and memory tools share one lazy resident embedding model.
    pub(crate) fn project_facts(&self) -> Result<Vec<MemoryRecord>> {
        self.memory.recall_sync(
            &self.request.workspace.to_string_lossy(),
            &self.request.prompt,
            8,
        )
    }
    /// Retrieval is advisory context. Promotion/evaluation stays on host maintenance;
    /// this method never executes a learned step or relaxes action permissions.
    pub(crate) fn project_skill_context(&self) -> Result<String> {
        let learning = LearningService::new(self.store.clone());
        let skills = learning.relevant_skills(
            &self.request.workspace.to_string_lossy(),
            &self.request.prompt,
            2,
        )?;
        let mut context = String::new();
        let mut included = Vec::new();
        for skill in skills {
            let mut section = format!(
                "Skill {} version {}: {}\n",
                skill.skill.name, skill.skill.version, skill.title
            );
            if let Some(guidance) = &skill.correction_guidance
                && guidance.len() <= 512
            {
                section.push_str(&format!("User correction: {guidance}\n"));
            }
            for (index, step) in skill.steps.iter().enumerate() {
                let line = format!(
                    "{}. {} {}{}\n",
                    index + 1,
                    step.tool,
                    step.arguments,
                    if step.requires_approval {
                        " (current approval required)"
                    } else {
                        ""
                    }
                );
                if section.len() + line.len() > 1600 {
                    section.push_str("Read skill_read for remaining steps.\n");
                    break;
                }
                section.push_str(&line);
            }
            if context.len() + section.len() > 2400 {
                break;
            }
            context.push_str(&section);
            included.push(skill);
        }
        if !included.is_empty() {
            learning.record_retrieval(self.request.session_id, self.request.turn_id, &included)?;
        }
        Ok(context)
    }
    pub fn set_delegate(&self, handler: Delegate) {
        *self.delegate.lock() = Some(handler);
    }
    pub fn initial_definitions(&self) -> Vec<ToolDefinition> {
        let always = [
            "file_read",
            "file_write",
            "files_list",
            "search_project",
            "run_command",
            "search_tools",
            "call_tool",
        ];
        let prompt = self.request.prompt.to_lowercase();
        self.definitions
            .iter()
            .filter(|d| {
                always.contains(&d.name.as_str())
                    || d.name
                        .split('_')
                        .next()
                        .is_some_and(|prefix| prompt.contains(prefix))
            })
            .take(18)
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
        })?;
        self.store
            .action(action.intent.id)?
            .context("Recovered action disappeared")
    }
    pub fn execute(&self, call_id: &str, name: &str, arguments: Value) -> Result<String> {
        self.execute_scoped(None, call_id, name, arguments)
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
        self.execute_scoped(Some(namespace), call_id, name, arguments)
    }
    fn execute_scoped(
        &self,
        namespace: Option<&str>,
        call_id: &str,
        name: &str,
        arguments: Value,
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
            );
        }
        let key = Self::journal_key(namespace, call_id);
        if let Some(action) =
            self.store
                .action_for_key(self.request.session_id, self.request.turn_id, &key)?
        {
            ensure!(
                action.intent.name == name && action.intent.arguments == arguments,
                "Tool call identity was reused with different arguments"
            );
            let action = self.recover_native_action(action)?;
            match action.state.as_str() {
                "succeeded" => {
                    return Ok(serde_json::to_string(
                        &self.bound_result(
                            action.intent.id,
                            action
                                .result
                                .context("Successful action has no stored result")?,
                        ),
                    )?);
                }
                "outcome_unknown" => {
                    bail!("Previous outcome is unknown. Reconcile the action before continuing")
                }
                "pending" | "running" => {
                    bail!("Action is already pending or executing; it will not be dispatched twice")
                }
                "denied" => bail!("Action was denied"),
                "failed" => {
                    if let Some(result) = action.result {
                        return Err(
                            ObservedFailure(self.bound_result(action.intent.id, result)).into()
                        );
                    }
                    bail!(
                        "Previous action failed: {}",
                        action.error.as_deref().unwrap_or("No details")
                    )
                }
                _ => bail!("Invalid action state"),
            }
        }
        ensure!(
            !self.cancel.is_cancelled(),
            "Task cancelled before tool dispatch"
        );
        let definition = self.effective_definition(name, &arguments)?;
        ensure!(
            self.request.permissions != PermissionMode::ReadOnly || !definition.writes,
            "{name} requires write permission; this task is read-only"
        );
        if name == "image_generate" {
            self.image_generation.validate(&arguments)?;
        }
        if name == "environment_execute" {
            // Reject malformed fresh requests before an approval or running
            // intent exists. Cached identities above remain authoritative.
            let _: workbench_core::EnvironmentExecuteRequest =
                serde_json::from_value(arguments.clone())
                    .context("Invalid environment command request")?;
        }
        let presentation = if name == "image_generate" {
            self.image_generation.approval(&arguments)?
        } else {
            arguments.clone()
        };
        let id = Uuid::new_v4();
        // Native clients can close inherited descriptors and outlive a crashed
        // worker. Only proven in-process writes qualify for generic review.
        let qualified = matches!(name, "file_write" | "memory_remember" | "skill_propose");
        let execution_lease = qualified
            .then(|| self.store.action_execution_lease(id, true))
            .transpose()?;
        let _action_scope = execution_lease
            .as_ref()
            .map(|lease| workbench_runtime::resources::ActionLeaseScope::enter(lease.file()));
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
                    &self.bound_result(previous.intent.id, result),
                )?);
            }
            ActionDecision::InFlight => bail!("Action is already executing"),
            ActionDecision::OutcomeUnknown => bail!("Action outcome is unknown"),
        }
        if let Err(error) = self.emit(EngineEvent::ToolStarted {
            action_id: id,
            name: name.into(),
            arguments: presentation.clone(),
        }) {
            let _ = self.store.deny_action(id);
            return Err(error);
        }
        if definition.writes {
            let (tx, rx) = mpsc::sync_channel(1);
            self.approvals.0.lock().insert(id, tx);
            if let Err(error) = self.emit(EngineEvent::ApprovalRequired {
                action_id: id,
                name: name.into(),
                arguments: presentation.clone(),
            }) {
                self.approvals.0.lock().remove(&id);
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
            self.approvals.0.lock().remove(&id);
            if !allowed || self.cancel.is_cancelled() {
                self.store.deny_action(id)?;
                let _ = self.emit(EngineEvent::ToolFinished {
                    action_id: id,
                    name: name.into(),
                    result: "Action denied or task cancelled".into(),
                    success: false,
                });
                bail!("Action denied or task cancelled")
            }
        }
        if self.cancel.is_cancelled() {
            self.store.deny_action(id)?;
            bail!("Task cancelled before execution")
        }
        self.store.mark_action_running(id)?;
        // All fallible execution branches stay inside this closure, so no `?`
        // can strand a running action without recording failure/uncertainty.
        let execution = (|| -> Result<Value> {
            let result = if name == "search_tools" {
                let query = arguments
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_lowercase();
                let words: Vec<_> = query.split_whitespace().collect();
                let matches: Vec<_> = self
                    .definitions
                    .iter()
                    .filter(|d| {
                        (namespace.is_none() || d.name != "agent_delegate")
                            && words.iter().any(|word| {
                                d.name.contains(word) || d.description.to_lowercase().contains(word)
                            })
                    })
                    .take(12)
                    .collect();
                json!({"status":"success","summary":matches,"next_actions":["Use call_tool with the selected tool name and documented arguments"],"artifacts":[]})
            } else if name == "agent_delegate" {
                self.delegate
                    .lock()
                    .clone()
                    .context("Subagent executor is unavailable")?(
                    &key, arguments.clone()
                )?
            } else if name.starts_with("connector_") {
                self.execute_connector(name, &arguments)?
            } else if name == "image_generate" {
                image_generation::execute(self, id, &arguments)?
            } else if builtin::handles(name) {
                builtin::execute(self, name, &arguments)?
            } else {
                let operation = async {
                    let _reservation = if let Some(demand) =
                        workbench_runtime::resources::capability_demand(name)
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
                    Some(lease) => {
                        self.runtime
                            .block_on(workbench_runtime::resources::scope_action(
                                lease.file(),
                                operation,
                            ))
                    }
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
                let presented = self.bound_result(id, result);
                self.emit(EngineEvent::ToolFinished {
                    action_id: id,
                    name: name.into(),
                    result: serde_json::to_string(&presented)?,
                    success: true,
                })?;
                Ok(serde_json::to_string(&presented)?)
            }
            Err(error) => {
                if let Some(observation) = error.downcast_ref::<ObservedFailure>() {
                    let result = &observation.0;
                    let message = "Tool returned an unsuccessful or uncertain result";
                    let unknown = definition.writes
                        || result.get("status").and_then(Value::as_str) == Some("outcome_unknown");
                    if unknown {
                        self.store
                            .mark_action_unknown_with_result(id, result, message)?;
                    } else {
                        self.store.fail_action_with_result(id, result, message)?;
                    }
                    let presented = self.bound_result(id, result.clone());
                    let _ = self.emit(EngineEvent::ToolFinished {
                        action_id: id,
                        name: name.into(),
                        result: serde_json::to_string(&presented)?,
                        success: false,
                    });
                    return Err(ObservedFailure(presented).into());
                }
                let message = format!("{error:#}");
                let definitive_rejection = error
                    .downcast_ref::<image_generation::DefinitiveFailure>()
                    .is_some()
                    || (name == "connector_call"
                        && error
                            .downcast_ref::<workbench_services::ConnectorNotDispatched>()
                            .is_some())
                    || (name == "environment_execute"
                        && error
                            .downcast_ref::<workbench_capabilities::environments::CommandNotDispatched>()
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
                });
                Err(error)
            }
        }
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
            definition.writes = target.writes;
        } else if name == "connector_tools" {
            definition.writes = self.connector(args)?.transport == "stdio";
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
        let path = self.artifacts.join(format!("tool-{id}.json"));
        let write = (|| -> Result<()> {
            use std::io::Write;
            if let Ok(metadata) = std::fs::symlink_metadata(&path) {
                ensure!(
                    metadata.is_file()
                        && !metadata.file_type().is_symlink()
                        && metadata.len() == encoded.len() as u64
                        && std::fs::read(&path)? == encoded,
                    "Stored tool presentation differs from the action journal"
                );
                return Ok(());
            }
            let mut staging = tempfile::NamedTempFile::new_in(&self.artifacts)?;
            staging.write_all(&encoded)?;
            staging.as_file().sync_all()?;
            staging
                .persist_noclobber(&path)
                .map_err(|error| error.error)?;
            Ok(())
        })();
        let mut references = Vec::new();
        if write.is_ok() {
            references.push(serde_json::to_value(&path).unwrap_or(Value::Null));
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
    pub async fn shutdown(&self) -> Result<()> {
        self.capabilities.shutdown().await?;
        self.connectors.reap_idle(Duration::ZERO).await;
        Ok(())
    }
}
fn is_mcp(config: &ConnectorConfig) -> bool {
    matches!(
        config.transport.as_str(),
        "stdio" | "http" | "streamable_http"
    )
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
                    "error" | "failed" | "failure" | "cancelled" | "timeout" | "outcome_unknown"
                )
            })
}
fn ensure_observation_success(value: &Value) -> Result<()> {
    if observation_failed(value) {
        return Err(ObservedFailure(value.clone()).into());
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod action_review_tests;
#[cfg(test)]
mod environment_recovery_tests;
#[cfg(test)]
mod native_connector_dispatch_tests;
#[cfg(test)]
mod tests;
