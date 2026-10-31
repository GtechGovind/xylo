//! Explicit, version-bound execution setup for local portable components.
//! Import and discovery never spawn commands or look up credentials.
use super::*;
use crate::{ConnectorBroker, ConnectorLaunchScope, CredentialVault, Store};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ComponentSetup {
    pub workspace: PathBuf,
    /// Explicit non-secret values. Secret-bearing names require vault references.
    pub environment: BTreeMap<String, String>,
    pub credential_refs: BTreeMap<String, String>,
    pub allowed_domains: Vec<String>,
    /// Hooks run automatically only for events explicitly enabled here.
    pub enabled_events: Vec<String>,
    pub timeout_seconds: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuntimeEntryStatus {
    pub name: String,
    pub readiness: Readiness,
    pub requirements: Vec<String>,
    pub evidence: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ComponentRuntimeStatus {
    pub package: String,
    pub digest: String,
    pub component: String,
    pub readiness: Readiness,
    pub entries: Vec<RuntimeEntryStatus>,
    pub setup: ComponentSetup,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HookInvocation {
    pub package: String,
    pub component: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedSetup {
    digest: String,
    setup: ComponentSetup,
    evidence: BTreeMap<String, String>,
}
#[derive(Clone)]
struct Entry {
    name: String,
    event: Option<String>,
    matcher: Option<String>,
    declaration: Value,
}
struct Plan {
    package: ExtensionPackage,
    component: ExtensionComponent,
    root: PathBuf,
    protected_paths: Vec<PathBuf>,
    setup: ComponentSetup,
    entries: Vec<Entry>,
}

impl ExtensionService {
    /// Caller holds the catalog lock. Revoke grants before uninstall commits so
    /// partial failures cannot resurrect credentials/events on reinstallation.
    pub(super) fn revoke_runtime_setup_under_lock(&self, package: &str) -> Result<()> {
        let mut setups = self.read_runtime()?;
        let previous = setups.len();
        let prefix = format!("{package}:");
        setups.retain(|key, _| !key.starts_with(&prefix));
        if setups.len() != previous {
            self.save_runtime(&setups)?;
        }
        Ok(())
    }
    fn runtime_path(&self) -> PathBuf {
        self.root.join("runtime.json")
    }
    /// Caller holds the catalog lock.
    pub(super) fn remove_unreadable_runtime(&self) -> Result<()> {
        let path = self.runtime_path();
        if super::catalog::decodes::<BTreeMap<String, SavedSetup>>(&path)? == Some(false) {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }
    /// Saved setups; an unreadable file reads as no setup (the next locked
    /// operation deletes it).
    fn read_runtime(&self) -> Result<BTreeMap<String, SavedSetup>> {
        match std::fs::read(self.runtime_path()) {
            Ok(bytes) => {
                ensure!(bytes.len() <= 1024 * 1024, "Runtime setup exceeds 1 MiB");
                Ok(serde_json::from_slice(&bytes).unwrap_or_default())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(e.into()),
        }
    }
    fn save_runtime(&self, setups: &BTreeMap<String, SavedSetup>) -> Result<()> {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        file.write_all(&serde_json::to_vec_pretty(setups)?)?;
        file.as_file().sync_all()?;
        file.persist(self.runtime_path())?;
        Ok(())
    }
    fn plan(&self, package: &str, component: &str, setup: ComponentSetup) -> Result<Plan> {
        let package = self
            .list()?
            .into_iter()
            .find(|p| p.id == package && p.enabled)
            .context("Extension is disabled or removed")?;
        let component = package
            .components
            .iter()
            .find(|c| c.name == component)
            .context("Component missing")?
            .clone();
        ensure!(
            matches!(
                component.kind,
                ComponentKind::Hooks | ComponentKind::Mcp | ComponentKind::LanguageServer
            ),
            "This component has no local executable adapter"
        );
        let root = self.package_root(&package)?.canonicalize()?;
        let declaration = component_declaration(&root, &component.path)?;
        let entries = entries(&component.kind, declaration)?;
        ensure!(!entries.is_empty(), "No executable entries declared");
        Ok(Plan {
            package,
            component,
            root,
            protected_paths: self
                .root
                .parent()
                .map(|path| vec![path.to_owned()])
                .unwrap_or_default(),
            setup,
            entries,
        })
    }
    pub fn component_setup(
        &self,
        package: &str,
        component: &str,
        mut setup: ComponentSetup,
    ) -> Result<ComponentRuntimeStatus> {
        validate_setup(&setup)?;
        setup.workspace = setup
            .workspace
            .canonicalize()
            .context("Choose an existing workspace for this component")?;
        ensure!(
            setup.workspace.is_dir(),
            "Component workspace must be a directory"
        );
        let plan = self.plan(package, component, setup.clone())?;
        let _lock = self.lock()?;
        let mut saved = self.read_runtime()?;
        saved.insert(
            format!("{package}:{component}"),
            SavedSetup {
                digest: plan.package.digest.clone(),
                setup,
                evidence: BTreeMap::new(),
            },
        );
        self.save_runtime(&saved)?;
        drop(_lock);
        let status = status(&plan, &BTreeMap::new());
        self.record_setup_state(
            package,
            &plan.package.digest,
            component,
            status.readiness.clone(),
            status
                .entries
                .iter()
                .flat_map(|entry| entry.requirements.clone())
                .collect(),
        )?;
        Ok(status)
    }
    pub fn component_status(
        &self,
        package: &str,
        component: &str,
    ) -> Result<ComponentRuntimeStatus> {
        let saved = self
            .read_runtime()?
            .get(&format!("{package}:{component}"))
            .cloned()
            .context("Component needs explicit execution setup")?;
        let plan = self.plan(package, component, saved.setup)?;
        ensure!(
            saved.digest == plan.package.digest,
            "Extension changed; review and configure its new version"
        );
        Ok(status(&plan, &saved.evidence))
    }
    /// The caller must authorize/journal mutations before invoking this method.
    // The engine and host call these with positional arguments; a parameter
    // struct would change their call sites for no behavioral gain.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_component(
        &self,
        package: &str,
        component: &str,
        operation: &str,
        input: Value,
        store: Store,
        vault: CredentialVault,
        cancel: CancellationToken,
    ) -> Result<Value> {
        self.execute_component_inner(
            package, component, operation, input, store, vault, cancel, None,
        )
        .await
    }
    /// Bind an approval to the exact immutable package and reviewed setup.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_component_bound(
        &self,
        package: &str,
        component: &str,
        operation: &str,
        input: Value,
        store: Store,
        vault: CredentialVault,
        cancel: CancellationToken,
        binding: &ComponentRuntimeStatus,
    ) -> Result<Value> {
        self.execute_component_inner(
            package,
            component,
            operation,
            input,
            store,
            vault,
            cancel,
            Some(binding),
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    async fn execute_component_inner(
        &self,
        package: &str,
        component: &str,
        operation: &str,
        input: Value,
        store: Store,
        vault: CredentialVault,
        cancel: CancellationToken,
        binding: Option<&ComponentRuntimeStatus>,
    ) -> Result<Value> {
        ensure!(
            !cancel.is_cancelled(),
            "Component execution cancelled before dispatch"
        );
        let saved = self
            .read_runtime()?
            .get(&format!("{package}:{component}"))
            .cloned()
            .context("Component needs explicit execution setup")?;
        let plan = self.plan(package, component, saved.setup.clone())?;
        ensure!(
            saved.digest == plan.package.digest,
            "Extension changed; review and configure its new version"
        );
        if let Some(binding) = binding {
            ensure!(
                binding.package == package
                    && binding.component == component
                    && binding.digest == plan.package.digest
                    && binding.setup == saved.setup,
                "Component changed since approval; review its setup again"
            );
        }
        validate_setup(&plan.setup)?;
        let selected = input.get("entry").and_then(Value::as_str);
        ensure!(
            operation == "verify"
                || selected.is_some()
                || plan.component.kind == ComponentKind::Hooks,
            "Choose a component entry"
        );
        let mut outputs = Vec::new();
        let mut verified = BTreeMap::new();
        for entry in &plan.entries {
            if selected.is_some_and(|name| name != entry.name) || !delivered(entry) {
                continue;
            }
            if plan.component.kind == ComponentKind::Hooks && operation != "verify" {
                let event = input
                    .get("event")
                    .and_then(Value::as_str)
                    .context("Hook event required")?;
                if entry.event.as_deref() != Some(event)
                    || !hook_matches(entry.matcher.as_deref(), &input)
                {
                    continue;
                }
                ensure!(
                    plan.setup.enabled_events.iter().any(|e| e == event),
                    "This hook event has not been enabled in execution setup"
                );
            }
            let requirements = entry_requirements(&plan, entry);
            ensure!(
                requirements.is_empty(),
                "Component needs setup: {}",
                requirements.join("; ")
            );
            let environment = resolve_environment(&plan, entry, &vault).await?;
            ensure!(
                !cancel.is_cancelled(),
                "Component execution cancelled before dispatch"
            );
            let secrets = plan
                .setup
                .credential_refs
                .keys()
                .filter_map(|name| environment.get(name).cloned())
                .collect::<Vec<_>>();
            let result = match plan.component.kind {
                ComponentKind::Mcp => {
                    run_mcp(
                        &plan,
                        entry,
                        operation,
                        &input,
                        environment,
                        store.clone(),
                        vault.clone(),
                        cancel.clone(),
                    )
                    .await
                }
                ComponentKind::Hooks => {
                    let mut hook_input = input.clone();
                    let fields = hook_input
                        .as_object_mut()
                        .context("Hook input must be an object")?;
                    fields.entry("event").or_insert_with(|| json!(entry.event));
                    fields
                        .entry("hook_event_name")
                        .or_insert_with(|| json!(entry.event));
                    fields
                        .entry("cwd")
                        .or_insert_with(|| json!(plan.setup.workspace));
                    run_hook(&plan, entry, &hook_input, environment, cancel.clone()).await
                }
                ComponentKind::LanguageServer => {
                    run_lsp(&plan, entry, operation, &input, environment, cancel.clone()).await
                }
                _ => unreachable!(),
            };
            let mut result = result
                .map_err(|error| anyhow::anyhow!(redacted(&format!("{error:#}"), &secrets)))?;
            // Policy control is extracted before data redaction. Credentials
            // that equal a policy word/key must never erase a hook denial.
            let hook_control = if plan.component.kind == ComponentKind::Hooks {
                let decision = result
                    .pointer("/hookSpecificOutput/permissionDecision")
                    .or_else(|| result.get("decision"))
                    .and_then(Value::as_str);
                Some(
                    json!({"blocked":result.get("continue").and_then(Value::as_bool)==Some(false)||decision.is_some_and(|decision|!matches!(decision,"allow"|"approve"))}),
                )
            } else {
                None
            };
            redact_value(&mut result, &secrets);
            verified.insert(
                entry.name.clone(),
                format!(
                    "{} completed for entry {}",
                    if operation == "verify" {
                        "Protocol execution verification"
                    } else {
                        "Execution"
                    },
                    entry.name
                ),
            );
            outputs.push(json!({"entry":entry.name,"output":result,"hook_control":hook_control}));
        }
        ensure!(!outputs.is_empty(), "No matching executable entry");
        let state = {
            let _lock = self.lock()?;
            let mut packages = self.read_catalog()?;
            let package_state = packages
                .iter_mut()
                .find(|entry| {
                    entry.id == package && entry.digest == plan.package.digest && entry.enabled
                })
                .context("Extension disabled, removed, or changed during execution")?;
            let mut setups = self.read_runtime()?;
            let current = setups
                .get_mut(&format!("{package}:{component}"))
                .context("Component setup removed during execution")?;
            ensure!(
                current.digest == plan.package.digest && current.setup == saved.setup,
                "Component changed during execution"
            );
            current.evidence.extend(verified);
            let state = status(&plan, &current.evidence);
            if state.readiness == Readiness::ExecutionVerified {
                let component = package_state
                    .components
                    .iter_mut()
                    .find(|entry| entry.name == component)
                    .context("Component removed during execution")?;
                component.readiness = Readiness::ExecutionVerified;
                component.evidence = Some(format!(
                    "{} entries executed in the configured sandbox",
                    state.entries.len()
                ));
                component.requirements.clear();
            }
            self.save_runtime(&setups)?;
            self.save(&packages)?;
            state
        };
        Ok(json!({"status":state,"results":outputs}))
    }
    /// Returns only explicitly enabled, workspace-matching hook invocations.
    /// This performs discovery only; the host must journal/authorize each result.
    pub fn matching_hooks(
        &self,
        event: &str,
        input: &Value,
        workspace: &Path,
    ) -> Result<Vec<HookInvocation>> {
        ensure!(supported_event(event), "Unsupported hook event");
        let workspace = workspace.canonicalize()?;
        let mut output = Vec::new();
        for package in self.list()?.into_iter().filter(|package| package.enabled) {
            for component in package
                .components
                .iter()
                .filter(|component| component.kind == ComponentKind::Hooks)
            {
                let Ok(status) = self.component_status(&package.id, &component.name) else {
                    continue;
                };
                if status.setup.workspace != workspace
                    || !status
                        .setup
                        .enabled_events
                        .iter()
                        .any(|enabled| enabled == event)
                {
                    continue;
                }
                let plan = self.plan(&package.id, &component.name, status.setup)?;
                if plan.entries.iter().any(|entry| {
                    entry.event.as_deref() == Some(event)
                        && hook_matches(entry.matcher.as_deref(), input)
                }) {
                    output.push(HookInvocation {
                        package: package.id.clone(),
                        component: component.name.clone(),
                    });
                }
            }
        }
        Ok(output)
    }
    /// Explicit configured lifecycle hooks; caller owns policy and durable intent.
    pub async fn run_hooks(
        &self,
        event: &str,
        input: Value,
        workspace: &Path,
        store: Store,
        vault: CredentialVault,
        cancel: CancellationToken,
    ) -> Result<Vec<Value>> {
        ensure!(supported_event(event), "Unsupported hook event");
        let workspace = workspace.canonicalize()?;
        let mut output = Vec::new();
        for package in self.list()?.into_iter().filter(|p| p.enabled) {
            for component in package
                .components
                .iter()
                .filter(|c| c.kind == ComponentKind::Hooks)
            {
                let Ok(status) = self.component_status(&package.id, &component.name) else {
                    continue;
                };
                if status.setup.workspace != workspace
                    || !status.setup.enabled_events.iter().any(|e| e == event)
                {
                    continue;
                }
                let mut input = input.clone();
                input
                    .as_object_mut()
                    .context("Hook input must be an object")?
                    .insert("event".into(), json!(event));
                // Nonmatching hook events are ordinary no-ops, not verified execution.
                let plan = self.plan(&package.id, &component.name, status.setup.clone())?;
                if !plan.entries.iter().any(|e| {
                    e.event.as_deref() == Some(event) && hook_matches(e.matcher.as_deref(), &input)
                }) {
                    continue;
                }
                output.push(
                    self.execute_component(
                        &package.id,
                        &component.name,
                        "event",
                        input,
                        store.clone(),
                        vault.clone(),
                        cancel.clone(),
                    )
                    .await?,
                );
            }
        }
        Ok(output)
    }
}
/// Hook events Xylo delivers. Entries for any other event (for example
/// `SessionStart`) are reported as unsupported individually and never run;
/// they do not make the rest of the component unsupported.
fn supported_event(event: &str) -> bool {
    matches!(event, "PreToolUse" | "PostToolUse")
}
/// Whether an entry can ever run: non-hook entries always, hook entries only
/// for a delivered event.
fn delivered(entry: &Entry) -> bool {
    entry.event.as_deref().is_none_or(supported_event)
}
fn validate_setup(setup: &ComponentSetup) -> Result<()> {
    ensure!(
        setup.timeout_seconds <= 300,
        "Component timeout cannot exceed 300 seconds"
    );
    ensure!(
        setup.environment.len() + setup.credential_refs.len() <= 64,
        "Too many environment variables"
    );
    for (name, value) in &setup.environment {
        validate_env_name(name)?;
        ensure!(
            value.len() <= 8192 && !value.contains('\0'),
            "Invalid environment value"
        );
        ensure!(
            !secret_name(name),
            "Store {name} in the credential vault and provide a reference"
        );
    }
    for (name, reference) in &setup.credential_refs {
        validate_env_name(name)?;
        ensure!(
            !reference.trim().is_empty() && reference.len() <= 256,
            "Invalid credential reference"
        );
    }
    for domain in &setup.allowed_domains {
        ensure!(
            !domain.is_empty()
                && domain.len() <= 253
                && (domain.parse::<std::net::IpAddr>().is_ok()
                    || domain.split('.').all(|label| !label.is_empty()
                        && label.len() <= 63
                        && !label.starts_with('-')
                        && !label.ends_with('-')
                        && label
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'))),
            "Use exact allowed network hostnames"
        );
    }
    ensure!(
        setup.enabled_events.iter().all(|e| supported_event(e)),
        "Unsupported hook lifecycle event"
    );
    Ok(())
}
fn validate_env_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 128
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            && name != "XYLO_SANDBOX_CHILD_ENV",
        "Unsupported environment variable name"
    );
    Ok(())
}
fn secret_name(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    ["TOKEN", "SECRET", "PASSWORD", "API_KEY", "PRIVATE_KEY"]
        .iter()
        .any(|part| n.contains(part))
}
fn component_declaration(root: &Path, path: &str) -> Result<Value> {
    let (path, key) = path
        .split_once('#')
        .map_or((path, None), |(p, k)| (p, Some(k)));
    let path = super::manifest::safe_path(root, path)?;
    ensure!(
        std::fs::metadata(&path)?.len() <= 1024 * 1024,
        "Executable declaration exceeds 1 MiB"
    );
    let mut value: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    if let Some(key) = key {
        value = value
            .get(key)
            .cloned()
            .context("Declared component missing")?;
    }
    Ok(value)
}
fn entries(kind: &ComponentKind, declaration: Value) -> Result<Vec<Entry>> {
    let mut output = Vec::new();
    match kind {
        ComponentKind::Mcp | ComponentKind::LanguageServer => {
            let key = if *kind == ComponentKind::Mcp {
                "mcpServers"
            } else {
                "lspServers"
            };
            let map = declaration
                .get(key)
                .unwrap_or(&declaration)
                .as_object()
                .context("Servers must be an object")?;
            for (name, value) in map {
                output.push(Entry {
                    name: name.clone(),
                    event: None,
                    matcher: None,
                    declaration: value.clone(),
                });
            }
        }
        ComponentKind::Hooks => {
            let map = declaration
                .get("hooks")
                .unwrap_or(&declaration)
                .as_object()
                .context("Hooks must be an event object")?;
            for (event, groups) in map {
                for (index, group) in groups
                    .as_array()
                    .context("Hook event must contain an array")?
                    .iter()
                    .enumerate()
                {
                    let commands = group
                        .get("hooks")
                        .and_then(Value::as_array)
                        .map(|v| v.as_slice())
                        .unwrap_or_else(|| std::slice::from_ref(group));
                    for (command_index, command) in commands.iter().enumerate() {
                        output.push(Entry {
                            name: format!("{event}:{index}:{command_index}"),
                            event: Some(event.clone()),
                            matcher: group
                                .get("matcher")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            declaration: command.clone(),
                        });
                    }
                }
            }
        }
        _ => bail!("Unsupported executable component"),
    }
    ensure!(output.len() <= 128, "Too many executable entries");
    Ok(output)
}
fn entry_requirements(plan: &Plan, entry: &Entry) -> Vec<String> {
    let mut required = Vec::new();
    if !cfg!(any(target_os = "macos", target_os = "linux")) {
        required.push("Unsupported managed sandbox platform".into());
    }
    if crate::sandbox::helper_path()
        .ok()
        .and_then(|path| path.parent().map(Path::to_owned))
        .is_none_or(|directory| {
            !directory.join("child-entry.mjs").is_file()
                || !directory
                    .join("node_modules/@anthropic-ai/sandbox-runtime/package.json")
                    .is_file()
        })
    {
        required.push("Install the pinned managed sandbox runtime dependencies".into());
    }
    if let Some(event) = entry.event.as_deref().filter(|e| !supported_event(e)) {
        required.push(format!(
            "Unsupported hook event {event}; Xylo delivers only PreToolUse and PostToolUse"
        ));
    }
    if entry.matcher.as_deref().is_some_and(|m| {
        m.chars()
            .any(|c| !c.is_ascii_alphanumeric() && !matches!(c, '_' | '|' | '*'))
    }) {
        required.push("Unsupported hook matcher; use exact tool names separated by | or *".into());
    }
    if plan.component.kind == ComponentKind::Hooks
        && entry
            .declaration
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("command")
            != "command"
    {
        required.push("Only local command hooks are supported".into());
    }
    if plan.component.kind == ComponentKind::Mcp && entry.declaration.get("url").is_some() {
        required.push("Hosted plugin MCP requires explicit native connector setup".into());
    }
    if plan.component.kind == ComponentKind::LanguageServer
        && entry
            .declaration
            .get("transport")
            .and_then(Value::as_str)
            .is_some_and(|v| v != "stdio")
    {
        required.push("Only stdio LSP is supported".into());
    }
    if let Err(error) = command(plan, entry) {
        required.push(error.to_string());
    }
    if let Some(env) = entry.declaration.get("env").and_then(Value::as_object) {
        for (name, value) in env {
            if value
                .as_str()
                .is_none_or(|v| v.contains("${") || v.contains('$'))
                && !plan.setup.environment.contains_key(name)
                && !plan.setup.credential_refs.contains_key(name)
            {
                required.push(format!(
                    "Configure explicit environment or credential reference for {name}"
                ));
            } else if secret_name(name) && !plan.setup.credential_refs.contains_key(name) {
                required.push(format!("Configure credential reference for {name}"));
            }
        }
    }
    required
}
fn status(plan: &Plan, evidence: &BTreeMap<String, String>) -> ComponentRuntimeStatus {
    let entries = plan
        .entries
        .iter()
        .map(|entry| {
            let requirements = entry_requirements(plan, entry);
            let readiness = if requirements.iter().any(|r| {
                r.starts_with("Unsupported") || r.starts_with("Only ") || r.starts_with("Hosted")
            }) {
                Readiness::UnsupportedRequirement
            } else if !requirements.is_empty() {
                Readiness::NeedsSetup
            } else if evidence.contains_key(&entry.name) {
                Readiness::ExecutionVerified
            } else {
                Readiness::Installed
            };
            RuntimeEntryStatus {
                name: entry.name.clone(),
                readiness,
                requirements,
                evidence: evidence.get(&entry.name).cloned(),
            }
        })
        .collect::<Vec<_>>();
    // Entries for events Xylo never delivers stay listed with their reason
    // but do not decide the component's readiness.
    let runnable = plan
        .entries
        .iter()
        .zip(&entries)
        .filter(|(entry, _)| delivered(entry))
        .map(|(_, status)| status)
        .collect::<Vec<_>>();
    let readiness = if runnable.is_empty()
        || runnable
            .iter()
            .any(|e| e.readiness == Readiness::UnsupportedRequirement)
    {
        Readiness::UnsupportedRequirement
    } else if runnable
        .iter()
        .any(|e| e.readiness == Readiness::NeedsSetup)
    {
        Readiness::NeedsSetup
    } else if runnable
        .iter()
        .all(|e| e.readiness == Readiness::ExecutionVerified)
    {
        Readiness::ExecutionVerified
    } else {
        Readiness::Installed
    };
    ComponentRuntimeStatus {
        package: plan.package.id.clone(),
        digest: plan.package.digest.clone(),
        component: plan.component.name.clone(),
        readiness,
        entries,
        setup: plan.setup.clone(),
    }
}
fn command(plan: &Plan, entry: &Entry) -> Result<(PathBuf, Vec<String>)> {
    if plan.component.kind == ComponentKind::Hooks {
        let script = entry
            .declaration
            .get("command")
            .and_then(Value::as_str)
            .context("Hook command missing")?;
        ensure!(
            !script.trim().is_empty() && script.len() <= 64 * 1024,
            "Invalid hook command"
        );
        return Ok((PathBuf::from("/bin/sh"), vec!["-c".into(), script.into()]));
    }
    let raw = entry
        .declaration
        .get("command")
        .and_then(Value::as_str)
        .context("Executable command missing")?;
    let expanded = raw
        .replace("${CLAUDE_PLUGIN_ROOT}", &plan.root.to_string_lossy())
        .replace("${PLUGIN_ROOT}", &plan.root.to_string_lossy());
    ensure!(
        !expanded.contains('$'),
        "Executable requires explicit unresolved environment setup"
    );
    let path = PathBuf::from(&expanded);
    let resolved = if path.is_absolute() {
        path
    } else if expanded.contains('/') {
        super::manifest::safe_path(&plan.root, &expanded)?
    } else {
        std::env::var_os("PATH")
            .and_then(|paths| {
                std::env::split_paths(&paths)
                    .map(|p| p.join(&expanded))
                    .find(|p| p.is_file())
            })
            .context(format!("Install required executable: {expanded}"))?
    };
    ensure!(
        resolved.is_file(),
        "Install required executable: {expanded}"
    );
    let args = entry
        .declaration
        .get("args")
        .map(|args| {
            args.as_array()
                .context("Executable args must be an array")?
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .context("Executable args must be strings")
                        .map(|arg| {
                            arg.replace("${CLAUDE_PLUGIN_ROOT}", &plan.root.to_string_lossy())
                                .replace("${PLUGIN_ROOT}", &plan.root.to_string_lossy())
                        })
                })
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_default();
    ensure!(
        !args.iter().any(|a| a.contains("${")),
        "Executable arguments require explicit unresolved environment setup"
    );
    Ok((resolved.canonicalize()?, args))
}
async fn resolve_environment(
    plan: &Plan,
    entry: &Entry,
    vault: &CredentialVault,
) -> Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();
    if let Some(declared) = entry.declaration.get("env").and_then(Value::as_object) {
        for (name, value) in declared {
            validate_env_name(name)?;
            if let Some(value) = value
                .as_str()
                .filter(|v| !v.contains('$') && !secret_name(name))
            {
                env.insert(name.clone(), value.into());
            }
        }
    }
    env.extend(plan.setup.environment.clone());
    for (name, reference) in &plan.setup.credential_refs {
        let vault = vault.clone();
        let reference = reference.clone();
        let value = tokio::task::spawn_blocking(move || vault.get(&reference))
            .await??
            .context(format!("Credential is missing for {name}"))?;
        env.insert(name.clone(), value);
    }
    env.insert(
        "CLAUDE_PLUGIN_ROOT".into(),
        plan.root.to_string_lossy().into_owned(),
    );
    env.insert(
        "PLUGIN_ROOT".into(),
        plan.root.to_string_lossy().into_owned(),
    );
    Ok(env)
}
fn hook_matches(matcher: Option<&str>, input: &Value) -> bool {
    let Some(matcher) = matcher else { return true };
    if matcher == "*" || matcher.is_empty() {
        return true;
    }
    let name = input
        .get("tool_name")
        .or_else(|| input.get("source"))
        .or_else(|| input.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or("");
    matcher.split('|').any(|m| {
        m == name
            || matches!(
                (m, name),
                ("Bash", "shell_command" | "run_command")
                    | ("Read", "file_read")
                    | ("Write", "file_write")
                    | ("Edit", "file_edit")
                    | ("Glob", "find_files" | "files_list")
                    | ("Grep", "search_project")
            )
    })
}
fn duration(plan: &Plan) -> Duration {
    Duration::from_secs(if plan.setup.timeout_seconds == 0 {
        30
    } else {
        plan.setup.timeout_seconds.min(300)
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_mcp(
    plan: &Plan,
    entry: &Entry,
    operation: &str,
    input: &Value,
    environment: BTreeMap<String, String>,
    store: Store,
    vault: CredentialVault,
    cancel: CancellationToken,
) -> Result<Value> {
    use sha2::{Digest, Sha256};
    let (program, args) = command(plan, entry)?;
    let digest = Sha256::digest(
        format!("{}:{}:{}", plan.package.id, plan.package.digest, entry.name).as_bytes(),
    );
    let config = xylo_core::ConnectorConfig {
        id: uuid::Uuid::from_slice(&digest[..16])?,
        name: format!("{} / {}", plan.package.name, entry.name),
        transport: "stdio".into(),
        endpoint: String::new(),
        command: Some(program),
        args,
        credential_ref: None,
    };
    let broker = ConnectorBroker::new(store, vault).with_local_scope(ConnectorLaunchScope {
        workspace: plan.setup.workspace.clone(),
        read_roots: vec![plan.root.clone()],
        protected_paths: plan.protected_paths.clone(),
        allowed_domains: plan.setup.allowed_domains.clone(),
        environment,
    });
    let operation_cancel = cancel.child_token();
    let work = async {
        let result = if operation == "verify" || operation == "tools/list" {
            broker
                .tools(&config, &operation_cancel)
                .await
                .map(|tools| json!({"tools":tools}))
        } else if operation == "tools/call" {
            let name = input
                .get("tool")
                .and_then(Value::as_str)
                .context("MCP tool name required")?;
            broker
                .call(
                    &config,
                    name,
                    input.get("arguments").cloned().unwrap_or_else(|| json!({})),
                    &operation_cancel,
                )
                .await
        } else {
            bail!("Unsupported MCP operation")
        };
        let shutdown = broker.disconnect(config.id).await;
        let output = result?;
        shutdown?;
        Ok(output)
    };
    let result = tokio::select! { _ = cancel.cancelled() => Err(anyhow::anyhow!("MCP component execution cancelled; effects may be incomplete")), result = tokio::time::timeout(duration(plan), work) => result.context("MCP component deadline exceeded; effects may be incomplete").and_then(|result| result) };
    if result.is_err() {
        operation_cancel.cancel();
        broker.abort(config.id).await;
    }
    result
}
struct OwnedProcess {
    child: tokio::process::Child,
    _temporary: tempfile::TempDir,
    pid: Option<u32>,
}
impl Drop for OwnedProcess {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        let _ = self.child.start_kill();
    }
}
fn spawn_component(
    plan: &Plan,
    entry: &Entry,
    environment: BTreeMap<String, String>,
) -> Result<OwnedProcess> {
    let (program, args) = command(plan, entry)?;
    let mut prepared = crate::sandbox::prepare_with_policy(
        &program.to_string_lossy(),
        &args,
        &plan.setup.workspace,
        std::slice::from_ref(&plan.root),
        &plan.protected_paths,
        &plan.setup.allowed_domains,
    )?;
    crate::sandbox::set_child_environment(&mut prepared, &environment)?;
    prepared
        .command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    prepared.command.process_group(0);
    let child = prepared
        .command
        .spawn()
        .context("Start sandboxed extension component")?;
    let pid = child.id();
    Ok(OwnedProcess {
        child,
        _temporary: prepared.temporary,
        pid,
    })
}
async fn run_hook(
    plan: &Plan,
    entry: &Entry,
    input: &Value,
    environment: BTreeMap<String, String>,
    cancel: CancellationToken,
) -> Result<Value> {
    let bytes = serde_json::to_vec(input)?;
    ensure!(bytes.len() <= 1024 * 1024, "Hook input exceeds 1 MiB");
    ensure!(
        !cancel.is_cancelled(),
        "Component execution cancelled before process start"
    );
    let mut process = spawn_component(plan, entry, environment)?;
    let mut stdin = process.child.stdin.take().context("Hook stdin missing")?;
    let stdout = process.child.stdout.take().context("Hook stdout missing")?;
    let work = async {
        stdin.write_all(&bytes).await?;
        stdin.shutdown().await?;
        drop(stdin);
        let mut output = Vec::new();
        stdout
            .take(1024 * 1024 + 1)
            .read_to_end(&mut output)
            .await?;
        ensure!(output.len() <= 1024 * 1024, "Hook output exceeds 1 MiB");
        let status = process.child.wait().await?;
        if status.code() == Some(2) {
            return Ok(json!({"hookSpecificOutput":{"permissionDecision":"deny"},"exit_code":2}));
        }
        ensure!(
            status.success(),
            "Hook command failed (exit {:?}); inspect component dependencies and setup",
            status.code()
        );
        let text = String::from_utf8(output).context("Hook output is not UTF-8")?;
        let output: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({"text":text}));
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
        Ok::<Value, anyhow::Error>(output)
    };
    tokio::select! {_=cancel.cancelled()=>bail!("Hook execution cancelled; effects may be incomplete"),result=tokio::time::timeout(duration(plan),work)=>result.context("Hook execution timed out; effects may be incomplete")?}
}
async fn write_lsp(stdin: &mut tokio::process::ChildStdin, message: Value) -> Result<()> {
    let bytes = serde_json::to_vec(&message)?;
    ensure!(bytes.len() <= 1024 * 1024, "LSP request exceeds 1 MiB");
    stdin
        .write_all(format!("Content-Length: {}\r\n\r\n", bytes.len()).as_bytes())
        .await?;
    stdin.write_all(&bytes).await?;
    stdin.flush().await?;
    Ok(())
}
async fn read_lsp(
    stdout: &mut BufReader<tokio::process::ChildStdout>,
    stdin: &mut tokio::process::ChildStdin,
    id: u64,
) -> Result<Value> {
    for _ in 0..128 {
        let mut length = None;
        let mut header_bytes = 0;
        loop {
            let mut line = String::new();
            ensure!(
                (&mut *stdout)
                    .take(8193_u64.saturating_sub(header_bytes as u64))
                    .read_line(&mut line)
                    .await?
                    > 0,
                "LSP server closed before response"
            );
            header_bytes += line.len();
            ensure!(header_bytes <= 8192, "LSP headers exceed 8 KiB");
            if line == "\r\n" || line == "\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = Some(value.trim().parse::<usize>()?);
            }
        }
        let length = length.context("LSP Content-Length missing")?;
        ensure!(length <= 4 * 1024 * 1024, "LSP response exceeds 4 MiB");
        let mut bytes = vec![0; length];
        stdout.read_exact(&mut bytes).await?;
        let message: Value = serde_json::from_slice(&bytes)?;
        if message.get("id").and_then(Value::as_u64) == Some(id) {
            ensure!(
                message.get("error").is_none(),
                "LSP server returned a protocol error"
            );
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
        ensure!(
            message.get("method").is_none_or(|m| m.as_str().is_some()),
            "Malformed LSP message"
        );
        if message.get("method").is_some() && message.get("id").is_some() {
            write_lsp(stdin,json!({"jsonrpc":"2.0","id":message["id"],"error":{"code":-32601,"message":"Client method unsupported"}})).await?;
        }
    }
    bail!("LSP notification budget exceeded")
}
async fn run_lsp(
    plan: &Plan,
    entry: &Entry,
    operation: &str,
    input: &Value,
    environment: BTreeMap<String, String>,
    cancel: CancellationToken,
) -> Result<Value> {
    if operation != "verify" {
        let method = input
            .get("method")
            .and_then(Value::as_str)
            .context("LSP method required")?;
        ensure!(
            matches!(
                method,
                "textDocument/hover"
                    | "textDocument/definition"
                    | "textDocument/references"
                    | "textDocument/documentSymbol"
                    | "workspace/symbol"
                    | "textDocument/completion"
            ),
            "LSP method is not an allowed read operation"
        );
        validate_lsp_paths(
            &input.get("params").cloned().unwrap_or_else(|| json!({})),
            &plan.setup.workspace,
        )?;
    }
    ensure!(
        !cancel.is_cancelled(),
        "Component execution cancelled before process start"
    );
    let mut process = spawn_component(plan, entry, environment)?;
    let mut stdin = process.child.stdin.take().context("LSP stdin missing")?;
    let mut stdout = BufReader::new(process.child.stdout.take().context("LSP stdout missing")?);
    let work = async {
        let root = url::Url::from_directory_path(&plan.setup.workspace)
            .map_err(|_| anyhow::anyhow!("Invalid LSP workspace"))?;
        write_lsp(&mut stdin,json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"processId":null,"rootUri":root.as_str(),"workspaceFolders":[{"uri":root.as_str(),"name":"workspace"}],"capabilities":{}}})).await?;
        let initialized = read_lsp(&mut stdout, &mut stdin, 1).await?;
        write_lsp(
            &mut stdin,
            json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
        )
        .await?;
        let result = if operation == "verify" {
            json!({"initialize":initialized})
        } else {
            let method = input
                .get("method")
                .and_then(Value::as_str)
                .context("LSP method required")?;
            ensure!(
                matches!(
                    method,
                    "textDocument/hover"
                        | "textDocument/definition"
                        | "textDocument/references"
                        | "textDocument/documentSymbol"
                        | "workspace/symbol"
                        | "textDocument/completion"
                ),
                "LSP method is not an allowed read operation"
            );
            let params = input.get("params").cloned().unwrap_or_else(|| json!({}));
            validate_lsp_paths(&params, &plan.setup.workspace)?;
            write_lsp(
                &mut stdin,
                json!({"jsonrpc":"2.0","id":2,"method":method,"params":params}),
            )
            .await?;
            read_lsp(&mut stdout, &mut stdin, 2).await?
        };
        write_lsp(
            &mut stdin,
            json!({"jsonrpc":"2.0","id":3,"method":"shutdown","params":null}),
        )
        .await?;
        read_lsp(&mut stdout, &mut stdin, 3).await?;
        write_lsp(&mut stdin, json!({"jsonrpc":"2.0","method":"exit"})).await?;
        stdin.shutdown().await?;
        drop(stdin);
        let status = process.child.wait().await?;
        ensure!(
            status.success(),
            "LSP server did not shut down successfully"
        );
        Ok::<Value, anyhow::Error>(result)
    };
    tokio::select! {_=cancel.cancelled()=>bail!("LSP operation cancelled"),result=tokio::time::timeout(duration(plan),work)=>result.context("LSP operation timed out")?}
}
fn validate_lsp_paths(value: &Value, workspace: &Path) -> Result<()> {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                if key == "uri" {
                    let uri = url::Url::parse(value.as_str().context("LSP URI must be a string")?)?;
                    let path = uri
                        .to_file_path()
                        .map_err(|_| anyhow::anyhow!("LSP URI must be a local file"))?;
                    ensure!(
                        path.canonicalize()?.starts_with(workspace),
                        "LSP URI escapes the configured workspace"
                    );
                }
                validate_lsp_paths(value, workspace)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                validate_lsp_paths(value, workspace)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn redacted(text: &str, secrets: &[String]) -> String {
    let mut text = text.to_owned();
    for secret in secrets.iter().filter(|value| !value.is_empty()) {
        text = text.replace(secret, "[redacted]");
    }
    text
}
fn redact_value(value: &mut Value, secrets: &[String]) {
    match value {
        Value::String(text) => *text = redacted(text, secrets),
        Value::Array(values) => {
            for value in values {
                redact_value(value, secrets)
            }
        }
        Value::Object(fields) => {
            let old = std::mem::take(fields);
            for (key, mut value) in old {
                redact_value(&mut value, secrets);
                fields.insert(redacted(&key, secrets), value);
            }
        }
        _ => {}
    }
}
