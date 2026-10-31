//! Evaluation tasks and the checks that grade them.
//!
//! A [`Task`] is a self-contained scenario: workspace fixtures, a prompt, a
//! permission mode, how the simulated user answers approval requests, a
//! script for the offline model and a grader. Fixture text, the prompt and
//! the script may name `{workspace}` and `{outside}`, which resolve to the
//! trial's project directory and to a sibling directory outside the project.
//! Traces store paths in the same placeholder form (plus `{profile}` for the
//! trial's data directory), so a trace replays in any directory.

use crate::{runner::Trial, scripted::Script};
use serde::{Deserialize, Serialize};
use xylo_core::PermissionMode;

/// Placeholder for the trial's workspace directory.
pub const WORKSPACE: &str = "{workspace}";
/// Placeholder for the trial's directory outside the workspace.
pub const OUTSIDE: &str = "{outside}";
/// Placeholder for the trial's profile (database, artifacts) in traces.
pub const PROFILE: &str = "{profile}";

/// One evaluation scenario.
#[derive(Clone)]
pub struct Task {
    /// Stable identifier used on the command line and in reports.
    pub id: &'static str,
    /// One line on what the task measures.
    pub summary: &'static str,
    pub prompt: String,
    pub mode: PermissionMode,
    /// Files created in the workspace, by relative path.
    pub files: Vec<(&'static str, String)>,
    /// Files created outside the workspace, by path relative to `{outside}`.
    pub outside_files: Vec<(&'static str, String)>,
    /// Context window of the scripted model's configuration.
    pub context_tokens: u32,
    /// Output limit of the scripted model's configuration.
    pub output_tokens: u32,
    /// What the offline model answers.
    pub script: Script,
    /// How the simulated user answers approval requests.
    pub approvals: ApprovalPolicy,
    /// Whether the task means anything against a real model. Tasks that
    /// exercise scripted provider faults (truncation, overflow, empty
    /// replies) are offline only.
    pub live: bool,
    /// Grades a finished trial; harness invariants are checked separately.
    pub grade: fn(&Trial) -> Vec<Check>,
}

/// How the simulated user answers approval requests.
#[derive(Clone, Copy, Debug)]
pub enum ApprovalPolicy {
    AllowAll,
    DenyAll,
    /// Allow only these tools; deny everything else.
    AllowOnly(&'static [&'static str]),
}

impl ApprovalPolicy {
    /// Whether the user allows `tool`.
    pub fn allows(&self, tool: &str) -> bool {
        match self {
            Self::AllowAll => true,
            Self::DenyAll => false,
            Self::AllowOnly(tools) => tools.contains(&tool),
        }
    }
}

/// The verdict of one check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Fail,
    /// The engine does not expose what the check needs (for example a
    /// usage field or record attribute it does not have yet). A skip never
    /// fails a trial; reports count skips so they stay visible.
    Skip,
}

/// One named check and why it came out as it did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
}

impl Check {
    /// A check that passes when `holds`.
    pub fn expect(name: &str, holds: bool, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: if holds {
                CheckStatus::Pass
            } else {
                CheckStatus::Fail
            },
            detail: detail.into(),
        }
    }

    /// A check that could not be evaluated.
    pub fn skip(name: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Skip,
            detail: detail.into(),
        }
    }

    /// Whether the check failed.
    pub fn failed(&self) -> bool {
        self.status == CheckStatus::Fail
    }
}
