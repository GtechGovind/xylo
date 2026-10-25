//! Durable identified user guidance. Queued is not delivered. Applied means the
//! exact user message and its checkpoint committed together, not model compliance.
use crate::{DraftSnapshot, DraftSubmission, Store};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;
use workbench_core::{
    ChatMessage, MAX_STEERING_BYTES, SteeringReceipt, SteeringRequest, SteeringState, now_ms,
};
const MAX_PER_TURN: i64 = 64;
const MAX_TURN_BYTES: i64 = 128 * 1024;
const MAX_RECORDS: i64 = 16384;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SteeringOutcome {
    /// Canonical ID can differ after deduplicating the same draft revision.
    pub request: SteeringRequest,
    pub receipt: SteeringReceipt,
    pub draft: DraftSnapshot,
    /// Exact empty revision committed by this guidance, if its draft CAS won.
    #[serde(default)]
    pub cleared_revision: Option<i64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SteeringApplication {
    pub messages: Value,
    pub applied: Vec<SteeringOutcome>,
    pub has_more: bool,
}
fn initialize(conn: &mut Connection) -> Result<()> {
    crate::drafts::initialize(conn)?;
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='user_guidance')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS user_guidance(id TEXT PRIMARY KEY,session_id TEXT NOT NULL,turn_id TEXT NOT NULL,workspace TEXT NOT NULL,draft_revision INTEGER NOT NULL,text TEXT NOT NULL,submission TEXT NOT NULL,state TEXT NOT NULL CHECK(state IN('queued','applied','cancelled')),reason TEXT,created_at INTEGER NOT NULL,applied_at INTEGER,cleared_revision INTEGER,UNIQUE(session_id,turn_id,workspace,draft_revision));CREATE INDEX IF NOT EXISTS user_guidance_pending ON user_guidance(session_id,turn_id,state,created_at,id);")?;
        tx.commit()?;
    }
    let has_clear:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('user_guidance') WHERE name='cleared_revision')",[],|row|row.get(0))?;
    if !has_clear {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('user_guidance') WHERE name='cleared_revision')",[],|row|row.get(0))?;
        if !exists {
            tx.execute_batch("ALTER TABLE user_guidance ADD COLUMN cleared_revision INTEGER")?;
        }
        tx.commit()?;
    }
    Ok(())
}
struct Record {
    request: SteeringRequest,
    submission: DraftSubmission,
    state: SteeringState,
    reason: Option<String>,
    cleared_revision: Option<i64>,
}
type GuidanceRow = (
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<i64>,
);
fn record(conn: &Connection, id: Uuid) -> Result<Option<Record>> {
    let row: Option<GuidanceRow> = conn
        .query_row(
            "SELECT session_id,turn_id,text,submission,state,reason,cleared_revision FROM user_guidance WHERE id=?1",
            [id.to_string()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .optional()?;
    row.map(
        |(session, turn, text, submission, state, reason, cleared_revision)| {
            Ok(Record {
                request: SteeringRequest {
                    id,
                    session_id: Uuid::parse_str(&session)?,
                    turn_id: Uuid::parse_str(&turn)?,
                    text,
                },
                submission: serde_json::from_str(&submission)?,
                state: match state.as_str() {
                    "queued" => SteeringState::Queued,
                    "applied" => SteeringState::Applied,
                    "cancelled" => SteeringState::Cancelled,
                    _ => anyhow::bail!("Invalid guidance state"),
                },
                reason,
                cleared_revision,
            })
        },
    )
    .transpose()
}
fn outcome(conn: &Connection, record: Record) -> Result<SteeringOutcome> {
    let draft = crate::drafts::snapshot_in_transaction(conn, &record.submission.key)?;
    let request = record.request;
    Ok(SteeringOutcome {
        receipt: SteeringReceipt {
            id: request.id,
            session_id: request.session_id,
            turn_id: request.turn_id,
            state: record.state,
            reason: record.reason,
        },
        request,
        draft,
        cleared_revision: record.cleared_revision,
    })
}
pub fn guidance_name(id: Uuid) -> String {
    format!("workbench_guidance_{}", id.simple())
}
impl Store {
    /// Startup-only recovery, before accepting work or spawning new workers.
    pub fn recover_steering(&self) -> Result<usize> {
        self.call(|conn| {
            initialize(conn)?;
            Ok(conn.execute(
                "UPDATE user_guidance SET state='cancelled',
                 reason='Application restarted before guidance was added' WHERE state='queued'",
                [],
            )?)
        })
    }

    /// Host calls after validating a live target and reserving dispatch capacity.
    /// The draft is not cleared. Repeated IDs/revisions reuse the canonical row.
    pub fn queue_steering(
        &self,
        request: &SteeringRequest,
        submission: &DraftSubmission,
    ) -> Result<SteeringOutcome> {
        ensure!(
            !request.text.trim().is_empty() && request.text.len() <= MAX_STEERING_BYTES,
            "Keep guidance between 1 and 16384 bytes"
        );
        ensure!(
            request.session_id == submission.key.session_id
                && request.text == submission.text.trim()
                && submission.attachments.is_empty(),
            "Guidance does not match this text draft"
        );
        let workspace = submission
            .key
            .workspace
            .as_ref()
            .and_then(|path| path.to_str())
            .context("Choose a project before guiding a task")?
            .to_owned();
        let request = request.clone();
        let submission = submission.clone();
        self.call(move |conn| {
            initialize(conn)?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if let Some(previous) = record(&tx, request.id)? {
                ensure!(
                    previous.request == request
                        && previous.submission.key == submission.key
                        && previous.submission.expected_revision == submission.expected_revision
                        && previous.submission.text == submission.text,
                    "Guidance ID was reused for different input"
                );
                let result = outcome(&tx, previous)?;
                tx.commit()?;
                return Ok(result);
            }
            let prior: Option<String> = tx.query_row(
                "SELECT id FROM user_guidance WHERE session_id=?1 AND turn_id=?2
                 AND workspace=?3 AND draft_revision=?4",
                params![request.session_id.to_string(), request.turn_id.to_string(), workspace, submission.expected_revision],
                |row| row.get(0),
            ).optional()?;
            if let Some(id) = prior {
                let previous = record(&tx, Uuid::parse_str(&id)?)?
                    .context("Guidance record disappeared")?;
                ensure!(
                    previous.request.text == request.text && previous.submission.text == submission.text,
                    "This draft revision already has different guidance"
                );
                let result = outcome(&tx, previous)?;
                tx.commit()?;
                return Ok(result);
            }
            let draft = crate::drafts::snapshot_in_transaction(&tx, &submission.key)?;
            ensure!(
                draft.revision == submission.expected_revision && draft.text == submission.text
                    && draft.attachments.is_empty(),
                "This draft changed before guidance could be queued"
            );
            let scope: Option<String> = tx.query_row(
                "SELECT json_extract(json,'$.workspace') FROM conversations WHERE id=?1",
                [request.session_id.to_string()], |row| row.get(0),
            ).optional()?;
            ensure!(scope.as_deref() == Some(&workspace), "The active task belongs to another project");
            let (count, bytes): (i64, i64) = tx.query_row(
                "SELECT count(*),COALESCE(sum(length(CAST(text AS BLOB))),0)
                 FROM user_guidance WHERE session_id=?1 AND turn_id=?2",
                params![request.session_id.to_string(), request.turn_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            ensure!(
                count < MAX_PER_TURN && bytes + request.text.len() as i64 <= MAX_TURN_BYTES,
                "This task has reached its guidance limit; start another turn"
            );
            let total: i64 = tx.query_row("SELECT count(*) FROM user_guidance", [], |row| row.get(0))?;
            ensure!(total < MAX_RECORDS, "Saved guidance has reached its storage limit");
            tx.execute(
                "INSERT INTO user_guidance(id,session_id,turn_id,workspace,draft_revision,text,submission,state,created_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,'queued',?8)",
                params![request.id.to_string(), request.session_id.to_string(), request.turn_id.to_string(),
                    workspace, submission.expected_revision, request.text, serde_json::to_string(&submission)?, now_ms()],
            )?;
            let result = outcome(&tx, record(&tx, request.id)?.context("Guidance was not saved")?)?;
            tx.commit()?;
            Ok(result)
        })
    }

    pub fn steering(&self, id: Uuid) -> Result<Option<SteeringOutcome>> {
        self.call(move |conn| {
            initialize(conn)?;
            record(conn, id)?
                .map(|record| outcome(conn, record))
                .transpose()
        })
    }

    pub fn steering_for_turn(
        &self,
        session: Uuid,
        turn: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<SteeringOutcome>> {
        self.call(move |conn| {
            initialize(conn)?;
            let mut statement = conn.prepare(
                "SELECT id FROM user_guidance WHERE session_id=?1 AND(?2 IS NULL OR turn_id=?2)
                 ORDER BY created_at DESC,id DESC LIMIT ?3",
            )?;
            let ids = statement
                .query_map(
                    params![
                        session.to_string(),
                        turn.map(|turn| turn.to_string()),
                        limit.clamp(1, 128) as i64
                    ],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            ids.into_iter()
                .map(|id| {
                    outcome(
                        conn,
                        record(conn, Uuid::parse_str(&id)?)?.context("Guidance disappeared")?,
                    )
                })
                .collect()
        })
    }

    /// Atomic application at a role-safe engine boundary. The checkpoint and user
    /// history are saved before receipts become observable. No tool executes here.
    pub fn apply_steering(
        &self,
        session: Uuid,
        turn: Uuid,
        messages: &Value,
    ) -> Result<SteeringApplication> {
        let mut messages = messages
            .as_array()
            .context("Guidance needs a message checkpoint")?
            .clone();
        self.call(move |conn| {
            initialize(conn)?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut statement = tx.prepare(
                "SELECT id FROM user_guidance WHERE session_id=?1 AND turn_id=?2 AND state='queued'
                 ORDER BY created_at,id LIMIT 8",
            )?;
            let ids = statement.query_map(params![session.to_string(),turn.to_string()], |row|row.get::<_,String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            if ids.is_empty() {
                tx.commit()?;
                return Ok(SteeringApplication { messages:Value::Array(messages), applied:Vec::new(), has_more:false });
            }
            let checkpoint: Option<String> = tx.query_row(
                "SELECT turn_id FROM checkpoints WHERE session_id=?1", [session.to_string()], |row|row.get(0),
            ).optional()?;
            ensure!(checkpoint.as_deref()==Some(&turn.to_string()),"Guidance target checkpoint is no longer active");
            let mut applied = Vec::new();
            for id in ids {
                let id = Uuid::parse_str(&id)?;
                let record = record(&tx,id)?.context("Guidance disappeared")?;
                let name = guidance_name(id);
                if let Some(existing) = messages.iter().find(|message|message.get("name").and_then(Value::as_str)==Some(&name)) {
                    ensure!(existing["role"]=="user" && existing["content"]==record.request.text,"Guidance marker has conflicting content");
                } else {
                    messages.push(json!({"role":"user","content":record.request.text,"name":name}));
                }
                let timestamp = now_ms();
                let message = ChatMessage {
                    id:Uuid::new_v5(&id,b"steering-message"), session_id:session, role:"user".into(),
                    content:record.request.text.clone(), created_at:timestamp, attachments:Vec::new(),
                };
                tx.execute(
                    "INSERT OR IGNORE INTO messages(id,session_id,json,created_at) VALUES(?1,?2,?3,?4)",
                    params![message.id.to_string(),session.to_string(),serde_json::to_string(&message)?,timestamp],
                )?;
                let before = crate::drafts::snapshot_in_transaction(&tx,&record.submission.key)?;
                let draft = crate::drafts::clear_submission_in_transaction(&tx,&record.submission,id)?;
                let cleared_revision = (draft.revision > before.revision).then_some(draft.revision);
                tx.execute(
                    "UPDATE user_guidance SET state='applied',reason=NULL,applied_at=?1,cleared_revision=?3 WHERE id=?2",
                    params![timestamp,id.to_string(),cleared_revision],
                )?;
                applied.push(SteeringOutcome {
                    receipt:SteeringReceipt {id,session_id:session,turn_id:turn,state:SteeringState::Applied,reason:None},
                    request:record.request,draft,cleared_revision,
                });
            }
            tx.execute(
                "UPDATE checkpoints SET turn_id=?1,messages=?2,updated_at=?3 WHERE session_id=?4",
                params![turn.to_string(),serde_json::to_string(&messages)?,now_ms(),session.to_string()],
            )?;
            let has_more: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM user_guidance WHERE session_id=?1 AND turn_id=?2 AND state='queued')",
                params![session.to_string(),turn.to_string()], |row|row.get(0),
            )?;
            tx.commit()?;
            Ok(SteeringApplication {messages:Value::Array(messages),applied,has_more})
        })
    }

    /// Finish/reap never labels undelivered guidance applied. Applied receipts are
    /// immutable; queued guidance retains its text draft for explicit user retry.
    pub fn finish_pending_steering(
        &self,
        session: Uuid,
        turn: Option<Uuid>,
        reason: &str,
    ) -> Result<Vec<SteeringOutcome>> {
        let reason = reason.chars().take(256).collect::<String>();
        self.call(move |conn| {
            initialize(conn)?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut statement = tx.prepare(
                "SELECT id FROM user_guidance WHERE session_id=?1 AND(?2 IS NULL OR turn_id=?2)
                 AND state='queued' ORDER BY created_at,id LIMIT 64",
            )?;
            let ids = statement.query_map(params![session.to_string(),turn.map(|turn|turn.to_string())], |row|row.get::<_,String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            let mut outcomes = Vec::new();
            for id in ids {
                tx.execute("UPDATE user_guidance SET state='cancelled',reason=?1 WHERE id=?2 AND state='queued'",params![reason,id])?;
                outcomes.push(outcome(&tx,record(&tx,Uuid::parse_str(&id)?)?.context("Guidance disappeared")?)?);
            }
            tx.commit()?;
            Ok(outcomes)
        })
    }
}

#[cfg(test)]
#[path = "steering/tests.rs"]
mod tests;
