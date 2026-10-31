//! Every prompt the engine sends to a model, and the built-in skills.
//!
//! Prompt text lives only in `crates/engine/prompts/*.md` and skills only in
//! `crates/engine/skills/<name>/`; both are compiled in with `include_str!`.
//! Templates use `{{name}}` placeholders filled by a strict renderer, and the
//! tests render every template, so a bad placeholder fails the build's tests
//! rather than a user's turn.
//!
//! Loading is lazy by design: the system prompt carries identity,
//! environment, working method, the permission policy, project instruction
//! files and a compact skills index (names with descriptions). Skill bodies,
//! skill resources, learned procedure steps and full memory text reach the
//! model only through tools. The system prompt depends only on
//! [`PromptContext`], which holds nothing derived from the user's prompt,
//! and its date has day granularity, so it is byte-identical across the
//! calls and turns of a task and stays provider-cacheable. What is retrieved
//! for a prompt (fact first lines, learned procedure names) travels in a
//! [`recalled_context`] note after that prompt instead, where it becomes
//! append-only history.

mod environment;
mod instructions;
mod render;
mod skills;

use std::path::Path;

use xylo_core::{MemoryOrigin, PermissionMode};

pub use environment::{GitInfo, utc_date};
pub use instructions::project_instructions;
pub use skills::{BundledSkill, bundled_skill, bundled_skills, skills_index};

const SYSTEM: &str = include_str!("../../prompts/system.md");
const ENVIRONMENT: &str = include_str!("../../prompts/environment.md");
const PERMISSIONS: &str = include_str!("../../prompts/permissions.md");
const PERMISSION_CHANGE: &str = include_str!("../../prompts/permission-change.md");
const SYSTEM_PROJECT_INSTRUCTIONS: &str =
    include_str!("../../prompts/system-project-instructions.md");
const SYSTEM_SKILLS: &str = include_str!("../../prompts/system-skills.md");
const PERMISSION_ASK_FOR_APPROVAL: &str =
    include_str!("../../prompts/permissions/ask-for-approval.md");
const PERMISSION_APPROVE_FOR_ME: &str = include_str!("../../prompts/permissions/approve-for-me.md");
const PERMISSION_FULL_ACCESS: &str = include_str!("../../prompts/permissions/full-access.md");
const PERMISSION_READ_ONLY: &str = include_str!("../../prompts/permissions/read-only.md");
const COMPACTION: &str = include_str!("../../prompts/compaction.md");
const COMPACTION_REQUEST: &str = include_str!("../../prompts/compaction-request.md");
const COMPACTION_UPDATE: &str = include_str!("../../prompts/compaction-update.md");
const COMPACTION_SUMMARY: &str = include_str!("../../prompts/compaction-summary.md");
const COMPACTION_EXCERPTS: &str = include_str!("../../prompts/compaction-excerpts.md");
const COMPACTION_TOOL_NOTES: &str = include_str!("../../prompts/compaction-tool-notes.md");
const COMPACTION_RECORD: &str = include_str!("../../prompts/compaction-record.md");
const COMPACTION_RECORD_OMITTED: &str = include_str!("../../prompts/compaction-record-omitted.md");
const MASKED_READ_SUPERSEDED: &str = include_str!("../../prompts/masked-read-superseded.md");
const MASKED_READ_CHANGED: &str = include_str!("../../prompts/masked-read-changed.md");
const MASKED_SAVED: &str = include_str!("../../prompts/masked-saved.md");
const MASKED_AGED: &str = include_str!("../../prompts/masked-aged.md");
const TITLE: &str = include_str!("../../prompts/title.md");
const SUBAGENT: &str = include_str!("../../prompts/subagent.md");
const SUBAGENT_TASK: &str = include_str!("../../prompts/subagent-task.md");
const SKILL_PERMISSIONS_NOTE: &str = include_str!("../../prompts/skill-permissions-note.md");
const FORKED_SKILL_CONTEXT: &str = include_str!("../../prompts/forked-skill-context.md");
const STEP_LIMIT: &str = include_str!("../../prompts/step-limit.md");
const APPLICATION_NOTE: &str = include_str!("../../prompts/application-note.md");
const RECALLED_CONTEXT: &str = include_str!("../../prompts/recalled-context.md");
const RECALLED_FACTS: &str = include_str!("../../prompts/recalled-facts.md");
const RECALLED_PROCEDURES: &str = include_str!("../../prompts/recalled-procedures.md");
const RECALLED_SKILLS: &str = include_str!("../../prompts/recalled-skills.md");
const CONTINUE_ANSWER: &str = include_str!("../../prompts/continue-answer.md");
const EMPTY_ANSWER: &str = include_str!("../../prompts/empty-answer.md");
const OUTPUT_LIMIT_WITHOUT_ANSWER: &str =
    include_str!("../../prompts/output-limit-without-answer.md");
