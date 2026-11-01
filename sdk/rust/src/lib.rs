//! Stable sessions over the versioned Xylo CLI protocol.
//!
//! A session owns one child process. [`Session::run`] collects a turn;
//! [`Session::stream`] exposes events while [`Control`] can approve or cancel.
//! Custom tools execute in the SDK caller only after the CLI broker authorizes
//! them. They must declare `writes=true` and use an `sdk_` name.

mod shutdown;

use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{collections::HashMap, path::PathBuf, process::Stdio, sync::Arc};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
    sync::Mutex,
};
use uuid::Uuid;
use xylo_core::stdio::{Command, Input};
pub use xylo_core::stdio::{Event, VERSION as PROTOCOL_VERSION};
pub use xylo_core::{
    EngineEvent, InteractionAnswer, PermissionMode, QuestionAnswer, QuestionOption, ToolDefinition,
    Usage, UserQuestion,
};

/// Explicit process and profile configuration. Environment is inherited.
#[derive(Clone, Default)]
pub struct SessionOptions {
    pub executable: PathBuf,
    pub cwd: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
    pub model: Option<String>,
    pub permission_mode: Option<PermissionMode>,
    pub allowed_tools: Option<Vec<String>>,
    pub disallowed_tools: Vec<String>,
    pub max_turns: Option<u32>,
    pub resume: Option<Uuid>,
    pub continue_latest: bool,
    pub append_system_prompt: Option<String>,
    pub debug: bool,
    /// Additional environment for the child, useful for OTEL configuration.
    pub environment: HashMap<String, String>,
}

/// A custom implementation running in the SDK process.
pub type ToolCallback = Arc<dyn Fn(Value) -> std::result::Result<Value, String> + Send + Sync>;
/// An event callback, invoked exactly once for each event read by the SDK.
pub type HookCallback = Arc<dyn Fn(&Event) + Send + Sync>;

/// A cloneable command channel that remains usable while events are streaming.
#[derive(Clone)]
pub struct Control {
    input: Arc<Mutex<Option<ChildStdin>>>,
}
impl Control {
    async fn send(&self, command: Command) -> Result<()> {
        let mut bytes = serde_json::to_vec(&Input::new(command))?;
        ensure!(
            bytes.len() < xylo_core::MAX_PROTOCOL_BYTES,
            "SDK command exceeds frame limit"
        );
        bytes.push(b'\n');
        let mut input = self.input.lock().await;
        let input = input.as_mut().context("Session input is closed")?;
        input.write_all(&bytes).await.context("Write CLI command")?;
        input.flush().await?;
        Ok(())
    }
    /// Explicitly approves or denies a currently pending broker action.
    pub async fn approve(&self, action_id: Uuid, allow: bool) -> Result<()> {
        self.send(Command::Approve { action_id, allow }).await
    }
    /// Answers one pending question or plan without changing tool constraints.
    pub async fn answer(&self, action_id: Uuid, answer: InteractionAnswer) -> Result<()> {
        self.send(Command::Answer { action_id, answer }).await
    }
    /// Cancels the active turn while keeping its conversation available.
    pub async fn cancel(&self) -> Result<()> {
        self.send(Command::Cancel).await
    }
}

/// A stable stdio session. Dropping it terminates the CLI process.
pub struct Session {
    child: Child,
    output: BufReader<ChildStdout>,
    control: Control,
    tools: HashMap<String, ToolCallback>,
    hooks: Vec<HookCallback>,
    active: bool,
    sequence: u64,
    conversation: Option<Uuid>,
}

