//! Live permission presets and the approval gate.
//!
//! The mode is read from a [`PermissionCell`] at every tool call, so a change
//! the host forwards with `WorkerCommand::SetPermissions` applies to the very
//! next call of a running task. [`decide`] maps a call and the current mode to
//! a verdict following the Codex presets. Enabled plugin hooks run in every
//! mode. Otherwise:
//!
//! - **Approve for me** asks only for what [`super::risk::assess`] flags as
//!   potentially unsafe (destructive commands, secrets, auto-run project
//!   files, desktop input, browser actions, plugin code, remote environments,
//!   schedules, paid image generation), for writes outside the project, for
//!   connector calls that write or send more than a small secret-free
//!   lookup, and for network access beyond a [`super::risk::routine_fetch`]
//!   (which also asks when project configuration steers the fetch tool).
//! - **Ask for approval** runs [`SANDBOXED_COMMANDS`] offline without
//!   asking, since the sandbox confines them. For every other call it asks
//!   whenever "Approve for me" would, and also for anything outside the
//!   project, every connector call, the desktop, network access and every
//!   writing tool except the [`PROJECT_WRITES`] (file edits, memory and skill
//!   proposals). Risky in-process edits therefore ask in both presets.
//! - **Full access** asks for nothing; commands run without the sandbox.
//! - **Read only** refuses every write (network access included) and treats
//!   reads like "Ask for approval".
//!
//! Independently of the mode, file tools never write a project's `.git` or
//! its agent configuration (see `builtin::write_target`), matching the
//! command sandbox.
//!
//! "Always allow" rules are owned by the host and persisted per project. A
//! turn is seeded with them through `TurnRequest.approved_rules`; a rule the
//! user remembers during the turn is added to that turn's set, and the host
//! persists it for later turns. Nothing outlives the turn in the worker, so a
//! revoked rule stops applying at the next turn.
use super::risk::{self, Fetch};
use anyhow::{Context, Result};
use parking_lot::Mutex;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU8, Ordering},
        mpsc,
    },
};
use uuid::Uuid;
use xylo_core::PermissionMode;
use xylo_services::{ConnectorBroker, CredentialVault, Store};

/// The current permission mode of a worker, shared by every broker it runs.
#[derive(Clone, Debug, Default)]
pub struct PermissionCell(Arc<AtomicU8>);

impl PermissionCell {
    /// A cell starting at `mode`.
    pub fn new(mode: PermissionMode) -> Self {
        let cell = Self::default();
        cell.set(mode);
        cell
    }

    /// The mode in force right now.
    pub fn get(&self) -> PermissionMode {
        match self.0.load(Ordering::Acquire) {
            0 => PermissionMode::AskForApproval,
            1 => PermissionMode::ApproveForMe,
            2 => PermissionMode::FullAccess,
            _ => PermissionMode::ReadOnly,
        }
    }

    /// Applies `mode` to every later tool call.
    pub fn set(&self, mode: PermissionMode) {
        let value = match mode {
            PermissionMode::AskForApproval => 0,
            PermissionMode::ApproveForMe => 1,
            PermissionMode::FullAccess => 2,
            PermissionMode::ReadOnly => 3,
        };
        self.0.store(value, Ordering::Release);
    }
}

/// Everything the brokers of one worker share: the approval gate, the live
/// permission mode, the unlocked secret vault and the MCP connection pool,
/// which keeps servers warm across turns (its reaper closes idle ones).
#[derive(Clone)]
pub struct ToolSession {
    approvals: ApprovalGate,
    permissions: PermissionCell,
    vault: CredentialVault,
    connectors: Arc<OnceLock<ConnectorBroker>>,
}

impl ToolSession {
    /// A session reading `permissions` live and using `vault` for every
    /// connector, plugin and image credential.
    pub fn new(permissions: PermissionCell, vault: CredentialVault) -> Self {
        Self {
            approvals: ApprovalGate::default(),
            permissions,
            vault,
            connectors: Arc::default(),
        }
    }