const MALFORMED_TOOL_CALL: &str = include_str!("../../prompts/malformed-tool-call.md");
const TRUNCATED_TOOL_CALL: &str = include_str!("../../prompts/truncated-tool-call.md");
const STEP_BUDGET: &str = include_str!("../../prompts/step-budget.md");
const STALL_NOTE: &str = include_str!("../../prompts/stall-note.md");
const STALL_WARNING: &str = include_str!("../../prompts/stall-warning.md");
const STALL_REPEATED_CALL: &str = include_str!("../../prompts/stall-repeated-call.md");
const STALL_ALTERNATING: &str = include_str!("../../prompts/stall-alternating.md");
const STALL_REPEATED_ERROR: &str = include_str!("../../prompts/stall-repeated-error.md");

/// Byte budgets of the variable system-prompt sections. The engine scales
/// them to the model's effective window, so the fixed preamble stays a small
/// share of a small window; [`PromptBudget::LARGEST`] is the ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptBudget {
    /// Bytes of project instruction text (AGENTS.md, CLAUDE.md).
    pub instruction_bytes: usize,
    /// Bytes of the skills index.
    pub skills_index_bytes: usize,
}

impl PromptBudget {
    /// The budgets of a large window; no window gets more.
    pub const LARGEST: Self = Self {
        instruction_bytes: instructions::INSTRUCTION_BYTES,
        skills_index_bytes: skills::SKILLS_INDEX_BYTES,
    };
}

/// Everything the system prompt depends on. Two equal contexts render
/// byte-identical prompts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptContext<'a> {
    /// The project directory the task works in.
    pub workspace: &'a Path,
    /// Operating system name, for example `macos`.
    pub os: &'a str,
    /// CPU architecture, for example `aarch64`.
    pub arch: &'a str,
    /// What `shell_command` runs in, as the model should understand it.
    pub shell: &'a str,
    /// Repository state; `None` outside a Git repository.
    pub git: Option<GitInfo>,
    /// Current UTC day as `YYYY-MM-DD` (see [`utc_date`]); never finer.
    pub date: String,
    /// The permission mode in force when the prompt is built.
    pub permission_mode: PermissionMode,
    /// Output of [`project_instructions`]; empty when there are none.
    pub project_instructions: String,
    /// Output of [`skills_index`]; empty when no skills are available.
    pub skills_index: String,
}

impl<'a> PromptContext<'a> {
    /// A context for `workspace` with the host environment, repository state,
    /// today's date and the project instruction files (within `budget`)
    /// filled in. The skills index starts empty; set it before calling
    /// [`system`].
    pub fn detect(
        workspace: &'a Path,
        permission_mode: PermissionMode,
        now_ms: i64,
        budget: PromptBudget,
    ) -> Self {
        PromptContext {
            workspace,
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            shell: crate::tools::shell_name(),
            git: GitInfo::detect(workspace),
            date: utc_date(now_ms),
            permission_mode,
            project_instructions: project_instructions(workspace, budget.instruction_bytes),
            skills_index: String::new(),
        }
    }
}

/// The main agent's system prompt.
pub fn system(ctx: &PromptContext) -> String {
    let mut sections = vec![fill(
        SYSTEM,
        &[
            ("environment", &environment_section(ctx)),
            ("permissions", &permissions_section(ctx.permission_mode)),
        ],
    )];
    sections.extend(project_instructions_section(ctx));
    let skills = ctx.skills_index.trim();
    if !skills.is_empty() {
        sections.push(fill(SYSTEM_SKILLS, &[("index", skills)]));
    }
    sections.join("\n\n")
}

/// System prompt for a delegated helper agent working on `task`. The helper
/// shares the parent's environment, permission mode and project
/// instructions, but not its skill or fact indexes: it is scoped to one task
/// and reports back instead of using the parent's workflow aids.
pub fn subagent(ctx: &PromptContext, task: &str) -> String {
    let mut sections = vec![fill(
        SUBAGENT,
        &[
            ("environment", &environment_section(ctx)),
            ("permissions", &permissions_section(ctx.permission_mode)),
        ],
    )];
    sections.extend(project_instructions_section(ctx));
    sections.push(fill(SUBAGENT_TASK, &[("task", task.trim())]));
    sections.join("\n\n")
}

