//! Explicit fact-version inspection/deletion. This module never mutates chat,
//! checkpoints, learned procedures, composer drafts, or unsaved Memory forms.
use crate::{SqliteMemory, Store};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use workbench_core::{MemoryRecord, now_ms};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FactVersion {
    pub record: MemoryRecord,
    pub active: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FactCursor {
    pub created_at: i64,
    pub id: Uuid,
    pub revision: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FactPage {
    pub scope: String,
    pub anchor: Option<Uuid>,
    pub anchor_version: Option<FactVersion>,
    pub versions: Vec<FactVersion>,
    pub before: Option<FactCursor>,
    pub has_more: bool,
    pub revision: i64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FactDeleteKind {
    Version { id: Uuid },
    Chain { id: Uuid },
    Project,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FactDeletePreview {
    pub scope: String,
    pub kind: FactDeleteKind,
    pub revision: i64,
    pub versions: usize,
    pub active: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FactDeleteRequest {
    pub operation_id: Uuid,
    pub preview: FactDeletePreview,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FactDeletion {
    pub operation_id: Uuid,
    pub scope: String,
    pub kind: FactDeleteKind,
    pub removed: usize,
    pub revision: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExportSelection {
    Project,
    Chain { id: Uuid },
}

pub(crate) fn cancellable<T>(
    conn: &mut Connection,
    cancel: &CancellationToken,
    run: impl FnOnce(&mut Connection) -> Result<T>,
) -> Result<T> {
    ensure!(!cancel.is_cancelled(), "Memory operation cancelled");
    let stopped = cancel.clone();
    conn.progress_handler(1000, Some(move || stopped.is_cancelled()));
    let result = run(conn);
    conn.progress_handler(0, None::<fn() -> bool>);
    if result.is_err() && cancel.is_cancelled() {
        anyhow::bail!("Memory operation cancelled");
    }
    result
}
pub(crate) fn validate_scope(scope: &str) -> Result<()> {
    ensure!(
        !scope.trim().is_empty() && scope.len() <= 8192 && !scope.contains('\0'),
        "Choose a valid project for memory"
    );
    Ok(())
}
pub(crate) fn initialize(conn: &Connection) -> Result<()> {
    let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='memory_fact_links')",[],|row|row.get(0))?;
    if exists {
        return Ok(());
    }
    conn.execute_batch("SAVEPOINT fact_metadata_setup")?;
    let setup=conn.execute_batch("CREATE TABLE IF NOT EXISTS memory_fact_links(id TEXT PRIMARY KEY,scope TEXT NOT NULL,parent TEXT);
CREATE INDEX IF NOT EXISTS memory_fact_parents ON memory_fact_links(scope,parent,id);
CREATE INDEX IF NOT EXISTS memory_fact_versions ON memories(scope,json_extract(json,'$.created_at') DESC,id DESC);
CREATE TABLE IF NOT EXISTS memory_fact_scopes(scope TEXT PRIMARY KEY,revision INTEGER NOT NULL CHECK(revision>=0),generation INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS memory_fact_deleted(id TEXT PRIMARY KEY,scope TEXT NOT NULL,deleted_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS memory_fact_receipts(operation_id TEXT PRIMARY KEY,request TEXT NOT NULL,receipt TEXT NOT NULL,created_at INTEGER NOT NULL);
INSERT OR IGNORE INTO memory_fact_links(id,scope,parent) SELECT id,scope,json_extract(json,'$.supersedes') FROM memories;
INSERT OR IGNORE INTO memory_fact_scopes(scope,revision) SELECT DISTINCT scope,1 FROM memories;");
    if let Err(error) = setup {
        let _ = conn.execute_batch("ROLLBACK TO fact_metadata_setup; RELEASE fact_metadata_setup;");
        return Err(error.into());
    }
    conn.execute_batch("RELEASE fact_metadata_setup")?;
    Ok(())
}
pub(crate) fn revision(conn: &Connection, scope: &str) -> Result<i64> {
    Ok(conn
        .query_row(
            "SELECT revision FROM memory_fact_scopes WHERE scope=?1",
            [scope],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0))
}
pub(crate) fn advance(conn: &Connection, scope: &str) -> Result<i64> {
    ensure!(
        revision(conn, scope)? < i64::MAX,
        "Memory revision limit reached"
    );
    conn.execute("INSERT INTO memory_fact_scopes(scope,revision) VALUES(?1,1) ON CONFLICT(scope) DO UPDATE SET revision=revision+1",[scope])?;
    revision(conn, scope)
}
pub(crate) fn generation(conn: &Connection, scope: &str) -> Result<i64> {
    Ok(conn
        .query_row(
            "SELECT generation FROM memory_fact_scopes WHERE scope=?1",
            [scope],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0))
}
fn advance_deletion(conn: &Connection, scope: &str) -> Result<i64> {
    let revision = advance(conn, scope)?;
    ensure!(
        generation(conn, scope)? < i64::MAX,
        "Memory generation limit reached"
    );
    conn.execute(
        "UPDATE memory_fact_scopes SET generation=generation+1 WHERE scope=?1",
        [scope],
    )?;
    Ok(revision)
}
pub(crate) fn delete_selected(conn: &Connection, scope: &str, id: Uuid) -> Result<()> {
    let actual: Option<String> = conn
        .query_row(
            "SELECT scope FROM memory_fact_links WHERE id=?1",
            [id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(actual) = actual {
        ensure!(actual == scope, "Memory belongs to another project");
        crate::memory::delete_vectors(conn, &id.to_string())?;
        conn.execute("DELETE FROM memories_fts WHERE id=?1", [id.to_string()])?;
        let removed = conn.execute("DELETE FROM memories WHERE id=?1", [id.to_string()])?;
        conn.execute(
            "INSERT OR IGNORE INTO memory_fact_deleted(id,scope,deleted_at) VALUES(?1,?2,?3)",
            params![id.to_string(), scope, now_ms()],
        )?;
        if removed > 0 {
            advance_deletion(conn, scope)?;
        }
    }
    Ok(())
}
pub(crate) fn reject_deleted(conn: &Connection, id: Uuid) -> Result<()> {
    let deleted: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM memory_fact_deleted WHERE id=?1)",
        [id.to_string()],
        |row| row.get(0),
    )?;
    ensure!(
        !deleted,
        "This memory version was deleted; refresh before saving a new entry"
    );
    Ok(())
}
pub(crate) fn inserted(conn: &Connection, record: &MemoryRecord) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO memory_fact_links(id,scope,parent) VALUES(?1,?2,?3)",
        params![
            record.id.to_string(),
            record.scope,
            record.supersedes.map(|id| id.to_string())
        ],
    )?;
    advance(conn, &record.scope)?;
    Ok(())
}
pub(crate) fn scoped_anchor(conn: &Connection, scope: &str, id: Uuid) -> Result<()> {
    let actual: Option<String> = conn
        .query_row(
            "SELECT scope FROM memory_fact_links WHERE id=?1",
            [id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    ensure!(
        actual.as_deref() == Some(scope),
        "Choose a saved fact from this project"
    );
    Ok(())
}
/// UNION (rather than UNION ALL) terminates even if legacy metadata is cyclic.
pub(crate) const CONNECTED:&str="WITH RECURSIVE connected(id) AS (
SELECT id FROM memory_fact_links WHERE scope=?1 AND id=?2
UNION SELECT l.parent FROM memory_fact_links l JOIN connected c ON l.id=c.id WHERE l.scope=?1 AND l.parent IS NOT NULL
UNION SELECT l.id FROM memory_fact_links l JOIN connected c ON l.parent=c.id WHERE l.scope=?1
)";
impl Store {
    pub fn fact_versions(
        &self,
        scope: &str,
        anchor: Option<Uuid>,
        before: Option<FactCursor>,
        limit: usize,
        cancel: &CancellationToken,
    ) -> Result<FactPage> {
        validate_scope(scope)?;
        let scope = scope.to_owned();
        let cancel = cancel.clone();
        let limit = limit.clamp(1, 100);
        self.call(move|conn| cancellable(conn,&cancel,|conn|{
            ensure!(!cancel.is_cancelled(),"Memory inspection cancelled");initialize(conn)?;
            let tx=conn.transaction()?;let revision=revision(&tx,&scope)?;
            ensure!(before.as_ref().is_none_or(|cursor|cursor.revision==revision),"Memory changed while browsing; refresh version history");
            if let Some(id)=anchor {scoped_anchor(&tx,&scope,id)?;}
            let sql=format!("{CONNECTED} SELECT m.active,m.json FROM memories m WHERE m.scope=?1 AND (?2 IS NULL OR m.id IN(SELECT id FROM connected)) AND (?3 IS NULL OR json_extract(m.json,'$.created_at')<?3 OR(json_extract(m.json,'$.created_at')=?3 AND m.id<?4)) ORDER BY json_extract(m.json,'$.created_at') DESC,m.id DESC LIMIT ?5");
            let mut versions={let mut statement=tx.prepare(&sql)?;let rows=statement.query_map(params![scope,anchor.map(|id|id.to_string()),before.as_ref().map(|cursor|cursor.created_at),before.as_ref().map(|cursor|cursor.id.to_string()),(limit+1)as i64],|row|Ok((row.get::<_,bool>(0)?,row.get::<_,String>(1)?)))?;
                let mut versions=Vec::with_capacity(limit+1);for row in rows {ensure!(!cancel.is_cancelled(),"Memory inspection cancelled");let(active,json)=row?;let record:MemoryRecord=serde_json::from_str(&json)?;ensure!(record.scope==scope,"Memory provenance is inconsistent");versions.push(FactVersion{record,active});}versions};
            let anchor_version=if let Some(id)=anchor {
                let row:Option<(bool,String)>=tx.query_row("SELECT active,json FROM memories WHERE scope=?1 AND id=?2",params![scope,id.to_string()],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
                row.map(|(active,json)|Ok::<_,anyhow::Error>(FactVersion{active,record:serde_json::from_str(&json)?})).transpose()?
            }else{None};
            let has_more=versions.len()>limit;versions.truncate(limit);
            let before=if has_more {versions.last().map(|version|FactCursor{created_at:version.record.created_at,id:version.record.id,revision})}else{None};
            tx.commit()?;Ok(FactPage{scope,anchor,anchor_version,versions,before,has_more,revision})
        }))
    }
    pub fn fact_delete_preview(
        &self,
        scope: &str,
        kind: FactDeleteKind,
        cancel: &CancellationToken,
    ) -> Result<FactDeletePreview> {
        validate_scope(scope)?;
        let scope = scope.to_owned();
        let cancel = cancel.clone();
        self.call(move |conn| {
            cancellable(conn, &cancel, |conn| {
                ensure!(!cancel.is_cancelled(), "Memory deletion preview cancelled");
                initialize(conn)?;
                let tx = conn.transaction()?;
                targets(&tx, &scope, &kind)?;
                let (versions, active) = counts(&tx)?;
                let revision = revision(&tx, &scope)?;
                ensure!(!cancel.is_cancelled(), "Memory deletion preview cancelled");
                tx.commit()?;
                Ok(FactDeletePreview {
                    scope,
                    kind,
                    revision,
                    versions,
                    active,
                })
            })
        })
    }
}
fn targets(conn: &Connection, scope: &str, kind: &FactDeleteKind) -> Result<()> {
    conn.execute_batch("CREATE TEMP TABLE IF NOT EXISTS memory_delete_targets(id TEXT PRIMARY KEY);DELETE FROM memory_delete_targets;")?;
    match kind {
        FactDeleteKind::Version { id } => {
            scoped_anchor(conn, scope, *id)?;
            conn.execute(
                "INSERT INTO memory_delete_targets(id) VALUES(?1)",
                [id.to_string()],
            )?;
        }
        FactDeleteKind::Chain { id } => {
            scoped_anchor(conn, scope, *id)?;
            conn.execute(
                &format!(
                    "{CONNECTED} INSERT INTO memory_delete_targets(id) SELECT id FROM connected"
                ),
                params![scope, id.to_string()],
            )?;
        }
        FactDeleteKind::Project => {
            conn.execute("INSERT INTO memory_delete_targets(id) SELECT id FROM memory_fact_links WHERE scope=?1",[scope])?;
        }
    }
    Ok(())
}
fn counts(conn: &Connection) -> Result<(usize, usize)> {
    Ok(conn.query_row("SELECT count(*),coalesce(sum(active),0) FROM memories WHERE id IN(SELECT id FROM memory_delete_targets)",[],|row|Ok((row.get(0)?,row.get(1)?)))?)
}
impl SqliteMemory {
    pub fn delete_facts(
        &self,
        request: FactDeleteRequest,
        cancel: &CancellationToken,
    ) -> Result<FactDeletion> {
        validate_scope(&request.preview.scope)?;
        let cancel = cancel.clone();
        self.store().call(move|conn| cancellable(conn,&cancel,|conn|{
            initialize(conn)?;let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let encoded=serde_json::to_string(&request)?;
            if let Some((original,receipt))=tx.query_row("SELECT request,receipt FROM memory_fact_receipts WHERE operation_id=?1",[request.operation_id.to_string()],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?))).optional()? {
                ensure!(original==encoded,"Deletion identity was reused for a different request");return serde_json::from_str(&receipt).context("Read memory deletion receipt");
            }
            ensure!(!cancel.is_cancelled(),"Memory deletion cancelled");
            let preview=&request.preview;
            ensure!(revision(&tx,&preview.scope)?==preview.revision,"Project facts changed; review the deletion again");
            targets(&tx,&preview.scope,&preview.kind)?;let(removed,active)=counts(&tx)?;
            ensure!(removed==preview.versions && active==preview.active,"Project facts changed; review the deletion again");
            crate::memory::delete_vector_targets(&tx)?;
            tx.execute("DELETE FROM memories_fts WHERE id IN(SELECT id FROM memory_delete_targets)",[])?;
            tx.execute("DELETE FROM memories WHERE scope=?1 AND id IN(SELECT id FROM memory_delete_targets)",[&preview.scope])?;
            tx.execute("INSERT OR IGNORE INTO memory_fact_deleted(id,scope,deleted_at) SELECT id,?1,?2 FROM memory_delete_targets",params![preview.scope,now_ms()])?;
            let revision=if removed>0 {advance_deletion(&tx,&preview.scope)?}else{preview.revision};
            let receipt=FactDeletion{operation_id:request.operation_id,scope:preview.scope.clone(),kind:preview.kind.clone(),removed,revision};
            tx.execute("INSERT INTO memory_fact_receipts(operation_id,request,receipt,created_at) VALUES(?1,?2,?3,?4)",params![request.operation_id.to_string(),encoded,serde_json::to_string(&receipt)?,now_ms()])?;
            tx.execute("DELETE FROM memory_fact_receipts WHERE operation_id IN(SELECT operation_id FROM memory_fact_receipts ORDER BY created_at DESC,operation_id LIMIT -1 OFFSET 4096)",[])?;
            ensure!(!cancel.is_cancelled(),"Memory deletion cancelled");tx.commit()?;Ok(receipt)
        }))
    }
}

#[cfg(test)]
#[path = "memory_facts_tests.rs"]
mod tests;