    /// Where approval decisions are delivered.
    pub fn approvals(&self) -> &ApprovalGate {
        &self.approvals
    }

    /// The live permission mode.
    pub fn permissions(&self) -> &PermissionCell {
        &self.permissions
    }

    /// The unlocked secret vault.
    pub fn vault(&self) -> &CredentialVault {
        &self.vault
    }

    /// The worker's MCP connection pool, created on first use. A worker has
    /// one store, so the first broker's store serves every later one.
    pub(crate) fn connectors(&self, store: &Store) -> ConnectorBroker {
        self.connectors
            .get_or_init(|| ConnectorBroker::new(store.clone(), self.vault.clone()))
            .clone()
    }

    /// Closes every MCP connection; call when the worker exits.
    pub async fn shutdown(&self) {
        if let Some(connectors) = self.connectors.get() {
            connectors.shutdown().await;
        }
    }

    /// A session with an isolated vault in its own temporary directory, for
    /// unit tests.
    #[cfg(test)]
    pub(crate) fn for_tests(mode: PermissionMode) -> Self {
        Self::new(PermissionCell::new(mode), CredentialVault::for_tests())
    }
}

/// The rules a turn treats as approved; shared by the turn's brokers.
pub(crate) type RuleSet = Arc<Mutex<HashSet<String>>>;

struct Pending {
    sender: mpsc::SyncSender<bool>,
    rule: Option<(String, RuleSet)>,
}

/// Approval channels are installed before emitting a request and authorize
/// one action.
#[derive(Clone, Default)]
pub struct ApprovalGate(Arc<Mutex<HashMap<Uuid, Pending>>>);

impl ApprovalGate {
    /// Delivers a decision; `remember` also allows later actions with the same
    /// rule for the rest of the turn (the host persists it for later turns).
    pub fn decide_with(&self, id: Uuid, allow: bool, remember: bool) -> Result<()> {
        let pending = self
            .0
            .lock()
            .remove(&id)
            .with_context(|| format!("Approval for action {id} is no longer pending"))?;
        if allow
            && remember
            && let Some((rule, rules)) = pending.rule
        {
            rules.lock().insert(rule);
        }
        pending
            .sender
            .send(allow)
            .with_context(|| format!("Action {id} stopped before receiving the decision"))
    }

    pub(crate) fn install(
        &self,
        id: Uuid,
        sender: mpsc::SyncSender<bool>,
        rule: Option<(String, RuleSet)>,
    ) {
        self.0.lock().insert(id, Pending { sender, rule });
    }

    pub(crate) fn withdraw(&self, id: Uuid) {
        self.0.lock().remove(&id);
    }

    /// Whether any action of this worker still waits for a decision.
    pub(crate) fn waiting(&self) -> bool {
        !self.0.lock().is_empty()
    }
}

/// Who started a tool call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    /// The model asked for it.
    Model,
    /// The broker runs an enabled plugin hook around another call.
    Hook,
}

/// Where a call's path argument points.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Reach {
    /// No path, or a path inside the project (or a saved tool result).
    Project,
    /// A canonical path outside the project.
    Outside(PathBuf),
}

/// What [`decide`] needs to know about one call.
pub(crate) struct Call<'a> {
    pub(crate) name: &'a str,
    pub(crate) arguments: &'a Value,
    /// The effective classification (connector and extension calls resolved).
    pub(crate) writes: bool,
    pub(crate) origin: Origin,
    pub(crate) reach: &'a Reach,
    pub(crate) workspace: &'a Path,
    /// The justification of a command that asked for internet access.
    pub(crate) network: Option<&'a str>,
}

/// The outcome of a permission check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Allow,
    /// Ask the user; the reason says why this mode needs a decision.
    Ask(String),
    /// Refuse without asking.
    Refuse(String),
}