/// A system note for the next model call after the user changed the
/// permission mode mid-task. Appending it, rather than rebuilding the system
/// prompt, keeps the cached system block unchanged.
pub fn permission_change(mode: PermissionMode) -> String {
    fill(
        PERMISSION_CHANGE,
        &[
            ("permission_mode", mode.label()),
            ("permission_policy", permission_policy(mode)),
        ],
    )
}

fn environment_section(ctx: &PromptContext) -> String {
    let workspace = ctx.workspace.display().to_string();
    let git = match &ctx.git {
        None => "not a Git repository".to_owned(),
        Some(GitInfo {
            branch: Some(branch),
        }) => format!("Git repository, branch {branch}"),
        Some(GitInfo { branch: None }) => {
            "Git repository (detached HEAD or unknown branch)".to_owned()
        }
    };
    fill(
        ENVIRONMENT,
        &[
            ("workspace", &workspace),
            ("os", ctx.os),
            ("arch", ctx.arch),
            ("shell", ctx.shell),
            ("git", &git),
            ("date", &ctx.date),
        ],
    )
}

fn permissions_section(mode: PermissionMode) -> String {
    fill(
        PERMISSIONS,
        &[
            ("permission_mode", mode.label()),
            ("permission_policy", permission_policy(mode)),
        ],
    )
}

fn project_instructions_section(ctx: &PromptContext) -> Option<String> {
    let instructions = ctx.project_instructions.trim();
    (!instructions.is_empty()).then(|| {
        fill(
            SYSTEM_PROJECT_INSTRUCTIONS,
            &[("instructions", instructions)],
        )
    })
}

/// How the model should understand `mode`'s approvals.
fn permission_policy(mode: PermissionMode) -> &'static str {
    let text = match mode {
        PermissionMode::AskForApproval => PERMISSION_ASK_FOR_APPROVAL,
        PermissionMode::ApproveForMe => PERMISSION_APPROVE_FOR_ME,
        PermissionMode::FullAccess => PERMISSION_FULL_ACCESS,
        PermissionMode::ReadOnly => PERMISSION_READ_ONLY,
    };
    text.trim_end()
}

/// System prompt for the context summarizer.
pub fn compaction() -> &'static str {
    COMPACTION.trim_end()
}

/// The summarizer's user message: the transcript to condense, and the
/// previous summary to update when one exists.
pub fn compaction_request(previous: Option<&str>, transcript: &str) -> String {
    match previous {
        Some(previous) => fill(
            COMPACTION_UPDATE,
            &[("previous", previous), ("transcript", transcript)],
        ),
        None => fill(COMPACTION_REQUEST, &[("transcript", transcript)]),
    }
}

/// The message that replaces condensed history: a fixed header that frames
/// the summary as data, followed by `summary`.
pub fn compaction_summary(summary: &str) -> String {
    fill(COMPACTION_SUMMARY, &[("summary", summary)])
}

/// First line of the excerpt note that replaces older messages when no
/// model-written summary is available. The caller appends the excerpts.
pub fn compaction_excerpts_header() -> &'static str {
    COMPACTION_EXCERPTS.trim_end()
}

/// First line of the note that replaces a run of completed tool calls and
/// results when tool output must be condensed. The caller appends the notes.
pub fn compaction_tool_notes_header() -> &'static str {
    COMPACTION_TOOL_NOTES.trim_end()
}

/// The values of one harness record (see [`compaction_record`]): each list
/// is already rendered as item lines.
pub struct RecordSections<'a> {
    pub user_messages: &'a str,
    pub files_read: &'a str,
    pub files_changed: &'a str,
    pub commands: &'a str,
    pub todos: &'a str,
}

/// The harness record appended to a summary: verbatim user messages and the
/// artifact ledger the application derives from tool calls.
pub fn compaction_record(sections: &RecordSections) -> String {
    fill(
        COMPACTION_RECORD,
        &[
            ("user_messages", sections.user_messages),
            ("files_read", sections.files_read),
            ("files_changed", sections.files_changed),
            ("commands", sections.commands),
            ("todos", sections.todos),
        ],
    )
}

/// The record's opening and closing lines and the label line before each
/// list, in [`RecordSections`] order; parsing a stored record relies on them.
pub fn compaction_record_markers() -> (&'static str, &'static str, Vec<&'static str>) {
    let lines: Vec<&'static str> = COMPACTION_RECORD.trim_end().lines().collect();
    let labels = lines
        .windows(2)
        .filter(|pair| pair[1].starts_with("{{"))
        .map(|pair| pair[0])
        .collect();
    (
        lines.first().copied().unwrap_or_default(),
        lines.last().copied().unwrap_or_default(),
        labels,
    )
}

