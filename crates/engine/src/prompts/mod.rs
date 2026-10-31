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
//! files and two compact indexes (skill names with descriptions, fact
//! titles). Skill bodies, skill resources, learned procedures and memory
//! text reach the model only through tools. The system prompt depends only
//! on [`PromptContext`] and its date has day granularity, so it is
//! byte-identical across the calls of a task and stays provider-cacheable.

mod environment;
mod instructions;
mod render;
mod skills;

use std::path::Path;

use xylo_core::PermissionMode;

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
const SYSTEM_FACTS: &str = include_str!("../../prompts/system-facts.md");
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
const TITLE: &str = include_str!("../../prompts/title.md");
const SUBAGENT: &str = include_str!("../../prompts/subagent.md");
const SUBAGENT_TASK: &str = include_str!("../../prompts/subagent-task.md");
const SKILL_PERMISSIONS_NOTE: &str = include_str!("../../prompts/skill-permissions-note.md");
const FORKED_SKILL_CONTEXT: &str = include_str!("../../prompts/forked-skill-context.md");
const STEP_LIMIT: &str = include_str!("../../prompts/step-limit.md");
const APPLICATION_NOTE: &str = include_str!("../../prompts/application-note.md");

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
    /// Output of [`facts_index`]; empty when no facts are retained.
    pub facts_index: String,
}

impl<'a> PromptContext<'a> {
    /// A context for `workspace` with the host environment, repository state,
    /// today's date and the project instruction files filled in. The skill and
    /// fact indexes start empty; set them before calling [`system`].
    pub fn detect(workspace: &'a Path, permission_mode: PermissionMode, now_ms: i64) -> Self {
        PromptContext {
            workspace,
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            shell: crate::tools::shell_name(),
            git: GitInfo::detect(workspace),
            date: utc_date(now_ms),
            permission_mode,
            project_instructions: project_instructions(workspace),
            skills_index: String::new(),
            facts_index: String::new(),
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
    for (template, index) in [
        (SYSTEM_SKILLS, &ctx.skills_index),
        (SYSTEM_FACTS, &ctx.facts_index),
    ] {
        if !index.trim().is_empty() {
            sections.push(fill(template, &[("index", index.trim())]));
        }
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

/// The compact index of retained project facts (titles only, bounded). The
/// full text is recalled with `memory_search`. Empty when there are none.
pub fn facts_index<'t>(titles: impl IntoIterator<Item = &'t str>) -> String {
    /// Most fact titles listed in the system prompt.
    const FACT_TITLES: usize = 24;
    titles
        .into_iter()
        .map(|title| skills::one_line(title, 120))
        .filter(|title| !title.is_empty())
        .take(FACT_TITLES)
        .map(|title| format!("- {title}"))
        .collect::<Vec<_>>()
        .join("\n")
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