/// Decides whether a call runs, asks or is refused under `mode`.
pub(crate) fn decide(mode: PermissionMode, call: &Call) -> Verdict {
    if call.origin == Origin::Hook {
        return Verdict::Allow;
    }
    if let Some(justification) = call.network {
        return network(mode, call, justification);
    }
    match mode {
        PermissionMode::FullAccess => Verdict::Allow,
        PermissionMode::ApproveForMe => approve_for_me(call),
        PermissionMode::ReadOnly if call.writes => Verdict::Refuse(format!(
            "{} requires write permission; this task is read-only",
            call.name
        )),
        PermissionMode::ReadOnly | PermissionMode::AskForApproval => ask_for_approval(call),
    }
}

/// "Approve for me": writes outside the project and connector traffic that
/// is not a small, secret-free lookup always ask; everything else, outside
/// reads included, asks only when the risk assessment flags it. Never
/// refuses, so "Ask for approval" can build on it.
fn approve_for_me(call: &Call) -> Verdict {
    if let Reach::Outside(path) = call.reach
        && call.writes
    {
        return Verdict::Ask(outside_reason(call, path));
    }
    if call.name == "connector_call" {
        if call.writes {
            return Verdict::Ask("Sends data to an external service".into());
        }
        let arguments = call.arguments.get("arguments").unwrap_or(&Value::Null);
        if !risk::lookup_arguments(arguments) {
            return Verdict::Ask(
                "Sends large or sensitive data to a connector the server calls read-only".into(),
            );
        }
    }
    match risk::assess(call.name, call.arguments, call.workspace) {
        Some(reason) => Verdict::Ask(reason),
        None => Verdict::Allow,
    }
}

fn outside_reason(call: &Call, path: &Path) -> String {
    let verb = if call.writes { "Changes" } else { "Reads" };
    format!("{verb} files outside the project: {}", path.display())
}

/// A command that asked to leave the sandbox's network restriction.
fn network(mode: PermissionMode, call: &Call, justification: &str) -> Verdict {
    match mode {
        PermissionMode::FullAccess => Verdict::Allow,
        PermissionMode::ReadOnly => Verdict::Refuse(format!(
            "{} requires write permission; this task is read-only",
            call.name
        )),
        PermissionMode::AskForApproval => Verdict::Ask(format!("Network access: {justification}")),
        PermissionMode::ApproveForMe => {
            match risk::routine_fetch(call.name, call.arguments, call.workspace) {
                Fetch::Routine => Verdict::Allow,
                Fetch::Configured(reason) => Verdict::Ask(reason),
                // Anything beyond a plain download may run project or
                // dependency code (build scripts, tests, install hooks)
                // with the internet open.
                Fetch::Other => Verdict::Ask(
                    risk::assess(call.name, call.arguments, call.workspace)
                        .unwrap_or_else(|| format!("Network access: {justification}")),
                ),
            }
        }
    }
}

/// Commands the sandbox confines to the project, offline. "Ask for approval"
/// runs them without consulting the risk assessment: the sandbox, not the
/// user, bounds what they can do (network access was decided before).
const SANDBOXED_COMMANDS: &[&str] = &["run_command", "shell_command"];

/// In-process writes "Ask for approval" runs unasked unless the risk
/// assessment flags them, as "Approve for me" does: no sandbox sees them, so
/// an edit to `.envrc`, a CI workflow or `.cargo/config.toml` asks.
const PROJECT_WRITES: &[&str] = &[
    "file_write",
    "file_edit",
    "memory_remember",
    "skill_propose",
];

/// Why "Ask for approval" asks for the other writing tools: the first entry
/// whose prefix the tool name starts with wins (a full name matches itself),
/// and an unlisted tool gets [`OTHER_CHANGE`].
const ASK_REASONS: &[(&str, &str)] = &[
    ("connector_", "Uses a connector or MCP server"),
    ("extension_", "Runs code from an installed plugin"),
    (
        "image_generate",
        "Generates an image with the saved provider, which may incur a charge",
    ),
    ("plugin_", "Runs an installed plugin"),
    ("environment_", "Runs in a remote or container environment"),
    ("schedule_", "Schedules work that runs later"),
    ("browser_", "Uses the browser and the internet"),
];

