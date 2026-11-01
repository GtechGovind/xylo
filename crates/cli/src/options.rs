//! Command-line input is validated before opening any profile.
use anyhow::{Context, Result, bail, ensure};
use std::path::PathBuf;
use uuid::Uuid;
use xylo_core::PermissionMode;
use xylo_engine::RunOptions;

pub const HELP: &str = "Usage: xylo-cli [OPTIONS]\n\n-p, --print PROMPT             Run one prompt (otherwise read stdin)\n--output-format FORMAT         text, json, stream-json\n--input-format stream-json     Persistent versioned JSONL commands on stdin\n--model MODEL                  Override the configured model\n--permission-mode MODE         ask_for_approval, approve_for_me, full_access, read_only\n--allowed-tools NAMES          Comma-separated exact tool names or Read/Write/Edit/Glob/Grep/Bash\n--disallowed-tools NAMES       Comma-separated denied tools; denial wins\n--max-turns N                  Maximum model rounds (1..400)\n--resume ID                    Continue a conversation by UUID\n--continue                     Continue the most recent conversation\n--cwd PATH                     Workspace for a new conversation\n--append-system-prompt TEXT    Append instructions to the system prompt\n--data-dir PATH                Use an isolated application profile\n--export PATH.md               Export complete history to Markdown and adjacent JSONL\n--debug                        Enable debug logging in <data-dir>/logs\n--help, --version\n\nExit codes: 0 success, 1 error, 2 refused. Pending approvals are denied unless a stream-json client explicitly answers them.\n";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Text,
    Json,
    Stream,
}

pub struct Options {
    pub prompt: Option<String>,
    pub output: Format,
    pub stream_input: bool,
    pub model: Option<String>,
    pub permission: Option<PermissionMode>,
    pub run: RunOptions,
    pub resume: Option<Uuid>,
    pub latest: bool,
    pub cwd: PathBuf,
    pub data: Option<PathBuf>,
    pub debug: bool,
    pub export: Option<PathBuf>,
}
impl Options {
    pub fn parse(arguments: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut options = Self {
            prompt: None,
            output: Format::Text,
            stream_input: false,
            model: None,
            permission: None,
            run: RunOptions::default(),
            resume: None,
            latest: false,
            cwd: std::env::current_dir()?,
            data: None,
            debug: false,
            export: None,
        };
        let mut args = arguments.into_iter();
        while let Some(flag) = args.next() {
            match flag.as_str() {
                "--debug" => options.debug = true,
                "--continue" => options.latest = true,
                _ => {
                    let value = args
                        .next()
                        .with_context(|| format!("{flag} requires a value; use --help"))?;
                    match flag.as_str() {
                        "-p" | "--print" => {
                            ensure!(
                                options.prompt.is_none(),
                                "Only one --print prompt is supported"
                            );
                            options.prompt = Some(value);
                        }
                        "--output-format" => {
                            options.output = match value.as_str() {
                                "text" => Format::Text,
                                "json" => Format::Json,
                                "stream-json" => Format::Stream,
                                _ => bail!("Unknown output format {value}"),
                            }
                        }
                        "--input-format" => {
                            ensure!(
                                value == "stream-json",
                                "Only --input-format stream-json is supported"
                            );
                            options.stream_input = true;
                        }
                        "--model" => {
                            ensure!(!value.trim().is_empty(), "--model must not be empty");
                            options.model = Some(value);
                        }
                        "--permission-mode" => {
                            options.permission = Some(
                                serde_json::from_value(serde_json::Value::String(value))
                                    .context("Unknown permission mode")?,
                            )
                        }
                        "--allowed-tools" => options.run.allowed_tools = Some(tools(&value)?),
                        "--disallowed-tools" => options.run.disallowed_tools = tools(&value)?,
                        "--max-turns" => {
                            let count: u32 = value
                                .parse()
                                .context("--max-turns requires a positive integer")?;
                            ensure!(
                                (1..=400).contains(&count),
                                "--max-turns must be between 1 and 400"
                            );
                            options.run.max_turns = Some(count);
                        }
                        "--resume" => {
                            options.resume = Some(
                                value
                                    .parse()
                                    .context("--resume requires a conversation UUID")?,
                            )
                        }
                        "--cwd" => options.cwd = PathBuf::from(value),
                        "--append-system-prompt" => options.run.append_system_prompt = value,
                        "--data-dir" => options.data = Some(PathBuf::from(value)),
                        "--export" => options.export = Some(PathBuf::from(value)),
                        _ => bail!("Unknown option {flag}; use --help"),
                    }
                }
            }
        }
        ensure!(
            !(options.latest && options.resume.is_some()),
            "--continue and --resume are mutually exclusive"
        );
        ensure!(
            !options.stream_input || options.output == Format::Stream,
            "--input-format stream-json requires --output-format stream-json"
        );
        ensure!(
            !options.stream_input || options.prompt.is_none(),
            "--print cannot be combined with --input-format stream-json"
        );
        options.cwd = options
            .cwd
            .canonicalize()
            .context("--cwd must be an existing directory")?;
        ensure!(options.cwd.is_dir(), "--cwd must be a directory");
        if let Some(path) = &options.export {
            ensure!(
                path.extension().is_some_and(|extension| extension == "md"),
                "--export requires an .md file"
            );
        }
        Ok(options)
    }
}
fn tools(value: &str) -> Result<Vec<String>> {
    let names = value
        .split(',')
        .map(str::trim)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    ensure!(
        names.iter().all(|name| !name.is_empty()),
        "Tool lists require comma-separated nonempty names"
    );
    Ok(names)
}