impl Session {
    /// Starts a persistent CLI process. Configuration is passed as separate
    /// arguments, never interpreted by a shell.
    pub async fn new(options: SessionOptions) -> Result<Self> {
        let executable = if options.executable.as_os_str().is_empty() {
            PathBuf::from("xylo-cli")
        } else {
            options.executable
        };
        let mut command = tokio::process::Command::new(executable);
        command.args([
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
        ]);
        for (flag, value) in [("--cwd", options.cwd), ("--data-dir", options.data_dir)] {
            if let Some(value) = value {
                command.arg(flag).arg(value);
            }
        }
        for (flag, value) in [
            ("--model", options.model),
            ("--append-system-prompt", options.append_system_prompt),
            ("--resume", options.resume.map(|id| id.to_string())),
            (
                "--max-turns",
                options.max_turns.map(|count| count.to_string()),
            ),
        ] {
            if let Some(value) = value {
                command.arg(flag).arg(value);
            }
        }
        if let Some(mode) = options.permission_mode {
            command.arg("--permission-mode").arg(
                serde_json::to_value(mode)?
                    .as_str()
                    .context("Invalid permission mode")?,
            );
        }
        if let Some(allowed) = options.allowed_tools {
            command.arg("--allowed-tools").arg(allowed.join(","));
        }
        if !options.disallowed_tools.is_empty() {
            command
                .arg("--disallowed-tools")
                .arg(options.disallowed_tools.join(","));
        }
        if options.continue_latest {
            command.arg("--continue");
        }
        if options.debug {
            command.arg("--debug");
        }
        let mut child = command
            .envs(options.environment)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .context("Start xylo-cli")?;
        let input = child.stdin.take().context("CLI stdin is missing")?;
        let output = BufReader::new(child.stdout.take().context("CLI stdout is missing")?);
        Ok(Self {
            child,
            output,
            control: Control {
                input: Arc::new(Mutex::new(Some(input))),
            },
            tools: HashMap::new(),
            hooks: Vec::new(),
            active: false,
            sequence: 0,
            conversation: None,
        })
    }
    /// Returns an independently usable approval and cancellation channel.
    pub fn control(&self) -> Control {
        self.control.clone()
    }
    /// Sends an explicit action decision.
    pub async fn approve(&self, action_id: Uuid, allow: bool) -> Result<()> {
        self.control.approve(action_id, allow).await
    }
    /// Registers an event hook. Hooks receive metadata and tool events as well as deltas.
    pub fn on_event(&mut self, hook: HookCallback) {
        self.hooks.push(hook);
    }
    /// Registers an external tool before starting the next turn.
    pub async fn register_tool(
        &mut self,
        definition: ToolDefinition,
        callback: ToolCallback,
    ) -> Result<()> {
        ensure!(!self.active, "Register tools while the session is idle");
        ensure!(
            definition.name.starts_with("sdk_") && definition.writes,
            "Custom tools require an sdk_ name and writes=true"
        );
        ensure!(
            !self.tools.contains_key(&definition.name),
            "Tool is already registered"
        );
        self.control
            .send(Command::RegisterTool {
                definition: definition.clone(),
            })
            .await?;
        self.tools.insert(definition.name, callback);
        Ok(())
    }
    /// Begins a turn and returns its event stream. Pending approvals must be
    /// answered through [`Control`]; otherwise the stream remains waiting.
    pub async fn stream(&mut self, prompt: impl Into<String>) -> Result<EventStream<'_>> {
        ensure!(
            !self.active,
            "A run is already active; finish or cancel its stream"
        );
        let prompt = prompt.into();
        ensure!(!prompt.trim().is_empty(), "The prompt must not be empty");
        self.control.send(Command::Run { prompt }).await?;
        self.active = true;
        Ok(EventStream {
            session: self,
            done: false,
        })
    }
    /// Runs a prompt and collects its final result. Approval requests are denied
    /// by default, so this convenience method cannot hang waiting for user input.
    pub async fn run(&mut self, prompt: impl Into<String>) -> Result<RunResult> {
        let control = self.control();
        let mut stream = self.stream(prompt).await?;
        let mut events = Vec::new();
        while let Some(event) = stream.next().await? {
            if let EngineEvent::ApprovalRequired { action_id, .. } = &event.payload {
                control.approve(*action_id, false).await?;
            }
            if let EngineEvent::QuestionRequired { action_id, .. }
            | EngineEvent::PlanRequired { action_id, .. } = &event.payload
            {
                control
                    .answer(*action_id, InteractionAnswer::Reject)
                    .await?;
            }
            events.push(event);
        }
        let terminal = events.last().context("CLI ended without a result")?;
        let (text, usage, error) = match &terminal.payload {
            EngineEvent::Completed { text, usage } => (text.clone(), Some(usage.clone()), None),
            EngineEvent::Error { message, .. } => (String::new(), None, Some(message.clone())),
            EngineEvent::Cancelled => (String::new(), None, Some("Run cancelled".into())),
            _ => anyhow::bail!("CLI result is not terminal"),
        };
        Ok(RunResult {
            session_id: terminal.session_id,
            turn_id: terminal.turn_id,
            exit_code: terminal
                .exit_code
                .context("CLI terminal frame has no exit code")?,
            text,
            usage,
            error,
            events,
        })
    }
    /// Drains unread events while requesting shutdown, without invoking callbacks.
    /// After five seconds, stops the CLI and allows five more seconds to reap it.
    pub async fn close(mut self) -> Result<i32> {
        shutdown::close(&mut self.child, &mut self.output, &self.control).await
    }
}

/// A borrowed stream for one turn. Call `next` until it returns `None`.
pub struct EventStream<'a> {
    session: &'a mut Session,
    done: bool,
}
impl EventStream<'_> {
    /// Reads and validates one engine event, running hooks and custom callbacks.
    pub async fn next(&mut self) -> Result<Option<Event>> {
        if self.done {
            return Ok(None);
        }
        let mut line = Vec::new();
        let length = (&mut self.session.output)
            .take((xylo_core::MAX_PROTOCOL_BYTES + 1) as u64)
            .read_until(b'\n', &mut line)
            .await?;
        ensure!(length > 0, "CLI closed its output before the run finished");
        ensure!(
            line.len() <= xylo_core::MAX_PROTOCOL_BYTES && line.last() == Some(&b'\n'),
            "Invalid or oversized CLI output frame"
        );
        let event: Event = serde_json::from_slice(&line).context("Decode CLI event")?;
        event.validate()?;
        ensure!(
            event.sequence == self.session.sequence + 1,
            "CLI event sequence is out of order"
        );
        if let Some(conversation) = self.session.conversation {
            ensure!(
                conversation == event.session_id,
                "CLI changed conversation identity"
            );
        }
        self.session.conversation = Some(event.session_id);
        self.session.sequence = event.sequence;
        for hook in &self.session.hooks {
            hook(&event);
        }
        if let EngineEvent::CustomToolRequested {
            action_id,
            name,
            arguments,
        } = &event.payload
        {
            let callback = self
                .session
                .tools
                .get(name)
                .context("CLI requested an unregistered SDK tool")?
                .clone();
            let arguments = arguments.clone();
            let result = tokio::task::spawn_blocking(move || callback(arguments))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            self.session
                .control
                .send(Command::ToolResult {
                    action_id: *action_id,
                    result,
                })
                .await?;
        }
        if event.is_terminal() {
            self.done = true;
            self.session.active = false;
        }
        Ok(Some(event))
    }
}

/// A collected turn with its exact terminal status and all observed events.
pub struct RunResult {
    pub session_id: Uuid,
    pub turn_id: Uuid,
    pub exit_code: i32,
    pub text: String,
    pub usage: Option<Usage>,
    pub error: Option<String>,
    pub events: Vec<Event>,
}
