//! Operator-only review of uncertain effects. Observation never authorizes replay.
mod forms;
mod leases;
pub use forms::*;
pub use leases::ActionExecutionLease;

use crate::{JournalAction, Store};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use xylo_core::{Conversation, now_ms};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionReviewScope {
    pub session_id: Uuid,
    pub workspace: PathBuf,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionReviewKey {
    pub updated_at: i64,
    pub id: Uuid,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionReviewCursor {
    pub snapshot: u64,
    pub before: Option<ActionReviewKey>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionReviewSummary {
    pub id: Uuid,
    pub turn_id: Uuid,
    pub name: String,
    pub writes: bool,
    pub state: String,
    pub updated_at: i64,
    pub revision: String,
    pub busy: bool,
    pub blocked_reason: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionReviewPage {
    pub scope: ActionReviewScope,
    pub items: Vec<ActionReviewSummary>,
    pub cursor: Option<ActionReviewCursor>,
    pub has_more: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundedActionPayload {
    pub json: Option<Value>,
    pub preview: String,
    pub bytes: usize,
    pub truncated: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionReviewDetail {
    pub scope: ActionReviewScope,
    pub action: ActionReviewSummary,
    pub arguments: BoundedActionPayload,
    pub result: Option<BoundedActionPayload>,
    pub error: Option<String>,
    pub artifacts: Vec<PathBuf>,
    pub artifacts_truncated: bool,
    pub resolution: Option<ActionResolutionReceipt>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionResolutionOutcome {
    Completed,
    DidNotComplete,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionResolutionRequest {
    pub operation_id: Uuid,
    pub scope: ActionReviewScope,
    pub action_id: Uuid,
    pub expected_revision: String,
    pub outcome: ActionResolutionOutcome,
    pub evidence: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionResolutionReceipt {
    pub operation_id: Uuid,
    pub scope: ActionReviewScope,
    pub action_id: Uuid,
    pub outcome: ActionResolutionOutcome,
    pub evidence: String,
    pub original_revision: String,
    pub revision: String,
    pub created_at: i64,
    pub source: String,
}

pub(super) fn scope(mut scope: ActionReviewScope) -> Result<ActionReviewScope> {
    ensure!(
        !scope.session_id.is_nil() && scope.workspace.is_absolute(),
        "Choose the original chat and project"
    );
    scope.workspace = scope
        .workspace
        .canonicalize()
        .context("The original project is unavailable")?;
    ensure!(
        scope.workspace.is_dir(),
        "The original project is unavailable"
    );
    Ok(scope)
}
pub(super) fn check_scope(conn: &Connection, scope: &ActionReviewScope) -> Result<()> {
    let saved: String = conn
        .query_row(
            "SELECT json FROM conversations WHERE id=?1",
            [scope.session_id.to_string()],
            |r| r.get(0),
        )
        .optional()?
        .context("This chat is unavailable")?;
    let saved: Conversation = serde_json::from_str(&saved)?;
    ensure!(
        saved.workspace.canonicalize()? == scope.workspace,
        "This action belongs to another project"
    );
    Ok(())
}
fn revision(id: Uuid, session: Uuid, state: &str, updated: i64, seq: i64) -> String {
    hex::encode(Sha256::digest(
        serde_json::to_vec(&(id, session, state, updated, seq))
            .expect("Scalar revision serialization"),
    ))
}
type StoredActionRow = (String, String, Option<String>, Option<String>, i64, i64);
fn read(conn: &Connection, id: Uuid, scope: &ActionReviewScope) -> Result<(JournalAction, i64)> {
    let row:Option<StoredActionRow>=conn.query_row("SELECT a.intent,a.state,a.result,a.error,a.updated_at,coalesce(r.seq,0) FROM actions a LEFT JOIN action_review_revisions r ON r.action_id=a.id WHERE a.id=?1 AND a.session_id=?2",params![id.to_string(),scope.session_id.to_string()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
    let (intent, state, result, error, updated_at, seq) =
        row.context("This action is unavailable in the selected chat")?;
    Ok((
        JournalAction {
            intent: serde_json::from_str(&intent)?,
            state,
            result: result.map(|v| serde_json::from_str(&v)).transpose()?,
            error,
            updated_at,
        },
        seq,
    ))
}
fn summary(action: &JournalAction, seq: i64) -> ActionReviewSummary {
    ActionReviewSummary {
        id: action.intent.id,
        turn_id: action.intent.turn_id,
        name: action.intent.name.clone(),
        writes: action.intent.writes,
        state: action.state.clone(),
        updated_at: action.updated_at,
        revision: revision(
            action.intent.id,
            action.intent.session_id,
            &action.state,
            action.updated_at,
            seq,
        ),
        busy: false,
        blocked_reason: None,
    }
}
fn payload(value: &Value) -> Result<BoundedActionPayload> {
    let encoded = serde_json::to_string(value)?;
    let truncated = encoded.len() > 16384;
    let preview = if truncated {
        encoded.chars().take(4096).collect()
    } else {
        encoded.clone()
    };
    Ok(BoundedActionPayload {
        json: (!truncated).then(|| value.clone()),
        preview,
        bytes: encoded.len(),
        truncated,
    })
}
pub(super) fn validate_request(request: &ActionResolutionRequest) -> Result<()> {
    ensure!(
        !request.operation_id.is_nil() && !request.action_id.is_nil(),
        "Choose a valid review identity"
    );
    ensure!(
        request.expected_revision.len() == 64
            && request
                .expected_revision
                .bytes()
                .all(|v| v.is_ascii_hexdigit()),
        "Refresh this action before recording its outcome"
    );
    ensure!(
        !request.evidence.trim().is_empty()
            && request.evidence.len() <= 4096
            && !request.evidence.contains('\0'),
        "Describe the observed outcome in at most 4096 bytes"
    );
    Ok(())
}
fn receipt(
    conn: &Connection,
    scope: &ActionReviewScope,
    id: Uuid,
) -> Result<Option<ActionResolutionReceipt>> {
    let stored: Option<String> = conn
        .query_row(
            "SELECT receipt FROM action_resolutions WHERE operation_id=?1",
            [id.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    let stored: Option<ActionResolutionReceipt> =
        stored.map(|v| serde_json::from_str(&v)).transpose()?;
    ensure!(
        stored.as_ref().is_none_or(|r| r.scope == *scope),
        "This recorded outcome belongs to another chat or project"
    );
    Ok(stored)
}

impl Store {
    /// Small metadata-only index for refreshing current-turn checkpoint content.
    /// Full observations are fetched one at a time only for matching tool calls.
    pub fn operator_reviewed_action_keys(
        &self,
        session: Uuid,
        turn: Uuid,
    ) -> Result<Vec<(Uuid, String)>> {
        self.call(move|conn|{
            let mut statement=conn.prepare("SELECT a.id,substr(a.action_key,1,513) FROM action_resolutions r JOIN actions a ON a.id=r.action_id WHERE a.session_id=?1 AND a.turn_id=?2 ORDER BY a.id LIMIT 1025")?;
            let rows=statement.query_map(params![session.to_string(),turn.to_string()],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?;
            let mut result=Vec::new();for row in rows{let(id,key)=row?;ensure!(key.len()<=512,"The original tool identity is too large to restore safely");result.push((Uuid::parse_str(&id)?,key));}
            ensure!(result.len()<=1024,"Too many observed outcomes to restore safely");Ok(result)
        })
    }
    /// Keyset pages exclude new/changed rows after the initial snapshot. Refresh
    /// starts a new snapshot; each individual mutation also requires exact CAS.
    pub fn action_reviews(
        &self,
        input: ActionReviewScope,
        cursor: Option<ActionReviewCursor>,
        limit: usize,
        cancel: &CancellationToken,
    ) -> Result<ActionReviewPage> {
        let scope = scope(input)?;
        let supplied = cursor.clone();
        let limit = limit.clamp(1, 50);
        let cancel = cancel.clone();
        let (mut page,actions)=self.call(move|conn|{
            check_scope(conn,&scope)?;ensure!(!cancel.is_cancelled(),"Action review cancelled");
            let latest:i64=conn.query_row("SELECT seq FROM action_review_clock WHERE id=1",[],|r|r.get(0))?;
            let snapshot=supplied.as_ref().map_or(latest as u64,|c|c.snapshot);ensure!(snapshot<=latest as u64,"Refresh the action list");
            let key=supplied.and_then(|c|c.before);let time=key.as_ref().map_or(i64::MAX,|k|k.updated_at);let id=key.map_or_else(||"~".into(),|k|k.id.to_string());
            let mut stmt=conn.prepare("SELECT a.id,json_extract(a.intent,'$.turn_id'),json_extract(a.intent,'$.name'),json_extract(a.intent,'$.writes'),a.state,a.updated_at,coalesce(r.seq,0) FROM actions a LEFT JOIN action_review_revisions r ON r.action_id=a.id WHERE a.session_id=?1 AND a.state='outcome_unknown' AND coalesce(r.seq,0)<=?2 AND (a.updated_at<?3 OR (a.updated_at=?3 AND a.id<?4)) ORDER BY a.updated_at DESC,a.id DESC LIMIT ?5")?;
            let rows=stmt.query_map(params![scope.session_id.to_string(),snapshot as i64,time,id,(limit+1) as i64],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,bool>(3)?,r.get::<_,String>(4)?,r.get::<_,i64>(5)?,r.get::<_,i64>(6)?)))?;
            let mut items=Vec::new();for row in rows {let(id,turn,name,writes,state,updated_at,seq)=row?;let id=Uuid::parse_str(&id)?;items.push(ActionReviewSummary{id,turn_id:Uuid::parse_str(&turn)?,name,writes,state:state.clone(),updated_at,revision:revision(id,scope.session_id,&state,updated_at,seq),busy:false,blocked_reason:None});}
            let has_more=items.len()>limit;items.truncate(limit);let cursor=if has_more {items.last().map(|i|ActionReviewCursor{snapshot,before:Some(ActionReviewKey{updated_at:i.updated_at,id:i.id})})}else{None};
            let actions=items.iter().map(|i|i.id).collect::<Vec<_>>();Ok((ActionReviewPage{scope,items,cursor,has_more},actions))
        })?;
        for (item, id) in page.items.iter_mut().zip(actions) {
            (item.busy, item.blocked_reason) = self.action_review_status(&page.scope, id)?;
        }
        Ok(page)
    }
    pub fn action_review(&self, input: ActionReviewScope, id: Uuid) -> Result<ActionReviewDetail> {
        let scope = scope(input)?;
        let saved_scope = scope.clone();
        let mut detail = self.call(move |conn| {
            check_scope(conn, &saved_scope)?;
            let (action, seq) = read(conn, id, &saved_scope)?;
            let all = action
                .result
                .as_ref()
                .and_then(|v| v.get("artifacts"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let artifacts = all
                .iter()
                .filter_map(Value::as_str)
                .filter(|v| v.len() <= 8192)
                .take(64)
                .map(PathBuf::from)
                .collect::<Vec<_>>();
            let resolution: Option<String> = conn
                .query_row(
                    "SELECT receipt FROM action_resolutions WHERE action_id=?1",
                    [id.to_string()],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(ActionReviewDetail {
                scope: saved_scope,
                action: summary(&action, seq),
                arguments: payload(&action.intent.arguments)?,
                result: action.result.as_ref().map(payload).transpose()?,
                error: action.error.map(|v| v.chars().take(2048).collect()),
                artifacts_truncated: all.len() > artifacts.len(),
                artifacts,
                resolution: resolution.map(|v| serde_json::from_str(&v)).transpose()?,
            })
        })?;
        (detail.action.busy, detail.action.blocked_reason) =
            self.action_review_status(&scope, id)?;
        Ok(detail)
    }
    pub fn action_resolution(
        &self,
        input: ActionReviewScope,
        id: Uuid,
    ) -> Result<Option<ActionResolutionReceipt>> {
        let scope = scope(input)?;
        self.call(move |conn| {
            check_scope(conn, &scope)?;
            receipt(conn, &scope, id)
        })
    }
    /// Atomically records an explicit observation and its immutable original.
    /// No native operation is dispatched, cancelled, undone or implicitly retried.
    pub fn resolve_action(
        &self,
        mut request: ActionResolutionRequest,
        cancel: &CancellationToken,
    ) -> Result<ActionResolutionReceipt> {
        request.scope = scope(request.scope)?;
        validate_request(&request)?;
        let existing = self.action_resolution(request.scope.clone(), request.operation_id)?;
        if let Some(existing) = existing {
            let wanted = request.clone();
            self.call(move |conn| {
                let old: String = conn.query_row(
                    "SELECT request FROM action_resolutions WHERE operation_id=?1",
                    [wanted.operation_id.to_string()],
                    |r| r.get(0),
                )?;
                ensure!(
                    serde_json::from_str::<ActionResolutionRequest>(&old)? == wanted,
                    "This review identity was reused with different observations"
                );
                Ok(())
            })?;
            return Ok(existing);
        }
        let action = self
            .action(request.action_id)?
            .context("This action is unavailable")?;
        let _guard = self.action_review_guard(&action)?;
        let cancel = cancel.clone();
        self.call(move|conn|{
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;check_scope(&tx,&request.scope)?;
            if let Some(saved)=receipt(&tx,&request.scope,request.operation_id)? {let old:String=tx.query_row("SELECT request FROM action_resolutions WHERE operation_id=?1",[request.operation_id.to_string()],|r|r.get(0))?;ensure!(serde_json::from_str::<ActionResolutionRequest>(&old)?==request,"This review identity was reused with different observations");return Ok(saved)}
            ensure!(!cancel.is_cancelled(),"Outcome recording cancelled before commit");
            if let Some(pending)=forms::pending(&tx,&request.scope)? {
                ensure!(pending.action_id!=request.action_id||pending==request,"Another observed outcome is already pending for this action; check its recorded outcome first");
            }
            let(action,seq)=read(&tx,request.action_id,&request.scope)?;let original_revision=revision(action.intent.id,action.intent.session_id,&action.state,action.updated_at,seq);
            ensure!(original_revision==request.expected_revision&&action.state=="outcome_unknown","This action changed; refresh it before recording an outcome");
            let original=serde_json::to_string(&action)?;let original_sha256=hex::encode(Sha256::digest(original.as_bytes()));
            let completed=request.outcome==ActionResolutionOutcome::Completed;let timestamp=now_ms();
            let observed=json!({"status":if completed{"user_confirmed"}else{"failed"},"summary":if completed{"User-confirmed completion after inspecting the outcome. Native execution output remains unverified."}else{"The user observed that the action did not complete. Partial effects may remain. The original action must not be replayed."},"operator_observation":{"source":"operator_review","operation_id":request.operation_id,"outcome":request.outcome,"evidence":request.evidence,"observed_at":timestamp,"native_execution_verified":false},"original_observation":action.result,"artifacts":action.result.as_ref().and_then(|v|v.get("artifacts")).cloned().unwrap_or(json!([]))});
            tx.execute("UPDATE actions SET state=?1,result=?2,error=?3,updated_at=?4 WHERE id=?5 AND state='outcome_unknown'",params![if completed{"succeeded"}else{"failed"},serde_json::to_string(&observed)?,format!("Outcome recorded by the user: {}",request.evidence),timestamp,request.action_id.to_string()])?;
            let new_seq:i64=tx.query_row("SELECT seq FROM action_review_revisions WHERE action_id=?1",[request.action_id.to_string()],|r|r.get(0))?;
            let saved=ActionResolutionReceipt{operation_id:request.operation_id,scope:request.scope.clone(),action_id:request.action_id,outcome:request.outcome,evidence:request.evidence.clone(),original_revision,revision:revision(request.action_id,request.scope.session_id,if completed{"succeeded"}else{"failed"},timestamp,new_seq),created_at:timestamp,source:"operator_review".into()};
            tx.execute("INSERT INTO action_resolutions(operation_id,action_id,request,receipt,original,original_sha256) VALUES(?1,?2,?3,?4,?5,?6)",params![request.operation_id.to_string(),request.action_id.to_string(),serde_json::to_string(&request)?,serde_json::to_string(&saved)?,original,original_sha256])?;
            tx.commit()?;Ok(saved)
        })
    }
}

#[cfg(test)]
mod tests;
