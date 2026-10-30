//! Durable native document edits, distinct from files and conversation drafts.
//!
//! A draft is a proposal against an immutable source revision. Saving it never
//! writes the source file or grants permission to execute an agent tool. All
//! methods run on the dedicated database executor, through a blocking host job.
use crate::Store;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;
use xylo_core::{DocumentEdit, DocumentSource, now_ms};

mod relink;
pub use relink::*;

pub const MAX_DOCUMENT_DRAFT_BYTES: usize = 6 * 1024 * 1024;
const MAX_STORAGE_BYTES: i64 = 64 * 1024 * 1024;
const MAX_ROWS: i64 = 4096;
const MAX_EDITS: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentDraftKey {
    pub workspace: PathBuf,
    pub source: PathBuf,
}

/// Persisted before dispatch. Its identity is reused for reconciliation after
/// a lost response; a retry must not invent a new file or request identity.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DocumentCopyIntent {
    pub operation_id: Uuid,
    pub generation: u64,
    pub name: String,
    pub edits: Vec<DocumentEdit>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DocumentDraftState {
    pub source: DocumentSource,
    pub edits: Vec<DocumentEdit>,
    pub export_name: String,
    pub sheet: Option<String>,
    /// A worksheet coordinate such as A1, not a screen or page offset.
    pub selected: Option<String>,
    pub pending_copy: Option<DocumentCopyIntent>,
    #[serde(default)]
    pub pending_relink: Option<Box<DocumentDraftRelinkRequest>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DocumentDraftSnapshot {
    pub key: DocumentDraftKey,
    pub state: Option<DocumentDraftState>,
    /// Zero means no row; cleared drafts retain a revision tombstone.
    pub revision: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DocumentDraftWrite {
    pub key: DocumentDraftKey,
    pub expected_revision: i64,
    pub mutation_id: Uuid,
    pub state: Option<DocumentDraftState>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", content = "draft", rename_all = "snake_case")]
pub enum DocumentDraftSave {
    Saved(DocumentDraftSnapshot),
    Conflict(DocumentDraftSnapshot),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DocumentDraftCursor {
    pub updated_at: i64,
    pub source: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DocumentDraftSummary {
    pub key: DocumentDraftKey,
    pub revision: i64,
    pub updated_at: i64,
    pub export_name: String,
    pub source_revision: String,
    pub edits: usize,
    pub has_pending_copy: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DocumentDraftPage {
    pub workspace: PathBuf,
    pub drafts: Vec<DocumentDraftSummary>,
    pub before: Option<DocumentDraftCursor>,
    pub has_more: bool,
}

fn path_text(path: &Path) -> Result<String> {
    ensure!(
        path.is_absolute()
            && !path
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
            && path.as_os_str() == path.components().collect::<PathBuf>().as_os_str(),
        "Document draft requires an absolute normalized path"
    );
    let text = path
        .to_str()
        .context("Document path cannot be represented as text")?;
    ensure!(
        text.len() <= 8192 && !text.contains('\0'),
        "Document path is invalid or too long"
    );
    Ok(text.to_owned())
}

fn validate(key: &DocumentDraftKey, state: &Option<DocumentDraftState>) -> Result<String> {
    path_text(&key.workspace)?;
    path_text(&key.source)?;
    if let Some(state) = state {
        ensure!(
            state.source.path == key.source,
            "Document draft source does not match its key"
        );
        ensure!(
            state.source.revision.len() == 64
                && state
                    .source
                    .revision
                    .bytes()
                    .all(|value| value.is_ascii_hexdigit()),
            "Document draft has no valid source revision"
        );
        ensure!(
            state.edits.len() <= MAX_EDITS,
            "Too many document edits to recover safely"
        );
        ensure!(state.export_name.len() <= 480, "Document name is too long");
        ensure!(
            state.sheet.as_ref().is_none_or(|value| value.len() <= 1024)
                && state
                    .selected
                    .as_ref()
                    .is_none_or(|value| value.len() <= 32),
            "Document selection is too large"
        );
        if let Some(request) = &state.pending_relink {
            ensure!(
                request.key == *key
                    && request.expected_revision > 0
                    && !request.operation_id.is_nil(),
                "Saved document location request is inconsistent"
            );
            path_text(&request.source.path)?;
            ensure!(
                state.pending_copy.is_none()
                    && request.source.path != state.source.path
                    && request.source.kind == state.source.kind
                    && request.source.revision == state.source.revision
                    && request.source.bytes == state.source.bytes,
                "The selected original differs from the saved version"
            );
        }
        if let Some(intent) = &state.pending_copy {
            ensure!(
                !intent.operation_id.is_nil()
                    && intent.name.len() <= 480
                    && intent.edits.len() <= MAX_EDITS,
                "Document save recovery is inconsistent"
            );
        }
    }
    let json = serde_json::to_string(state)?;
    ensure!(
        json.len() <= MAX_DOCUMENT_DRAFT_BYTES,
        "These document edits are too large to recover safely; save a copy before continuing"
    );
    Ok(json)
}

fn initialize(conn: &mut Connection) -> Result<()> {
    let ready: bool = conn.query_row(
        "SELECT count(*)=6 FROM sqlite_schema WHERE name IN('document_drafts','document_draft_usage','document_draft_recent','document_draft_insert','document_draft_update','document_draft_delete')",
        [], |row| row.get(0),
    )?;
    if ready {
        return Ok(());
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch("CREATE TABLE IF NOT EXISTS document_drafts(workspace TEXT NOT NULL,source TEXT NOT NULL,json TEXT NOT NULL CHECK(json_valid(json) AND length(CAST(json AS BLOB))<=6291456),revision INTEGER NOT NULL CHECK(revision>0),updated_at INTEGER NOT NULL,mutation_id TEXT NOT NULL,expected INTEGER NOT NULL,PRIMARY KEY(workspace,source));
CREATE INDEX IF NOT EXISTS document_draft_recent ON document_drafts(workspace,updated_at DESC,source ASC);
CREATE TABLE IF NOT EXISTS document_draft_usage(singleton INTEGER PRIMARY KEY CHECK(singleton=1),bytes INTEGER NOT NULL DEFAULT 0,rows INTEGER NOT NULL DEFAULT 0);
INSERT OR IGNORE INTO document_draft_usage(singleton) VALUES(1);
CREATE TRIGGER IF NOT EXISTS document_draft_insert AFTER INSERT ON document_drafts BEGIN UPDATE document_draft_usage SET bytes=bytes+length(CAST(new.json AS BLOB)),rows=rows+1 WHERE singleton=1; END;
CREATE TRIGGER IF NOT EXISTS document_draft_update AFTER UPDATE ON document_drafts BEGIN UPDATE document_draft_usage SET bytes=bytes-length(CAST(old.json AS BLOB))+length(CAST(new.json AS BLOB)) WHERE singleton=1; END;
CREATE TRIGGER IF NOT EXISTS document_draft_delete AFTER DELETE ON document_drafts BEGIN UPDATE document_draft_usage SET bytes=bytes-length(CAST(old.json AS BLOB)),rows=rows-1 WHERE singleton=1; END;
UPDATE document_draft_usage SET bytes=(SELECT coalesce(sum(length(CAST(json AS BLOB))),0) FROM document_drafts),rows=(SELECT count(*) FROM document_drafts) WHERE singleton=1;")?;
    tx.commit()?;
    Ok(())
}

fn absent(key: DocumentDraftKey) -> DocumentDraftSnapshot {
    DocumentDraftSnapshot {
        key,
        state: None,
        revision: 0,
        updated_at: 0,
    }
}

type Stored = (DocumentDraftSnapshot, String, i64, usize);
fn read(conn: &Connection, key: &DocumentDraftKey) -> Result<Option<Stored>> {
    let row: Option<(String, i64, i64, String, i64)> = conn.query_row(
        "SELECT json,revision,updated_at,mutation_id,expected FROM document_drafts WHERE workspace=?1 AND source=?2",
        params![path_text(&key.workspace)?, path_text(&key.source)?],
        |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)),
    ).optional()?;
    row.map(|(json, revision, updated_at, mutation, expected)| {
        ensure!(
            json.len() <= MAX_DOCUMENT_DRAFT_BYTES,
            "Saved document edits exceed their recovery limit"
        );
        let state: Option<DocumentDraftState> = serde_json::from_str(&json)?;
        validate(key, &state)?;
        Ok((
            DocumentDraftSnapshot {
                key: key.clone(),
                state,
                revision,
                updated_at,
            },
            mutation,
            expected,
            json.len(),
        ))
    })
    .transpose()
}

impl Store {
    pub fn document_draft(&self, key: DocumentDraftKey) -> Result<DocumentDraftSnapshot> {
        validate(&key, &None)?;
        self.call(move |conn| {
            initialize(conn)?;
            Ok(read(conn, &key)?.map_or_else(|| absent(key), |row| row.0))
        })
    }

    pub fn save_document_draft(&self, write: DocumentDraftWrite) -> Result<DocumentDraftSave> {
        let json = validate(&write.key, &write.state)?;
        ensure!(
            write.expected_revision >= 0 && !write.mutation_id.is_nil(),
            "Invalid document draft revision or save identity"
        );
        self.call(move |conn| {
            initialize(conn)?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let stored = read(&tx,&write.key)?;
            let current = stored.as_ref().map_or_else(|| absent(write.key.clone()),|row| row.0.clone());
            if let Some((_,mutation,expected,_))=&stored
                && *mutation==write.mutation_id.to_string() {
                ensure!(*expected==write.expected_revision && serde_json::to_string(&current.state)?==json,
                    "Document draft save identity was reused for different content");
                tx.commit()?;
                return Ok(DocumentDraftSave::Saved(current));
            }
            if current.revision!=write.expected_revision {
                tx.commit()?;
                return Ok(DocumentDraftSave::Conflict(current));
            }
            let (bytes,rows):(i64,i64)=tx.query_row("SELECT bytes,rows FROM document_draft_usage WHERE singleton=1",[],|row| Ok((row.get(0)?,row.get(1)?)))?;
            ensure!(stored.is_some() || rows<MAX_ROWS,"Saved document drafts have reached their limit; existing edits remain saved");
            ensure!(bytes.saturating_sub(stored.as_ref().map_or(0,|row|row.3 as i64)).saturating_add(json.len() as i64)<=MAX_STORAGE_BYTES,
                "Saved document drafts have reached their size limit; existing edits remain saved");
            let revision=current.revision.checked_add(1).context("Document draft revision limit reached")?;
            let updated_at=now_ms().max(current.updated_at.saturating_add(1));
            tx.execute("INSERT INTO document_drafts(workspace,source,json,revision,updated_at,mutation_id,expected) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(workspace,source) DO UPDATE SET json=excluded.json,revision=excluded.revision,updated_at=excluded.updated_at,mutation_id=excluded.mutation_id,expected=excluded.expected",
                params![path_text(&write.key.workspace)?,path_text(&write.key.source)?,json,revision,updated_at,write.mutation_id.to_string(),write.expected_revision])?;
            tx.commit()?;
            Ok(DocumentDraftSave::Saved(DocumentDraftSnapshot { key:write.key,state:write.state,revision,updated_at }))
        })
    }

    pub fn document_drafts(
        &self,
        workspace: PathBuf,
        before: Option<DocumentDraftCursor>,
        limit: usize,
    ) -> Result<DocumentDraftPage> {
        let scope = path_text(&workspace)?;
        if let Some(cursor) = &before {
            path_text(&cursor.source)?;
        }
        let limit = limit.clamp(1, 100);
        self.call(move |conn| {
            initialize(conn)?;
            let mut query=conn.prepare("SELECT source,revision,updated_at,json_extract(json,'$.export_name'),json_extract(json,'$.source.revision'),json_array_length(json,'$.edits'),json_type(json,'$.pending_copy')!='null' FROM document_drafts WHERE workspace=?1 AND json!='null' AND (?2 IS NULL OR updated_at<?2 OR (updated_at=?2 AND source>?3)) ORDER BY updated_at DESC,source ASC LIMIT ?4")?;
            let rows=query.query_map(params![scope,before.as_ref().map(|cursor|cursor.updated_at),before.as_ref().map(|cursor|path_text(&cursor.source)).transpose()?.unwrap_or_default(),limit+1],|row| {
                Ok(DocumentDraftSummary { key:DocumentDraftKey{workspace:workspace.clone(),source:PathBuf::from(row.get::<_,String>(0)?)},revision:row.get(1)?,updated_at:row.get(2)?,export_name:row.get(3)?,source_revision:row.get(4)?,edits:row.get(5)?,has_pending_copy:row.get(6)? })
            })?;
            let mut drafts=rows.collect::<rusqlite::Result<Vec<_>>>()?;
            let has_more=drafts.len()>limit;
            drafts.truncate(limit);
            let cursor=drafts.last().map(|draft|DocumentDraftCursor{updated_at:draft.updated_at,source:draft.key.source.clone()});
            Ok(DocumentDraftPage { workspace,drafts,before:cursor,has_more })
        })
    }
}

#[cfg(test)]
mod tests;