/// The record line standing for `count` pinned user messages dropped from
/// the middle to keep the record within its budget.
pub fn compaction_record_omitted(count: usize) -> String {
    fill(COMPACTION_RECORD_OMITTED, &[("count", &count.to_string())])
}

/// The count of a line written by [`compaction_record_omitted`].
pub fn parse_compaction_record_omitted(line: &str) -> Option<usize> {
    let (prefix, suffix) = COMPACTION_RECORD_OMITTED
        .trim_end()
        .split_once("{{count}}")?;
    line.strip_prefix(prefix)?
        .strip_suffix(suffix)?
        .parse()
        .ok()
}

/// Why an older tool result left the context, which decides how the model
/// can get it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskReason<'a> {
    /// A later read of the same file with the same range is authoritative.
    ReadSuperseded,
    /// The file was edited or rewritten after this read.
    ReadChanged,
    /// The complete output is saved at this path.
    Saved(&'a str),
    /// Older than the tool's retention; repeating the call restores it.
    Aged,
}

/// The stub that replaces a masked tool result. `call` names the tool and
/// its key argument; `bytes` is the original size.
pub fn masked_observation(call: &str, bytes: usize, reason: MaskReason) -> String {
    let size = format!("{bytes} bytes");
    let mut values = vec![("call", call), ("size", size.as_str())];
    let template = match reason {
        MaskReason::ReadSuperseded => MASKED_READ_SUPERSEDED,
        MaskReason::ReadChanged => MASKED_READ_CHANGED,
        MaskReason::Saved(path) => {
            values.push(("path", path));
            MASKED_SAVED
        }
        MaskReason::Aged => MASKED_AGED,
    };
    fill(template, &values)
}

/// Whether `content` is a stub written by [`masked_observation`].
pub fn is_masked_observation(content: &str) -> bool {
    [
        MASKED_READ_SUPERSEDED,
        MASKED_READ_CHANGED,
        MASKED_SAVED,
        MASKED_AGED,
    ]
    .iter()
    .filter_map(|template| template.split("{{").next())
    .any(|prefix| !prefix.is_empty() && content.starts_with(prefix))
}

/// System prompt for generating a 3–6 word conversation title.
pub fn title() -> &'static str {
    TITLE.trim_end()
}

/// The `permissions` value of a skill's tool result: loading a skill never
/// grants tool access.
pub fn skill_permissions_note() -> &'static str {
    SKILL_PERMISSIONS_NOTE.trim_end()
}

/// The context handed to the child agent that runs a forked skill.
pub fn forked_skill_context(skill: &str, package_root: &str) -> String {
    fill(
        FORKED_SKILL_CONTEXT,
        &[("skill", skill), ("package_root", package_root)],
    )
}

/// Wraps an application note (permission change, step limit) so the model
/// can tell it apart from the user's own words. Sent as a user-role message
/// at its place in the conversation, never merged into the system prompt.
pub fn application_note(text: &str) -> String {
    fill(APPLICATION_NOTE, &[("text", text)])
        .trim_end()
        .to_owned()
}

/// System note for the final model call of a turn that reached its step
/// limit: the model must stop calling tools and report.
pub fn step_limit() -> &'static str {
    STEP_LIMIT.trim_end()
}

/// Application note after a reply the output limit cut off: the model
/// continues the same answer.
pub fn continue_answer() -> &'static str {
    CONTINUE_ANSWER.trim_end()
}

/// Application note after a reply with neither text nor tool calls.
pub fn empty_answer() -> &'static str {
    EMPTY_ANSWER.trim_end()
}

/// Application note after a reply whose reasoning used the whole output
/// limit before any answer text.
pub fn output_limit_without_answer() -> &'static str {
    OUTPUT_LIMIT_WITHOUT_ANSWER.trim_end()
}

/// Application note after a tool call the provider could not parse.
pub fn malformed_tool_call() -> &'static str {
    MALFORMED_TOOL_CALL.trim_end()
}

/// Tool error for a `tool` call the output limit cut off after `received`
/// bytes of arguments.
pub fn truncated_tool_call(tool: &str, received: usize) -> String {
    fill(
        TRUNCATED_TOOL_CALL,
        &[("tool", tool), ("received", &received.to_string())],
    )
}

