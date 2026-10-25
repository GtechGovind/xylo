//! Explicit native execution targets. Profile revisions bind command approval
//! to one stored identity; no executable or target override travels with a run.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerRuntime {
    Docker,
    Podman,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContainerTarget {
    Existing { target: String },
    Managed { image: String, command: Vec<String> },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EnvironmentTarget {
    Ssh {
        executable: PathBuf,
        host: String,
        user: String,
        port: u16,
        known_hosts: PathBuf,
        identity_file: Option<PathBuf>,
    },
    Container {
        executable: PathBuf,
        runtime: ContainerRuntime,
        container: ContainerTarget,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentProfile {
    pub id: Uuid,
    pub name: String,
    pub target: EnvironmentTarget,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentRecord {
    pub profile: EnvironmentProfile,
    pub revision: u64,
    /// Explicit application selection, verified by a live probe before use.
    /// Listing profiles never starts clients or claims current reachability.
    pub connected: bool,
    pub container_id: Option<String>,
    pub updated_at: i64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentState {
    Connected,
    Disconnected,
    Stopped,
    Missing,
    OutcomeUnknown,
    Unavailable,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnvironmentStatus {
    pub environment: EnvironmentRecord,
    pub state: EnvironmentState,
    pub message: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentAction {
    Save,
    Remove,
    Connect,
    Disconnect,
    Create,
    Start,
    Stop,
    RemoveContainer,
    Execute,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentOperationState {
    Prepared,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    OutcomeUnknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentOutputStream {
    Stdout,
    Stderr,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentCaptureReason {
    Eof,
    Cancelled,
    TimedOut,
    ByteLimit,
    ReadError,
    WriteError,
    Interrupted,
}
/// A verified raw stream artifact. Completeness concerns captured client bytes,
/// independently of command exit status or remote side-effect verification.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnvironmentOutputArtifact {
    pub stream: EnvironmentOutputStream,
    pub path: PathBuf,
    pub sha256: String,
    pub stored_bytes: u64,
    pub observed_bytes: u64,
    /// False after an interrupted capture when only a retained lower bound is
    /// provable; never infer discarded bytes from a partially written file.
    #[serde(default)]
    pub observed_bytes_exact: bool,
    pub complete: bool,
    pub reason: EnvironmentCaptureReason,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnvironmentOutput {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    #[serde(default)]
    pub artifacts: Vec<EnvironmentOutputArtifact>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnvironmentReceipt {
    pub operation_id: Uuid,
    pub environment_id: Uuid,
    pub action: EnvironmentAction,
    pub state: EnvironmentOperationState,
    pub environment: Option<EnvironmentRecord>,
    pub output: Option<EnvironmentOutput>,
    pub message: String,
    #[serde(default)]
    pub created_at_ms: Option<i64>,
    #[serde(default)]
    pub command_label: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentSaveRequest {
    pub operation_id: Uuid,
    pub expected_revision: u64,
    pub profile: EnvironmentProfile,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentActionRequest {
    pub operation_id: Uuid,
    pub environment_id: Uuid,
    pub expected_revision: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentExecuteRequest {
    pub operation_id: Uuid,
    pub environment_id: Uuid,
    pub expected_revision: u64,
    pub command: Vec<String>,
    pub timeout_seconds: Option<u64>,
}