const OTHER_CHANGE: &str = "Changes something outside the project's files";

fn ask_for_approval(call: &Call) -> Verdict {
    if let Reach::Outside(path) = call.reach {
        return Verdict::Ask(outside_reason(call, path));
    }
    let name = call.name;
    if name.starts_with("desktop_") {
        return Verdict::Ask("Uses your screen, mouse or keyboard outside the project".into());
    }
    // A server's read-only hint is its own claim; every call asks.
    if name == "connector_call" {
        return Verdict::Ask("Uses a connector or MCP server".into());
    }
    if SANDBOXED_COMMANDS.contains(&name) {
        return Verdict::Allow;
    }
    if let ask @ Verdict::Ask(_) = approve_for_me(call) {
        return ask;
    }
    if !call.writes || PROJECT_WRITES.contains(&name) {
        return Verdict::Allow;
    }
    let reason = ASK_REASONS
        .iter()
        .find(|(prefix, _)| name.starts_with(prefix))
        .map_or(OTHER_CHANGE, |(_, reason)| reason);
    Verdict::Ask(reason.into())
}

/// Longest rule key stored as is; longer keys are hashed so they persist
/// within the store's limit while still covering exactly one action.
const RULE_KEY_BYTES: usize = 400;
/// Longest label shown for a rule.
const RULE_LABEL_CHARS: usize = 160;

/// The rule key and its label for "always allow in this project". Every
/// label states exactly what the rule covers.
pub(crate) fn approval_rule(
    name: &str,
    arguments: &Value,
    reach: &Reach,
    workspace: &Path,
) -> Option<(String, String)> {
    let (key, label) = rule(name, arguments, reach, workspace)?;
    Some((bounded_key(key), bounded_label(label)))
}

fn rule(
    name: &str,
    arguments: &Value,
    reach: &Reach,
    workspace: &Path,
) -> Option<(String, String)> {
    let text = |key: &str| arguments.get(key).and_then(Value::as_str);
    if let Reach::Outside(path) = reach {
        let folder = if path.is_dir() {
            path.as_path()
        } else {
            path.parent().unwrap_or(path)
        };
        let (kind, label) = if matches!(name, "file_write" | "file_edit") {
            ("edit", "file edits")
        } else {
            ("read", "reading files")
        };
        return Some((
            format!("{kind}:{}", folder.display()),
            format!("{label} in {}", folder.display()),
        ));
    }
    if arguments.get("network").and_then(Value::as_bool) == Some(true) {
        return network_rule(name, arguments, workspace);
    }
    match name {
        // Risky project files (secrets, hooks, CI, tool configuration) get a
        // rule of their own that ordinary edits never cover.
        "file_write" | "file_edit" => match risk::assess(name, arguments, workspace) {
            Some(_) => {
                let shown = risk::project_relative(workspace, text("path")?)?;
                Some((format!("edit:{shown}"), format!("edits to `{shown}`")))
            }
            None => Some(("edit".into(), "file edits in this project".into())),
        },
        "run_command" => {
            let (key, label) = command_rule(arguments, workspace)?;
            Some((format!("run:{key}"), label))
        }
        "shell_command" => {
            let command = text("command")?;
            Some((format!("shell:{command}"), format!("exactly `{command}`")))
        }
        "memory_remember" => Some(("memory".into(), "saving project facts".into())),
        "connector_call" => {
            let connector = text("connector_id")?;
            let tool = text("tool")?;
            Some((
                format!("connector:{connector}:{tool}"),
                format!("`{tool}` on this connector"),
            ))
        }
        "image_generate" => Some((
            "image".into(),
            "image generation with the saved provider".into(),
        )),
        "plugin_run_installed" => {
            let id = text("id")?;
            let digest = text("package_sha256")?;
            Some((
                format!("plugin:{id}:{digest}"),
                format!("running the reviewed plugin `{id}`"),
            ))
        }
        "extension_component" => {
            let package = text("id")?;
            let component = text("component")?;
            let operation = text("operation")?;
            Some((
                format!("component:{package}:{component}:{operation}"),
                format!("`{operation}` on the `{component}` plugin component"),
            ))
        }
        _ => None,
    }
}