/// Application note that the task is at `step` of at most `limit` model
/// calls.
pub fn step_budget(step: u32, limit: u32) -> String {
    fill(
        STEP_BUDGET,
        &[("step", &step.to_string()), ("limit", &limit.to_string())],
    )
}

/// First application note about a repeated pattern; `pattern` is one of the
/// `stall_*` descriptions.
pub fn stall_note(pattern: &str) -> String {
    fill(STALL_NOTE, &[("pattern", pattern)])
}

/// Second, final application note about a repeated pattern before the
/// harness stops the task.
pub fn stall_warning(pattern: &str) -> String {
    fill(STALL_WARNING, &[("pattern", pattern)])
}

/// Describes the same call (`tools`, already formatted) repeated `count`
/// times in a row.
pub fn stall_repeated_call(tools: &str, count: usize) -> String {
    fill(
        STALL_REPEATED_CALL,
        &[("tools", tools), ("count", &count.to_string())],
    )
}

/// Describes two steps (`tools`, already formatted) alternating `count`
/// times with the same results.
pub fn stall_alternating(tools: &str, count: usize) -> String {
    fill(
        STALL_ALTERNATING,
        &[("tools", tools), ("count", &count.to_string())],
    )
}

/// Describes `tool` failing with the same error `count` times recently.
pub fn stall_repeated_error(tool: &str, count: usize) -> String {
    fill(
        STALL_REPEATED_ERROR,
        &[("tool", tool), ("count", &count.to_string())],
    )
}

/// One project fact in a [`recalled_context`] note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecalledFact<'a> {
    /// The fact's text; only its first line is shown.
    pub text: &'a str,
    /// Who the fact came from.
    pub origin: MemoryOrigin,
    /// When it was saved, in milliseconds since the Unix epoch.
    pub created_at: i64,
}

/// Longest fact or procedure line in a recalled-context note.
const RECALLED_LINE_CHARS: usize = 160;

/// The note that carries what was retrieved for one user prompt: dated,
/// attributed, quoted fact lines, learned procedures as `(name, title)` and
/// the installed skills the prompt invokes by name as `(id, description)`.
/// Quoting (JSON string syntax with angle brackets escaped) keeps fact text
/// from closing the note or posing as the application's own words. `None`
/// when nothing was recalled.
pub fn recalled_context(
    facts: &[RecalledFact],
    procedures: &[(String, String)],
    skills: &[(String, String)],
) -> Option<String> {
    let facts: Vec<String> = facts
        .iter()
        .filter(|fact| !fact.text.trim().is_empty())
        .map(|fact| {
            format!(
                "- {}, {}: {}",
                utc_date(fact.created_at),
                fact.origin.label(),
                quoted(fact.text)
            )
        })
        .collect();
    let procedures: Vec<String> = procedures
        .iter()
        .map(|(name, title)| format!("- {}: {}", skills::one_line(name, 64), quoted(title)))
        .collect();
    let skills: Vec<String> = skills
        .iter()
        .map(|(id, description)| format!("- {}: {}", skills::one_line(id, 64), quoted(description)))
        .collect();
    let mut sections = Vec::new();
    if !skills.is_empty() {
        sections.push(fill(RECALLED_SKILLS, &[("skills", &skills.join("\n"))]));
    }
    if !facts.is_empty() {
        sections.push(fill(RECALLED_FACTS, &[("facts", &facts.join("\n"))]));
    }
    if !procedures.is_empty() {
        sections.push(fill(
            RECALLED_PROCEDURES,
            &[("procedures", &procedures.join("\n"))],
        ));
    }
    (!sections.is_empty()).then(|| fill(RECALLED_CONTEXT, &[("sections", &sections.join("\n\n"))]))
}

/// The first line of `text`, bounded and quoted as a JSON string with `<`
/// and `>` escaped.
fn quoted(text: &str) -> String {
    let first = text.trim().lines().next().unwrap_or_default();
    let line = skills::one_line(first, RECALLED_LINE_CHARS);
    serde_json::to_string(&line)
        .unwrap_or_default()
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
}

/// Renders a bundled template. Every bundled template and its value names
/// are rendered by the tests, so a failure here is a build defect; it is
/// logged and the unrendered template returned rather than panicking a turn.
fn fill(template: &'static str, values: &[(&str, &str)]) -> String {
    let template = template.trim_end();
    render::render(template, values).unwrap_or_else(|error| {
        tracing::error!(%error, "Bundled prompt template failed to render");
        template.to_owned()
    })
}

#[cfg(test)]
mod tests;
