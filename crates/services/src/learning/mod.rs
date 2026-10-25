//! Scoped, evidence-grounded procedural memory. No model weights are trained and
//! no learned document executes tools or changes permissions.
mod model;
pub use model::*;
mod persistence;
mod sanitize;
pub(crate) use persistence::schema;
#[cfg(test)]
mod tests;
use crate::{ActionIntent, JournalAction, SkillVersion, Store};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workbench_core::{ChatMessage, Conversation, now_ms};

const MAX_STEPS: usize = 16;
const MAX_CASES: usize = 16;
fn digest(value: &impl serde::Serialize) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}
#[derive(Clone)]
pub struct LearningService {
    store: Store,
    owner: Uuid,
}
impl LearningService {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            owner: Uuid::new_v4(),
        }
    }
    /// Dedicated host maintenance task, not a foreground-turn await. It performs
    /// bounded SQLite work only; no provider request, model load or tool execution.
    pub async fn run(&self, cancel: CancellationToken) -> Result<()> {
        let mut before = None;
        let mut backfill = true;
        loop {
            if cancel.is_cancelled() {
                return Ok(());
            }
            if backfill {
                let service = self.clone();
                let page =
                    tokio::task::spawn_blocking(move || service.backfill_completed(before, 32))
                        .await??;
                before = page.0;
                backfill = page.1;
            }
            let batch = self.process_pending(4, &cancel).await?;
            if batch.processed == 0 && !backfill {
                tokio::select! {_ = cancel.cancelled()=>return Ok(()),_ = tokio::time::sleep(Duration::from_secs(2))=>{}}
            } else {
                tokio::task::yield_now().await;
            }
        }
    }
    pub async fn process_pending(
        &self,
        limit: usize,
        cancel: &CancellationToken,
    ) -> Result<LearningBatch> {
        let mut batch = LearningBatch::default();
        for _ in 0..limit.clamp(1, 32) {
            if cancel.is_cancelled() {
                break;
            }
            let service = self.clone();
            let result = tokio::task::spawn_blocking(move || service.process_one()).await??;
            let Some(result) = result else {
                break;
            };
            batch.processed += 1;
            batch.candidates += result.candidates;
            batch.promoted += result.promoted;
            batch.suspended += result.suspended;
            batch.failed_jobs += result.failed_jobs;
        }
        Ok(batch)
    }
    /// Call only after a turn is terminal/reaped. Completion receipts enqueue
    /// automatically. This also records failed/uncertain turns as counterexamples.
    pub fn queue_turn(&self, session: Uuid, turn: Uuid) -> Result<()> {
        self.store.call(move|conn|{conn.execute("INSERT INTO learning_jobs(session_id,turn_id,state,created_at,updated_at) VALUES(?1,?2,'pending',?3,?3) ON CONFLICT(session_id,turn_id) DO UPDATE SET state=CASE WHEN learning_jobs.state='processing' THEN learning_jobs.state ELSE 'pending' END,updated_at=excluded.updated_at",params![session.to_string(),turn.to_string(),now_ms()])?;Ok(())})
    }
    /// Idempotent terminal hook for engine cleanup. Explicit queue_turn additionally
    /// retries already processed evidence after a user reconciliation.
    pub fn queue_terminal_turn(&self, session: Uuid, turn: Uuid) -> Result<()> {
        self.store.call(move|conn|{conn.execute("INSERT OR IGNORE INTO learning_jobs(session_id,turn_id,state,created_at,updated_at) VALUES(?1,?2,'pending',?3,?3)",params![session.to_string(),turn.to_string(),now_ms()])?;Ok(())})
    }
    /// Bounded backwards receipt scan; cursor is append rowid, not wall-clock time.
    pub fn backfill_completed(
        &self,
        before: Option<i64>,
        limit: usize,
    ) -> Result<(Option<i64>, bool)> {
        self.backfill_impl(before, limit)
    }
    pub fn status(&self, scope: &str) -> Result<LearningStatus> {
        self.status_impl(scope)
    }
    pub fn skills(&self, scope: &str, limit: usize) -> Result<Vec<LearnedSkill>> {
        self.list_impl(scope, None, limit)
    }
    pub fn active_skill(&self, scope: &str, name: &str) -> Result<Option<LearnedSkill>> {
        self.active_impl(scope, name)
    }
    pub fn queue_latest_completed(&self, session: Uuid) -> Result<Option<Uuid>> {
        self.queue_latest_impl(session)
    }
    pub fn versions(&self, scope: &str, name: &str, limit: usize) -> Result<Vec<LearnedSkill>> {
        self.list_impl(scope, Some(name), limit)
    }
    pub fn record_correction(&self, input: CorrectionInput) -> Result<LearnedSkill> {
        self.correction_impl(input, None)
    }
    /// Product entry point: check the source conversation's workspace before any
    /// suspension, candidate insertion or promotion-history mutation.
    pub fn record_scoped_correction(
        &self,
        scope: &str,
        input: &CorrectionInput,
    ) -> Result<LearnedSkill> {
        self.correction_impl(input.clone(), Some(scope.to_owned()))
    }
    pub fn relevant_skills(
        &self,
        scope: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<LearnedSkill>> {
        self.relevant_impl(scope, query, limit)
    }
    pub fn record_retrieval(
        &self,
        session: Uuid,
        turn: Uuid,
        skills: &[LearnedSkill],
    ) -> Result<()> {
        self.retrieval_impl(session, turn, skills)
    }
    pub fn reuses(&self, scope: &str, limit: usize) -> Result<Vec<SkillReuse>> {
        self.reuses_impl(scope, limit)
    }
    /// Explicit product review. Re-evaluate current evidence, then append an
    /// auditable promotion; it does not weaken any broker approval requirement.
    pub fn rollback(&self, scope: &str, name: &str, version: i64) -> Result<LearnedSkill> {
        self.rollback_impl(scope, name, version)
    }
    /// Only the host's explicit user-review action may call this; never an agent tool.
    pub fn review(
        &self,
        scope: &str,
        name: &str,
        version: i64,
        evidence: &str,
    ) -> Result<LearnedSkill> {
        self.review_impl(scope, name, version, evidence)
    }
}
