//! Durable next-turn snapshots. Enqueueing does not create a transcript message.
use crate::{DraftSnapshot, DraftSubmission, Store};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
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
const MAX_SESSION: i64 = 8;
const MAX_GLOBAL: i64 = 64;
const MAX_LEDGER: i64 = 16_384;
fn initialize(conn: &mut Connection) -> Result<()> {
    crate::drafts::initialize(conn)?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS turn_queue(sequence INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT NOT NULL UNIQUE,session_id TEXT NOT NULL,workspace TEXT NOT NULL,revision INTEGER NOT NULL,turn TEXT,submission TEXT,state TEXT NOT NULL CHECK(state IN('queued','removed','started')),paused INTEGER NOT NULL DEFAULT 0,reason TEXT,cleared_revision INTEGER,UNIQUE(session_id,workspace,revision));CREATE INDEX IF NOT EXISTS turn_queue_fifo ON turn_queue(session_id,state,sequence);")?;
    let has_clear:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('turn_queue') WHERE name='cleared_revision')",[],|row|row.get(0))?;
    if !has_clear {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('turn_queue') WHERE name='cleared_revision')",[],|row|row.get(0))?;
        if !exists {
            tx.execute_batch("ALTER TABLE turn_queue ADD COLUMN cleared_revision INTEGER")?;
        }
        tx.commit()?;
    }
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
            initialize(conn)?;
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let scope=turn.workspace.to_str().context("Invalid project path")?;
            let existing:Option<(String,String,Option<String>,Option<String>,String,bool,Option<String>,Option<i64>)>=tx.query_row("SELECT id,session_id,turn,submission,state,paused,reason,cleared_revision FROM turn_queue WHERE id=?1 OR (session_id=?2 AND workspace=?3 AND revision=?4) ORDER BY id=?1 DESC LIMIT 1",params![turn.turn_id.to_string(),turn.session_id.to_string(),scope,submission.expected_revision],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?))).optional()?;
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
            let ledger:i64=tx.query_row("SELECT count(*) FROM turn_queue",[],|r|r.get(0))?;
            ensure!(per_session<MAX_SESSION,"This chat already has eight queued messages");
            ensure!(global<MAX_GLOBAL,"The application already has 64 queued messages");
            ensure!(ledger<MAX_LEDGER,"Queue receipt storage is full");
            tx.execute("INSERT INTO turn_queue(id,session_id,workspace,revision,turn,submission,state) VALUES(?1,?2,?3,?4,?5,?6,'queued')",params![turn.turn_id.to_string(),turn.session_id.to_string(),scope,submission.expected_revision,turn_json,submission_json])?;
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
        self.call(move|conn|{initialize(conn)?;let mut query=conn.prepare("SELECT turn,paused,reason FROM turn_queue WHERE session_id=?1 AND state='queued' ORDER BY sequence")?;let rows=query.query_map([session.to_string()],read_item)?.collect::<rusqlite::Result<Vec<_>>>()?;rows.into_iter().map(decode).collect()})
    }
    pub fn queued_turn(&self, session: Uuid, id: Uuid) -> Result<Option<QueuedTurn>> {
        self.call(move|conn|{initialize(conn)?;conn.query_row("SELECT turn,paused,reason FROM turn_queue WHERE session_id=?1 AND id=?2 AND state='queued'",params![session.to_string(),id.to_string()],read_item).optional()?.map(decode).transpose()})
    }
    /// Keep a small immutable-ID tombstone, discarding the removed snapshot.
    pub fn remove_queued_turn(&self, session: Uuid, id: Uuid) -> Result<()> {
        self.call(move|conn|{initialize(conn)?;let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let state:Option<String>=tx.query_row("SELECT state FROM turn_queue WHERE session_id=?1 AND id=?2",params![session.to_string(),id.to_string()],|r|r.get(0)).optional()?;
            ensure!(state.is_some(),"Queued message does not belong to this chat");
            ensure!(state.as_deref()!=Some("started"),"Message already started; stop the active task");
            tx.execute("UPDATE turn_queue SET state='removed',turn=NULL,submission=NULL,reason=NULL WHERE session_id=?1 AND id=?2",params![session.to_string(),id.to_string()])?;tx.commit()?;Ok(())})
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
            initialize(conn)?;
            let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let row=tx.query_row("SELECT turn,paused,reason FROM turn_queue WHERE session_id=?1 AND id=?2 AND state='queued'",params![session.to_string(),id.to_string()],read_item).optional()?.context("Queued message is unavailable")?;
            let item=decode(row)?;
            let snapshot=&item.turn;
            ensure!(effective.session_id==session && effective.turn_id==id && effective.workspace==snapshot.workspace && effective.prompt==snapshot.prompt && effective.attachments==snapshot.attachments,"Queued admission differs from saved user input");
            let rank=|mode:xylo_core::PermissionMode| match mode {
                xylo_core::PermissionMode::ReadOnly=>0,
                xylo_core::PermissionMode::AskForWrites=>1,
                xylo_core::PermissionMode::AcceptEdits=>2,
                xylo_core::PermissionMode::Auto=>3,
            };
            ensure!(rank(effective.permissions)<=rank(snapshot.permissions),"Queued admission cannot increase permissions");
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
            tx.execute("UPDATE turn_queue SET state='started',turn=NULL,submission=NULL,reason=NULL WHERE session_id=?1 AND id=?2 AND state='queued'",params![session.to_string(),id.to_string()])?;
            tx.commit()?;
            Ok(item)
        })
    }
    /// Call only after durable user-turn admission. Never erase an unstarted item.
    pub fn complete_queued_turn(&self, session: Uuid, id: Uuid) -> Result<()> {
        self.call(move|conn|{initialize(conn)?;let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let state:Option<String>=tx.query_row("SELECT state FROM turn_queue WHERE session_id=?1 AND id=?2",params![session.to_string(),id.to_string()],|r|r.get(0)).optional()?;
            ensure!(matches!(state.as_deref(),Some("queued"|"started")),"Queued message is unavailable");
            let registered:Option<String>=tx.query_row("SELECT json FROM messages WHERE id=?1 AND session_id=?2",params![Uuid::new_v5(&id,b"user-message").to_string(),session.to_string()],|r|r.get(0)).optional()?;
            let registered:xylo_core::ChatMessage=serde_json::from_str(registered.as_deref().context("Queued message has not been admitted")?)?;
            if state.as_deref()==Some("queued") {
                let snapshot:String=tx.query_row("SELECT turn FROM turn_queue WHERE id=?1",[id.to_string()],|r|r.get(0))?;
                let snapshot:TurnRequest=serde_json::from_str(&snapshot)?;
                ensure!(registered.role=="user" && registered.session_id==snapshot.session_id && registered.content==snapshot.prompt && registered.attachments==snapshot.attachments,"Admitted message differs from queued input");
            }
            tx.execute("UPDATE turn_queue SET state='started',turn=NULL,submission=NULL,reason=NULL WHERE session_id=?1 AND id=?2",params![session.to_string(),id.to_string()])?;tx.commit()?;Ok(())})
    }
    pub fn pause_turn_queue(&self, session: Uuid, reason: &str) -> Result<()> {
        let reason = bounded_reason(Some(reason.to_owned()))?;
        self.call(move |conn| {
            initialize(conn)?;
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
        self.call(move|conn|{initialize(conn)?;let changed=conn.execute("UPDATE turn_queue SET paused=?3,reason=?4 WHERE session_id=?1 AND id=?2 AND state='queued'",params![session.to_string(),id.to_string(),paused,reason])?;ensure!(changed==1,"Queued message does not belong to this chat");Ok(())})
    }
    /// Startup-only: pending snapshots never resume automatically after restart.
    pub fn recover_turn_queue(&self) -> Result<usize> {
        self.call(|conn|{initialize(conn)?;Ok(conn.execute("UPDATE turn_queue SET paused=1,reason='Application restarted; review and run this queued message' WHERE state='queued'",[])?)})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DraftKey, DraftSave, DraftWrite};
    use xylo_core::{PermissionMode, ProviderConfig};
    fn fixture() -> (tempfile::TempDir, Store, DraftKey) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("queue.db")).unwrap();
        let key = DraftKey {
            session_id: Uuid::new_v4(),
            workspace: Some(dir.path().to_path_buf()),
        };
        (dir, store, key)
    }
    fn draft(store: &Store, key: &DraftKey, text: &str) -> DraftSubmission {
        let current = store.draft(key).unwrap();
        let DraftSave::Saved(saved) = store
            .save_draft(&DraftWrite {
                key: key.clone(),
                expected_revision: current.revision,
                mutation_id: Uuid::new_v4(),
                text: text.into(),
                attachments: vec![],
            })
            .unwrap()
        else {
            panic!("conflict")
        };
        DraftSubmission {
            key: key.clone(),
            expected_revision: saved.revision,
            text: saved.text,
            attachments: saved.attachments,
        }
    }
    fn turn(submission: &DraftSubmission) -> TurnRequest {
        TurnRequest {
            session_id: submission.key.session_id,
            turn_id: Uuid::new_v4(),
            prompt: submission.text.trim().into(),
            workspace: submission.key.workspace.clone().unwrap(),
            provider: ProviderConfig {
                model: "fixture-model".into(),
                ..ProviderConfig::default()
            },
            permissions: PermissionMode::ReadOnly,
            attachments: submission.attachments.clone(),
        }
    }
    #[test]
    fn exact_draft_moves_atomically_without_registering_user_message() {
        let (_dir, store, key) = fixture();
        let submission = draft(&store, &key, "  next task  ");
        let request = turn(&submission);
        let receipt = store.queue_turn(&request, &submission).unwrap();
        assert!(receipt.cleared);
        assert!(receipt.draft.is_empty());
        assert_eq!(receipt.draft.revision, submission.expected_revision + 1);
        assert_eq!(
            store.turn_queue(key.session_id).unwrap()[0].id,
            request.turn_id
        );
        assert!(store.conversations(10).unwrap().is_empty());
        let newer = draft(&store, &key, "newer text");
        let duplicate = store.queue_turn(&request, &submission).unwrap();
        assert!(!duplicate.cleared);
        assert_eq!(duplicate.draft.text, newer.text);
    }
    #[test]
    fn lost_ack_reuses_only_its_owned_clear_revision() {
        let (_dir, store, key) = fixture();
        let sub = draft(&store, &key, "next");
        let req = turn(&sub);
        let first = store.queue_turn(&req, &sub).unwrap();
        assert!(first.cleared);
        let retry = turn(&sub);
        let receipt = store.queue_turn(&retry, &sub).unwrap();
        assert_eq!(receipt.item.id, req.turn_id);
        assert!(
            receipt.cleared,
            "canonical retry must acknowledge its owned clear"
        );
        draft(&store, &key, "newer draft");
        let empty = draft(&store, &key, "");
        let receipt = store.queue_turn(&retry, &sub).unwrap();
        assert!(
            !receipt.cleared,
            "a later empty mutation is not owned by this queue"
        );
        assert_eq!(receipt.draft.revision, empty.expected_revision);
        assert_eq!(store.turn_queue(key.session_id).unwrap().len(), 1);
    }
    #[test]
    fn old_queue_schema_migrates_without_inventing_clear_ownership() {
        let (dir, store, key) = fixture();
        let sub = draft(&store, &key, "next");
        let req = turn(&sub);
        store.queue_turn(&req, &sub).unwrap();
        store
            .call(|conn| {
                conn.execute_batch("ALTER TABLE turn_queue DROP COLUMN cleared_revision")?;
                Ok(())
            })
            .unwrap();
        drop(store);
        let store = Store::open(dir.path().join("queue.db")).unwrap();
        let receipt = store.queue_turn(&turn(&sub), &sub).unwrap();
        assert!(
            !receipt.cleared,
            "legacy rows have no proof of an owned draft clear"
        );
        assert_eq!(receipt.item.id, req.turn_id);
        assert_eq!(store.turn_queue(key.session_id).unwrap().len(), 1);
    }
    #[test]
    fn late_submission_preserves_newer_draft_and_rejects_mutated_identity() {
        let (_dir, store, key) = fixture();
        let old = draft(&store, &key, "old task");
        let newer = draft(&store, &key, "new draft");
        let request = turn(&old);
        let receipt = store.queue_turn(&request, &old).unwrap();
        assert!(!receipt.cleared);
        assert_eq!(receipt.draft.text, newer.text);
        let mut altered = request.clone();
        altered.provider.model = "another-model".into();
        assert!(store.queue_turn(&altered, &old).is_err());
        let mut same_revision = turn(&old);
        assert_eq!(
            store.queue_turn(&same_revision, &old).unwrap().item.id,
            request.turn_id
        );
        same_revision.prompt = "different".into();
        assert!(store.queue_turn(&same_revision, &old).is_err());
        assert_eq!(store.turn_queue(key.session_id).unwrap().len(), 1);
    }
    #[test]
    fn fifo_caps_ownership_and_removed_tombstones() {
        let (_dir, store, key) = fixture();
        let mut ids = vec![];
        for index in 0..8 {
            let sub = draft(&store, &key, &format!("task {index}"));
            let req = turn(&sub);
            store.queue_turn(&req, &sub).unwrap();
            ids.push((req, sub));
        }
        assert_eq!(
            store
                .turn_queue(key.session_id)
                .unwrap()
                .iter()
                .map(|x| x.id)
                .collect::<Vec<_>>(),
            ids.iter().map(|x| x.0.turn_id).collect::<Vec<_>>()
        );
        let ninth = draft(&store, &key, "ninth");
        assert!(store.queue_turn(&turn(&ninth), &ninth).is_err());
        assert_eq!(store.draft(&key).unwrap().text, "ninth");
        assert!(
            store
                .remove_queued_turn(Uuid::new_v4(), ids[0].0.turn_id)
                .is_err()
        );
        assert!(
            store
                .queued_turn(Uuid::new_v4(), ids[0].0.turn_id)
                .unwrap()
                .is_none()
        );
        store
            .remove_queued_turn(key.session_id, ids[0].0.turn_id)
            .unwrap();
        assert!(
            store
                .queued_turn(key.session_id, ids[0].0.turn_id)
                .unwrap()
                .is_none()
        );
        assert!(store.queue_turn(&ids[0].0, &ids[0].1).is_err());
        assert_eq!(store.turn_queue(key.session_id).unwrap().len(), 7);
        for session_index in 0..7 {
            let mut other = key.clone();
            other.session_id = Uuid::new_v4();
            for index in 0..8 {
                let sub = draft(&store, &other, &format!("{session_index}/{index}"));
                store.queue_turn(&turn(&sub), &sub).unwrap();
            }
        }
        let sub = draft(&store, &key, "64th");
        store.queue_turn(&turn(&sub), &sub).unwrap();
        let mut other = key.clone();
        other.session_id = Uuid::new_v4();
        let sub = draft(&store, &other, "65th");
        assert!(store.queue_turn(&turn(&sub), &sub).is_err());
        assert_eq!(store.draft(&other).unwrap().text, "65th");
    }
    #[test]
    fn restart_pauses_snapshots_and_resume_is_explicit() {
        let (dir, store, key) = fixture();
        let sub = draft(&store, &key, "next");
        let req = turn(&sub);
        store.queue_turn(&req, &sub).unwrap();
        drop(store);
        let store = Store::open(dir.path().join("queue.db")).unwrap();
        assert_eq!(store.recover_turn_queue().unwrap(), 1);
        let item = store
            .queued_turn(key.session_id, req.turn_id)
            .unwrap()
            .unwrap();
        assert!(item.paused);
        assert!(item.reason.unwrap().contains("restart"));
        store
            .set_queued_turn_paused(key.session_id, req.turn_id, false, None)
            .unwrap();
        assert!(!store.turn_queue(key.session_id).unwrap()[0].paused);
        store
            .pause_turn_queue(key.session_id, "Previous task failed")
            .unwrap();
        assert_eq!(
            store.turn_queue(key.session_id).unwrap()[0]
                .reason
                .as_deref(),
            Some("Previous task failed")
        );
        assert!(
            store
                .set_queued_turn_paused(Uuid::new_v4(), req.turn_id, false, None)
                .is_err()
        );
    }
    #[test]
    fn newer_empty_draft_is_not_claimed_as_cleared_and_started_items_cannot_be_removed() {
        let (_dir, store, key) = fixture();
        let sub = draft(&store, &key, "old");
        draft(&store, &key, "");
        let req = turn(&sub);
        let receipt = store.queue_turn(&req, &sub).unwrap();
        assert!(
            !receipt.cleared,
            "a newer empty draft belongs to its own mutation"
        );
        assert!(
            store
                .complete_queued_turn(key.session_id, req.turn_id)
                .is_err()
        );
        store.register_user_turn(&req).unwrap();
        store
            .complete_queued_turn(key.session_id, req.turn_id)
            .unwrap();
        store
            .complete_queued_turn(key.session_id, req.turn_id)
            .unwrap();
        assert!(store.turn_queue(key.session_id).unwrap().is_empty());
        assert!(
            store
                .remove_queued_turn(key.session_id, req.turn_id)
                .unwrap_err()
                .to_string()
                .contains("already started")
        );
    }
    #[test]
    fn admitted_ids_cannot_be_queued_and_conflicting_admission_cannot_consume_queue() {
        let (_dir, store, key) = fixture();
        let sub = draft(&store, &key, "task");
        let req = turn(&sub);
        store.register_user_turn(&req).unwrap();
        assert!(store.queue_turn(&req, &sub).is_err());
        assert_eq!(store.draft(&key).unwrap().text, "task");
        let (_dir, store, key) = fixture();
        let sub = draft(&store, &key, "task");
        let req = turn(&sub);
        store.queue_turn(&req, &sub).unwrap();
        let mut conflicting = req.clone();
        conflicting.prompt = "different".into();
        store.register_user_turn(&conflicting).unwrap();
        assert!(
            store
                .complete_queued_turn(key.session_id, req.turn_id)
                .is_err()
        );
        assert_eq!(store.turn_queue(key.session_id).unwrap().len(), 1);
    }
    #[test]
    fn queued_admission_commits_message_and_started_receipt_together() {
        let (_dir, store, key) = fixture();
        let sub = draft(&store, &key, "next");
        let req = turn(&sub);
        store.queue_turn(&req, &sub).unwrap();
        let mut escalated = req.clone();
        escalated.permissions = PermissionMode::Auto;
        assert!(
            store
                .admit_queued_turn(key.session_id, req.turn_id, &escalated)
                .is_err()
        );
        assert!(store.conversations(10).unwrap().is_empty());
        let item = store
            .admit_queued_turn(key.session_id, req.turn_id, &req)
            .unwrap();
        assert_eq!(item.id, req.turn_id);
        assert!(store.turn_queue(key.session_id).unwrap().is_empty());
        assert_eq!(store.conversations(10).unwrap().len(), 1);
        assert!(
            store
                .remove_queued_turn(key.session_id, req.turn_id)
                .is_err()
        );
        assert!(
            store
                .admit_queued_turn(key.session_id, req.turn_id, &req)
                .is_err()
        );
    }
    #[test]
    fn queued_admission_freezes_provider_identity_and_only_reduces_permissions() {
        let (_dir, store, key) = fixture();
        let sub = draft(&store, &key, "next");
        let mut req = turn(&sub);
        req.permissions = PermissionMode::Auto;
        req.provider.managed_local = true;
        store.queue_turn(&req, &sub).unwrap();
        let mut changed = req.clone();
        changed.provider.model = "different".into();
        assert!(
            store
                .admit_queued_turn(key.session_id, req.turn_id, &changed)
                .is_err()
        );
        changed = req.clone();
        changed.provider.credential_ref = Some("another-account".into());
        assert!(
            store
                .admit_queued_turn(key.session_id, req.turn_id, &changed)
                .is_err()
        );
        changed = req.clone();
        changed.provider.endpoint = "https://remote.example/v1".into();
        assert!(
            store
                .admit_queued_turn(key.session_id, req.turn_id, &changed)
                .is_err()
        );
        changed = req.clone();
        changed.permissions = PermissionMode::ReadOnly;
        changed.provider.endpoint = "http://127.0.0.1:9099/v1".into();
        store
            .admit_queued_turn(key.session_id, req.turn_id, &changed)
            .unwrap();
        assert!(store.turn_queue(key.session_id).unwrap().is_empty());
    }
    #[test]
    fn queued_admission_db_failure_keeps_snapshot_and_no_user_message() {
        let (_dir, store, key) = fixture();
        let sub = draft(&store, &key, "next");
        let req = turn(&sub);
        store.queue_turn(&req, &sub).unwrap();
        store.call(|conn| {conn.execute_batch("CREATE TRIGGER reject_admitted_message BEFORE INSERT ON messages BEGIN SELECT RAISE(ABORT,'injected admission failure'); END;")?;Ok(())}).unwrap();
        assert!(
            store
                .admit_queued_turn(key.session_id, req.turn_id, &req)
                .is_err()
        );
        assert_eq!(
            store.turn_queue(key.session_id).unwrap()[0].turn.prompt,
            "next"
        );
        assert!(store.conversations(10).unwrap().is_empty());
    }
    #[test]
    fn clear_failure_rolls_back_enqueued_row_and_auth_secrets_are_rejected() {
        let (_dir, store, key) = fixture();
        let sub = draft(&store, &key, "keep");
        let mut req = turn(&sub);
        req.provider
            .headers
            .insert("Authorization".into(), "Bearer fixture-secret".into());
        assert!(store.queue_turn(&req, &sub).is_err());
        assert_eq!(store.draft(&key).unwrap().text, "keep");
        req.provider.headers.clear();
        store.turn_queue(key.session_id).unwrap();
        store.call(|conn| {conn.execute_batch("CREATE TRIGGER reject_draft_clear BEFORE UPDATE ON composer_drafts WHEN new.text='' BEGIN SELECT RAISE(ABORT,'injected clear failure'); END;")?;Ok(())}).unwrap();
        assert!(store.queue_turn(&req, &sub).is_err());
        assert_eq!(store.draft(&key).unwrap().text, "keep");
        assert!(store.turn_queue(key.session_id).unwrap().is_empty());
    }
    #[test]
    fn attachment_snapshot_and_failed_commit_keep_user_input() {
        let (_dir, store, key) = fixture();
        let mut sub = draft(&store, &key, "");
        let image = xylo_core::ImageAttachment {
            id: Uuid::new_v4(),
            session_id: key.session_id,
            name: "same.png".into(),
            media_type: "image/png".into(),
            sha256: "a".repeat(64),
            bytes: 42,
            width: 8,
            height: 8,
            source: xylo_core::AttachmentSource::Picker,
            imported_at: 7,
        };
        let DraftSave::Saved(saved) = store
            .save_draft(&DraftWrite {
                key: key.clone(),
                expected_revision: sub.expected_revision,
                mutation_id: Uuid::new_v4(),
                text: String::new(),
                attachments: vec![image.clone()],
            })
            .unwrap()
        else {
            panic!()
        };
        sub.expected_revision = saved.revision;
        sub.attachments = vec![image.clone()];
        let req = turn(&sub);
        let receipt = store.queue_turn(&req, &sub).unwrap();
        assert!(receipt.cleared);
        assert_eq!(receipt.item.turn.attachments, vec![image]);
        let sub = draft(&store, &key, "must survive");
        let req = turn(&sub);
        store.call(|conn|{conn.execute_batch("CREATE TRIGGER reject_queue BEFORE INSERT ON turn_queue BEGIN SELECT RAISE(ABORT,'injected failure'); END;")?;Ok(())}).unwrap();
        assert!(store.queue_turn(&req, &sub).is_err());
        assert_eq!(store.draft(&key).unwrap().text, "must survive");
        assert!(
            store
                .queued_turn(key.session_id, req.turn_id)
                .unwrap()
                .is_none()
        );
    }
}