/// Network rules are keyed like command rules with a `net:` prefix, so
/// allowing a command offline never allows it online, or the reverse.
fn network_rule(name: &str, arguments: &Value, workspace: &Path) -> Option<(String, String)> {
    match name {
        "run_command" => {
            let (key, label) = command_rule(arguments, workspace)?;
            Some((
                format!("net:run:{key}"),
                format!("network access for {label}"),
            ))
        }
        "shell_command" => {
            let command = arguments.get("command").and_then(Value::as_str)?;
            Some((
                format!("net:shell:{command}"),
                format!("network access for exactly `{command}`"),
            ))
        }
        _ => None,
    }
}

/// Build, test, lint and format subcommands whose rule may cover any
/// arguments: they run the project's own code, which the sandbox confines,
/// and change nothing beyond build output. `None` means the program itself.
const BROAD_COMMANDS: &[(&str, Option<&str>)] = &[
    ("cargo", Some("build")),
    ("cargo", Some("check")),
    ("cargo", Some("test")),
    ("cargo", Some("clippy")),
    ("cargo", Some("fmt")),
    ("go", Some("build")),
    ("go", Some("test")),
    ("go", Some("vet")),
    ("npm", Some("test")),
    ("pnpm", Some("test")),
    ("yarn", Some("test")),
    ("swift", Some("build")),
    ("swift", Some("test")),
    ("pytest", None),
];

/// The rule for an argv command: the exact argument vector, except for the
/// build, test, lint and format subcommands above (written first, before any
/// option) when the risk assessment does not flag the call.
fn command_rule(arguments: &Value, workspace: &Path) -> Option<(String, String)> {
    let program = arguments.get("program").and_then(Value::as_str)?;
    let args: Vec<&str> = arguments
        .get("args")
        .and_then(Value::as_array)
        .map(|args| args.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let base = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program);
    let broad = BROAD_COMMANDS.iter().find(|(known, subcommand)| {
        *known == base
            && match subcommand {
                Some(subcommand) => args.first() == Some(subcommand),
                None => true,
            }
    });
    if let Some((_, subcommand)) = broad
        && risk::assess("run_command", arguments, workspace).is_none()
    {
        let command = match subcommand {
            Some(subcommand) => format!("{program} {subcommand}"),
            None => program.to_owned(),
        };
        return Some((
            format!("any:{command}"),
            format!("`{command}` with any arguments"),
        ));
    }
    let exact = std::iter::once(program)
        .chain(args.iter().copied())
        .map(|word| shell_words::quote(word).into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    Some((format!("exact:{exact}"), format!("exactly `{exact}`")))
}

fn bounded_key(key: String) -> String {
    use sha2::{Digest, Sha256};
    if key.len() <= RULE_KEY_BYTES {
        return key;
    }
    let mut prefix_end = 64.min(key.len());
    while !key.is_char_boundary(prefix_end) {
        prefix_end -= 1;
    }
    format!(
        "{}…sha256:{}",
        &key[..prefix_end],
        hex::encode(Sha256::digest(key.as_bytes()))
    )
}

fn bounded_label(label: String) -> String {
    match label.char_indices().nth(RULE_LABEL_CHARS) {
        Some((cut, _)) => format!("{}…`", &label[..cut]),
        None => label,
    }
}

#[cfg(test)]
mod tests;
