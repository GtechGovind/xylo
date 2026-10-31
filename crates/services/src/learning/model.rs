use crate::SkillVersion;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillRef {
    pub name: String,
    pub version: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcedureStep {
    pub tool: String,
    /// Advisory, typed arguments. A {"$input":"field"} value needs task input.
    /// No component interprets or executes this document as a command.
    pub arguments: Value,
    pub requires_approval: bool,
    pub verification: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LearningEvidence {
    pub session_id: Uuid,
    pub turn_id: Uuid,
    pub action_ids: Vec<Uuid>,
    pub result_digests: Vec<String>,
    pub completed_ordinal: Option<i64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvaluationCase {
    pub session_id: Uuid,
    pub turn_id: Uuid,
    pub passed: bool,
    pub reason: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LearningEvaluation {
    pub evaluator: String,
    pub passed: bool,
    pub score: f64,
    pub checks: Vec<String>,
    pub cases: Vec<EvaluationCase>,
    pub evaluated_at: i64,
    /// Outcome/template consistency is not a task-quality or model benchmark.
    pub limitation: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LearnedSkill {
    pub skill: SkillVersion,
    pub scope: String,
    pub title: String,
    pub state: String,
    pub triggers: Vec<String>,
    pub steps: Vec<ProcedureStep>,
    pub provenance: Vec<LearningEvidence>,
    pub correction_message_id: Option<Uuid>,
    pub correction_guidance: Option<String>,
    pub correction_command: Option<Value>,
    pub review: Option<SkillReview>,
    pub evaluation: Option<LearningEvaluation>,
    pub(super) workflow_key: Option<String>,
    pub(super) after_ordinal: i64,
    pub(super) expected_tools: Vec<String>,
    pub(super) after_case_id: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkillReview {
    pub evidence: String,
    pub reviewed_at: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CorrectionInput {
    pub user_message_id: Uuid,
    /// Must occur verbatim in the persisted user message, never a tool result.
    pub guidance: String,
    #[serde(default)]
    pub expected_tools: Vec<String>,
    #[serde(default)]
    pub replaces: Option<SkillRef>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LearningBatch {
    pub processed: usize,
    pub candidates: usize,
    pub promoted: usize,
    pub suspended: usize,
    pub failed_jobs: usize,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LearningStatus {
    pub pending_jobs: usize,
    pub failed_jobs: usize,
    pub candidates: usize,
    pub promoted: usize,
    pub verified_reuses: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkillReuse {
    pub session_id: Uuid,
    pub turn_id: Uuid,
    pub skill: SkillRef,
    pub outcome: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct ObservedCase {
    pub scope: String,
    pub terms: Vec<String>,
    pub workflow_key: String,
    pub steps: Vec<ProcedureStep>,
    pub evidence: LearningEvidence,
    pub eligible: bool,
    pub reason: String,
}
