//! Durable next-turn snapshots. Enqueueing does not create a transcript message.
use crate::{DraftSnapshot, DraftSubmission, Store};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use xylo_core::TurnRequest;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueuedTurn {
    pub id: Uuid,
    pub turn: TurnRequest,
    pub paused: bool,
    pub reason: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueueReceipt {
    pub item: QueuedTurn,
    pub draft: DraftSnapshot,
    pub cleared: bool,
}
/// id, session, turn, submission, state, paused, reason, cleared revision.
type QueueRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    bool,
    Option<String>,
    Option<i64>,
);
const MAX_SESSION: i64 = 8;
const MAX_GLOBAL: i64 = 64;
/// Removed/started rows are idempotency tombstones: a retried queue request
/// for a consumed id must not queue it again. They are kept for this window
/// and at most [`MAX_TOMBSTONES`] of them, and never count against the caps.
const TOMBSTONE_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;
const MAX_TOMBSTONES: i64 = 16_384;

fn purge_tombstones(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute(
        "DELETE FROM turn_queue WHERE state IN('removed','started') AND updated_at<?1",
        [xylo_core::now_ms().saturating_sub(TOMBSTONE_RETENTION_MS)],
    )?;
    conn.execute(
        "DELETE FROM turn_queue WHERE sequence IN(SELECT sequence FROM turn_queue WHERE state IN('removed','started') ORDER BY updated_at DESC,sequence DESC LIMIT -1 OFFSET ?1)",
        [MAX_TOMBSTONES],
    )?;
    Ok(())
}
fn read_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<(String, bool, Option<String>)> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
}
fn decode((turn, paused, reason): (String, bool, Option<String>)) -> Result<QueuedTurn> {
    let turn: TurnRequest = serde_json::from_str(&turn)?;
    Ok(QueuedTurn {
        id: turn.turn_id,
        turn,
        paused,
        reason,
    })
}
fn bounded_reason(reason: Option<String>) -> Result<Option<String>> {
    ensure!(
        reason.as_ref().is_none_or(|s| s.len() <= 2048),
        "Queue status is too long"
    );
    Ok(reason)
}
impl Store {
    /// Commit the immutable request and exact draft clear together. A newer draft
    /// wins the clear CAS, while the explicitly submitted snapshot still queues.
    /// This operation never registers a user message or resolves credentials.
    pub fn queue_turn(
        &self,
        turn: &TurnRequest,
        submission: &DraftSubmission,
    ) -> Result<QueueReceipt> {
        ensure!(
            turn.session_id == submission.key.session_id
                && submission.key.workspace.as_ref() == Some(&turn.workspace),
            "Draft belongs to another chat or project"
        );
        ensure!(
            turn.workspace.is_absolute()
                && turn
                    .workspace
                    .to_str()
                    .is_some_and(|s| s.len() <= 8192 && !s.contains('\0')),
            "Choose an absolute project folder"
        );
        ensure!(
            submission.expected_revision >= 0 && submission.text.len() <= crate::MAX_DRAFT_BYTES,
            "Invalid draft revision or size"
        );
        ensure!(
            turn.prompt == submission.text.trim() && turn.attachments == submission.attachments,
            "Submitted draft differs from the queued turn"
        );
        ensure!(
            !turn.prompt.is_empty() || !turn.attachments.is_empty(),
            "Write a message or attach an image before queueing"
        );
        ensure!(
            submission.attachments.len() <= crate::MAX_DRAFT_ATTACHMENTS
                && submission
                    .attachments
                    .iter()
                    .all(|x| x.session_id == turn.session_id),
            "Invalid queued image references"
        );
        ensure!(
            serde_json::to_vec(&submission.attachments)?.len() <= 8192,
            "Queued image references are too large"
        );
        turn.provider.validate()?;
        crate::providers::normalize_provider(&turn.provider)?;
        let turn = turn.clone();
        let submission = submission.clone();
        let turn_json = serde_json::to_string(&turn)?;
        let submission_json = serde_json::to_string(&submission)?;
        ensure!(
            turn_json.len() + submission_json.len() <= 1024 * 1024,
            "Queued message is too large"
        );
        self.call(move |conn| {
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let scope=turn.workspace.to_str().context("Invalid project path")?;
            let existing:Option<QueueRow>=tx.query_row("SELECT id,session_id,turn,submission,state,paused,reason,cleared_revision FROM turn_queue WHERE id=?1 OR (session_id=?2 AND workspace=?3 AND revision=?4) ORDER BY id=?1 DESC LIMIT 1",params![turn.turn_id.to_string(),turn.session_id.to_string(),scope,submission.expected_revision],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?))).optional()?;
            if let Some((id,session,prior_turn,prior_submission,state,paused,reason,cleared_revision))=existing {
                ensure!(session==turn.session_id.to_string(), "Queued message belongs to another chat");
                ensure!(state=="queued", "Message was already removed or started");
                let mut requested=turn.clone();requested.turn_id=Uuid::parse_str(&id)?;
                ensure!(serde_json::from_str::<serde_json::Value>(prior_turn.as_deref().context("Missing queued request")?)?==serde_json::to_value(&requested)? && serde_json::from_str::<serde_json::Value>(prior_submission.as_deref().context("Missing queue submission")?)?==serde_json::to_value(&submission)?, "Queued ID or draft revision was reused with different input");
                let item=decode((prior_turn.unwrap(),paused,reason))?;
                let draft=crate::drafts::snapshot_in_transaction(&tx,&submission.key)?;
                let cleared=draft.is_empty() && cleared_revision==Some(draft.revision);
                return Ok(QueueReceipt {item,draft,cleared});
            }
            let admitted:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE id=?1)",[Uuid::new_v5(&turn.turn_id,b"user-message").to_string()],|r|r.get(0))?;
            ensure!(!admitted,"Message already started; stop the active task");
            let prior_scope:Option<String>=tx.query_row("SELECT json_extract(json,'$.workspace') FROM conversations WHERE id=?1",[turn.session_id.to_string()],|r|r.get(0)).optional()?;
            ensure!(prior_scope.as_deref().is_none_or(|s|s==scope), "This chat belongs to another project");
            let per_session:i64=tx.query_row("SELECT count(*) FROM turn_queue WHERE session_id=?1 AND state='queued'",[turn.session_id.to_string()],|r|r.get(0))?;
            let global:i64=tx.query_row("SELECT count(*) FROM turn_queue WHERE state='queued'",[],|r|r.get(0))?;
            ensure!(per_session<MAX_SESSION,"This chat already has eight queued messages");
            ensure!(global<MAX_GLOBAL,"The application already has 64 queued messages");
            purge_tombstones(&tx)?;
            tx.execute("INSERT INTO turn_queue(id,session_id,workspace,revision,turn,submission,state,updated_at) VALUES(?1,?2,?3,?4,?5,?6,'queued',?7)",params![turn.turn_id.to_string(),turn.session_id.to_string(),scope,submission.expected_revision,turn_json,submission_json,xylo_core::now_ms()])?;
            let previous=crate::drafts::snapshot_in_transaction(&tx,&submission.key)?;
            let draft=crate::drafts::clear_submission_in_transaction(&tx,&submission,turn.turn_id)?;
            let cleared=previous.revision==submission.expected_revision
                && previous.text==submission.text && previous.attachments==submission.attachments
                && previous.revision.checked_add(1)==Some(draft.revision) && draft.is_empty();
            if cleared { tx.execute("UPDATE turn_queue SET cleared_revision=?2 WHERE id=?1",params![turn.turn_id.to_string(),draft.revision])?; }
            tx.commit()?;
            Ok(QueueReceipt {item:QueuedTurn{id:turn.turn_id,turn,paused:false,reason:None},draft,cleared})
        })
    }
    pub fn turn_queue(&self, session: Uuid) -> Result<Vec<QueuedTurn>> {
        self.call(move|conn|{let mut query=conn.prepare("SELECT turn,paused,reason FROM turn_queue WHERE session_id=?1 AND state='queued' ORDER BY sequence")?;let rows=query.query_map([session.to_string()],read_item)?.collect::<rusqlite::Result<Vec<_>>>()?;rows.into_iter().map(decode).collect()})
    }
    pub fn queued_turn(&self, session: Uuid, id: Uuid) -> Result<Option<QueuedTurn>> {
        self.call(move|conn|{conn.query_row("SELECT turn,paused,reason FROM turn_queue WHERE session_id=?1 AND id=?2 AND state='queued'",params![session.to_string(),id.to_string()],read_item).optional()?.map(decode).transpose()})
    }
    /// Keep a small immutable-ID tombstone, discarding the removed snapshot.
    pub fn remove_queued_turn(&self, session: Uuid, id: Uuid) -> Result<()> {
        self.call(move|conn|{let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let state:Option<String>=tx.query_row("SELECT state FROM turn_queue WHERE session_id=?1 AND id=?2",params![session.to_string(),id.to_string()],|r|r.get(0)).optional()?;
            ensure!(state.is_some(),"Queued message does not belong to this chat");
            ensure!(state.as_deref()!=Some("started"),"Message already started; stop the active task");
            tx.execute("UPDATE turn_queue SET state='removed',turn=NULL,submission=NULL,reason=NULL,updated_at=?3 WHERE session_id=?1 AND id=?2",params![session.to_string(),id.to_string(),xylo_core::now_ms()])?;tx.commit()?;Ok(())})
    }
    /// Register the immutable user input and consume its queue receipt in one
    /// commit. The host must reserve worker capacity before calling this method.
    pub fn admit_queued_turn(
        &self,
        session: Uuid,
        id: Uuid,
        effective_turn: &TurnRequest,
    ) -> Result<QueuedTurn> {
        effective_turn.provider.validate()?;
        crate::providers::normalize_provider(&effective_turn.provider)?;
        let effective = effective_turn.clone();
        self.call(move |conn| {
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let row=tx.query_row("SELECT turn,paused,reason FROM turn_queue WHERE session_id=?1 AND id=?2 AND state='queued'",params![session.to_string(),id.to_string()],read_item).optional()?.context("Queued message is unavailable")?;
            let item=decode(row)?;
            let snapshot=&item.turn;
            ensure!(effective.session_id==session && effective.turn_id==id && effective.workspace==snapshot.workspace && effective.prompt==snapshot.prompt && effective.attachments==snapshot.attachments,"Queued admission differs from saved user input");
            let mut provider=effective.provider.clone();
            if snapshot.provider.managed_local && provider.managed_local && provider.endpoint!=snapshot.provider.endpoint {
                let endpoint=url::Url::parse(&provider.endpoint)?;
                ensure!(matches!(endpoint.host_str(),Some("localhost"|"127.0.0.1"|"[::1]")),"Managed model endpoint must remain local");
                provider.endpoint=snapshot.provider.endpoint.clone();
            }
            ensure!(serde_json::to_value(&provider)?==serde_json::to_value(&snapshot.provider)?,"Queued admission cannot change the provider snapshot");
            let prior_scope:Option<String>=tx.query_row("SELECT json_extract(json,'$.workspace') FROM conversations WHERE id=?1",[session.to_string()],|r|r.get(0)).optional()?;
            ensure!(prior_scope.as_deref().is_none_or(|s|Some(s)==effective.workspace.to_str()),"This chat belongs to another project");
            crate::store::register_user_turn_in_transaction(&tx,&effective)?;
            tx.execute("UPDATE turn_queue SET state='started',turn=NULL,submission=NULL,reason=NULL,updated_at=?3 WHERE session_id=?1 AND id=?2 AND state='queued'",params![session.to_string(),id.to_string(),xylo_core::now_ms()])?;
            tx.commit()?;
            Ok(item)
        })
    }
    /// Call only after durable user-turn admission. Never erase an unstarted item.
    pub fn complete_queued_turn(&self, session: Uuid, id: Uuid) -> Result<()> {
        self.call(move|conn|{let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let state:Option<String>=tx.query_row("SELECT state FROM turn_queue WHERE session_id=?1 AND id=?2",params![session.to_string(),id.to_string()],|r|r.get(0)).optional()?;
            ensure!(matches!(state.as_deref(),Some("queued"|"started")),"Queued message is unavailable");
            let registered:Option<String>=tx.query_row("SELECT json FROM messages WHERE id=?1 AND session_id=?2",params![Uuid::new_v5(&id,b"user-message").to_string(),session.to_string()],|r|r.get(0)).optional()?;
            let registered:xylo_core::ChatMessage=serde_json::from_str(registered.as_deref().context("Queued message has not been admitted")?)?;
            if state.as_deref()==Some("queued") {
                let snapshot:String=tx.query_row("SELECT turn FROM turn_queue WHERE id=?1",[id.to_string()],|r|r.get(0))?;
                let snapshot:TurnRequest=serde_json::from_str(&snapshot)?;
                ensure!(registered.role=="user" && registered.session_id==snapshot.session_id && registered.content==snapshot.prompt && registered.attachments==snapshot.attachments,"Admitted message differs from queued input");
            }
            tx.execute("UPDATE turn_queue SET state='started',turn=NULL,submission=NULL,reason=NULL,updated_at=?3 WHERE session_id=?1 AND id=?2",params![session.to_string(),id.to_string(),xylo_core::now_ms()])?;tx.commit()?;Ok(())})
    }
    pub fn pause_turn_queue(&self, session: Uuid, reason: &str) -> Result<()> {
        let reason = bounded_reason(Some(reason.to_owned()))?;
        self.call(move |conn| {
            conn.execute(
                "UPDATE turn_queue SET paused=1,reason=?2 WHERE session_id=?1 AND state='queued'",
                params![session.to_string(), reason],
            )?;
            Ok(())
        })
    }
    pub fn set_queued_turn_paused(
        &self,
        session: Uuid,
        id: Uuid,
        paused: bool,
        reason: Option<String>,
    ) -> Result<()> {
        let reason = bounded_reason(if paused { reason } else { None })?;
        self.call(move|conn|{let changed=conn.execute("UPDATE turn_queue SET paused=?3,reason=?4 WHERE session_id=?1 AND id=?2 AND state='queued'",params![session.to_string(),id.to_string(),paused,reason])?;ensure!(changed==1,"Queued message does not belong to this chat");Ok(())})
    }
    /// Startup-only: pending snapshots never resume automatically after restart.
    /// Also drops expired tombstones.
    pub fn recover_turn_queue(&self) -> Result<usize> {
        self.call(|conn|{purge_tombstones(conn)?;Ok(conn.execute("UPDATE turn_queue SET paused=1,reason='Application restarted; review and run this queued message' WHERE state='queued'",[])?)})
    }
}

#[cfg(test)]
mod tests;
