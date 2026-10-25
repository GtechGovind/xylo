//! Revisioned composer state, separate from durable user messages/checkpoints.
//! Synchronous Store methods belong on a host blocking worker, never the UI.
use crate::Store;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;
use workbench_core::{ImageAttachment, TurnRequest, now_ms};

/// Composer metadata caps. Reaching a cap returns an error; nonempty drafts are
/// never automatically deleted. Empty revision tombstones fence old saves.
pub const MAX_DRAFT_BYTES: usize = 256 * 1024;
pub const MAX_DRAFT_ATTACHMENTS: usize = 4;
pub const MAX_DRAFT_STORAGE_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_DRAFT_ROWS: usize = 16_384;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DraftKey {
    pub session_id: Uuid,
    /// None represents a composer before a project has been selected.
    pub workspace: Option<PathBuf>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftSnapshot {
    pub key: DraftKey,
    pub text: String,
    #[serde(default)]
    pub attachments: Vec<ImageAttachment>,
    /// Zero represents an absent row. Every committed mutation increments this.
    pub revision: i64,
    pub updated_at: i64,
}
impl DraftSnapshot {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.attachments.is_empty()
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DraftWrite {
    pub key: DraftKey,
    pub expected_revision: i64,
    pub mutation_id: Uuid,
    pub text: String,
    #[serde(default)]
    pub attachments: Vec<ImageAttachment>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", content = "draft", rename_all = "snake_case")]
pub enum DraftSave {
    Saved(DraftSnapshot),
    Conflict(DraftSnapshot),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DraftCursor {
    pub updated_at: i64,
    pub session_id: Uuid,
    pub workspace: Option<PathBuf>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DraftSummary {
    pub key: DraftKey,
    pub revision: i64,
    pub updated_at: i64,
    pub preview: String,
    pub bytes: usize,
    pub attachment_count: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DraftPage {
    /// Exact scope echoed for rejecting late responses after project changes.
    pub workspace: Option<PathBuf>,
    pub drafts: Vec<DraftSummary>,
    pub before: Option<DraftCursor>,
    pub has_more: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DraftSubmission {
    pub key: DraftKey,
    pub expected_revision: i64,
    pub text: String,
    #[serde(default)]
    pub attachments: Vec<ImageAttachment>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DraftAdmission {
    pub turn_id: Uuid,
    pub draft: DraftSnapshot,
    /// False when newer draft edits won the CAS or the turn was already admitted.
    pub cleared: bool,
}

fn scope(workspace: &Option<PathBuf>) -> Result<String> {
    let value = match workspace {
        Some(path) => {
            ensure!(path.is_absolute(), "Choose an absolute project folder");
            path.to_str()
                .context("Project folder cannot be represented as text")?
                .to_owned()
        }
        None => String::new(),
    };
    ensure!(
        value.len() <= 8192 && !value.contains('\0'),
        "Project folder is invalid or too long"
    );
    Ok(value)
}
fn validate(key: &DraftKey, text: &str, attachments: &[ImageAttachment]) -> Result<String> {
    ensure!(
        text.len() <= MAX_DRAFT_BYTES,
        "Draft is too large to save; shorten it or copy it to a file"
    );
    ensure!(
        attachments.len() <= MAX_DRAFT_ATTACHMENTS,
        "Attach at most four images to a draft"
    );
    ensure!(
        attachments
            .iter()
            .all(|image| image.session_id == key.session_id),
        "Draft image belongs to another chat"
    );
    let json = serde_json::to_string(attachments)?;
    ensure!(json.len() <= 8192, "Draft image references are too large");
    Ok(json)
}
pub(crate) fn initialize(conn: &mut Connection) -> Result<()> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='composer_drafts')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS composer_draft_usage(id INTEGER PRIMARY KEY CHECK(id=1),total_bytes INTEGER NOT NULL DEFAULT 0,row_count INTEGER NOT NULL DEFAULT 0);
INSERT OR IGNORE INTO composer_draft_usage(id) VALUES(1);
CREATE TABLE IF NOT EXISTS composer_drafts(session_id TEXT NOT NULL,workspace TEXT NOT NULL,text TEXT NOT NULL CHECK(length(CAST(text AS BLOB))<=262144),attachments TEXT NOT NULL,revision INTEGER NOT NULL CHECK(revision>0),updated_at INTEGER NOT NULL,last_mutation TEXT NOT NULL,last_expected INTEGER NOT NULL,PRIMARY KEY(session_id,workspace));
CREATE INDEX IF NOT EXISTS composer_drafts_recent ON composer_drafts(workspace,updated_at DESC,session_id DESC);
CREATE TRIGGER IF NOT EXISTS composer_drafts_insert AFTER INSERT ON composer_drafts BEGIN UPDATE composer_draft_usage SET total_bytes=total_bytes+length(CAST(new.text AS BLOB))+length(CAST(new.attachments AS BLOB)),row_count=row_count+1 WHERE id=1; END;
CREATE TRIGGER IF NOT EXISTS composer_drafts_update AFTER UPDATE ON composer_drafts BEGIN UPDATE composer_draft_usage SET total_bytes=total_bytes-length(CAST(old.text AS BLOB))-length(CAST(old.attachments AS BLOB))+length(CAST(new.text AS BLOB))+length(CAST(new.attachments AS BLOB)) WHERE id=1; END;
CREATE TRIGGER IF NOT EXISTS composer_drafts_delete AFTER DELETE ON composer_drafts BEGIN UPDATE composer_draft_usage SET total_bytes=total_bytes-length(CAST(old.text AS BLOB))-length(CAST(old.attachments AS BLOB)),row_count=row_count-1 WHERE id=1; END;")?;
        tx.commit()?;
    }
    Ok(())
}
type Stored = (DraftSnapshot, String, i64);
fn read(conn: &Connection, key: &DraftKey, workspace: &str) -> Result<Option<Stored>> {
    let row: Option<(String,String,i64,i64,String,i64)> = conn.query_row("SELECT text,attachments,revision,updated_at,last_mutation,last_expected FROM composer_drafts WHERE session_id=?1 AND workspace=?2", params![key.session_id.to_string(),workspace], |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?))).optional()?;
    row.map(
        |(text, attachments, revision, updated_at, mutation, expected)| {
            Ok((
                DraftSnapshot {
                    key: key.clone(),
                    text,
                    attachments: serde_json::from_str(&attachments)?,
                    revision,
                    updated_at,
                },
                mutation,
                expected,
            ))
        },
    )
    .transpose()
}
fn absent(key: DraftKey) -> DraftSnapshot {
    DraftSnapshot {
        key,
        text: String::new(),
        attachments: Vec::new(),
        revision: 0,
        updated_at: 0,
    }
}
fn capacity(conn: &Connection, previous: Option<&DraftSnapshot>, bytes: usize) -> Result<()> {
    let (total, rows): (i64, i64) = conn.query_row(
        "SELECT total_bytes,row_count FROM composer_draft_usage WHERE id=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let prior = previous
        .map(|draft| {
            draft.text.len()
                + serde_json::to_string(&draft.attachments).map_or(0, |json| json.len())
        })
        .unwrap_or(0);
    ensure!(
        previous.is_some() || rows < MAX_DRAFT_ROWS as i64,
        "Saved drafts have reached their storage limit; existing drafts remain saved"
    );
    ensure!(
        (total as usize).saturating_sub(prior).saturating_add(bytes) <= MAX_DRAFT_STORAGE_BYTES,
        "Saved drafts have reached their size limit; existing drafts remain saved"
    );
    Ok(())
}
fn write(
    conn: &Connection,
    request: &DraftWrite,
    workspace: &str,
    attachments: &str,
) -> Result<DraftSave> {
    ensure!(request.expected_revision >= 0, "Invalid draft revision");
    let stored = read(conn, &request.key, workspace)?;
    if let Some((current, mutation, expected)) = &stored
        && *mutation == request.mutation_id.to_string()
    {
        ensure!(
            *expected == request.expected_revision
                && current.text == request.text
                && current.attachments == request.attachments,
            "Draft mutation ID was reused for different content"
        );
        return Ok(DraftSave::Saved(current.clone()));
    }
    let current = stored
        .as_ref()
        .map(|row| row.0.clone())
        .unwrap_or_else(|| absent(request.key.clone()));
    if current.revision != request.expected_revision {
        return Ok(DraftSave::Conflict(current));
    }
    capacity(
        conn,
        stored.as_ref().map(|row| &row.0),
        request.text.len() + attachments.len(),
    )?;
    let revision = current
        .revision
        .checked_add(1)
        .context("Draft revision limit reached")?;
    let updated_at = now_ms();
    conn.execute("INSERT INTO composer_drafts(session_id,workspace,text,attachments,revision,updated_at,last_mutation,last_expected) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(session_id,workspace) DO UPDATE SET text=excluded.text,attachments=excluded.attachments,revision=excluded.revision,updated_at=excluded.updated_at,last_mutation=excluded.last_mutation,last_expected=excluded.last_expected",params![request.key.session_id.to_string(),workspace,request.text,attachments,revision,updated_at,request.mutation_id.to_string(),request.expected_revision])?;
    Ok(DraftSave::Saved(DraftSnapshot {
        key: request.key.clone(),
        text: request.text.clone(),
        attachments: request.attachments.clone(),
        revision,
        updated_at,
    }))
}
impl Store {
    pub fn draft(&self, key: &DraftKey) -> Result<DraftSnapshot> {
        let key = key.clone();
        let workspace = scope(&key.workspace)?;
        self.call(move |conn| {
            initialize(conn)?;
            Ok(read(conn, &key, &workspace)?
                .map(|row| row.0)
                .unwrap_or_else(|| absent(key)))
        })
    }
    pub fn save_draft(&self, request: &DraftWrite) -> Result<DraftSave> {
        let request = request.clone();
        let workspace = scope(&request.key.workspace)?;
        let attachments = validate(&request.key, &request.text, &request.attachments)?;
        self.call(move |conn| {
            initialize(conn)?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let result = write(&tx, &request, &workspace, &attachments)?;
            tx.commit()?;
            Ok(result)
        })
    }
    /// Nonempty drafts in an exact workspace, including unsubmitted chat UUIDs.
    pub fn drafts_page(
        &self,
        workspace: Option<PathBuf>,
        before: Option<DraftCursor>,
        limit: usize,
    ) -> Result<DraftPage> {
        let scope = scope(&workspace)?;
        let limit = limit.clamp(1, 100);
        if let Some(cursor) = &before {
            ensure!(
                cursor.workspace == workspace,
                "Draft cursor belongs to another project"
            );
        }
        self.call(move|conn|{initialize(conn)?;
            let mut statement=conn.prepare("SELECT session_id,revision,updated_at,substr(text,1,120),length(CAST(text AS BLOB)),json_array_length(attachments) FROM composer_drafts WHERE workspace=?1 AND(text!='' OR json_array_length(attachments)>0) AND(?2 IS NULL OR updated_at<?2 OR(updated_at=?2 AND session_id<?3)) ORDER BY updated_at DESC,session_id DESC LIMIT ?4")?;
            let rows=statement.query_map(params![scope,before.as_ref().map(|cursor|cursor.updated_at),before.as_ref().map(|cursor|cursor.session_id.to_string()),(limit+1)as i64],|row|Ok((row.get::<_,String>(0)?,row.get::<_,i64>(1)?,row.get::<_,i64>(2)?,row.get::<_,String>(3)?,row.get::<_,usize>(4)?,row.get::<_,usize>(5)?)))?;
            let mut drafts=rows.map(|row|{let(id,revision,updated_at,preview,bytes,attachment_count)=row?;Ok(DraftSummary{key:DraftKey{session_id:Uuid::parse_str(&id)?,workspace:workspace.clone()},revision,updated_at,preview,bytes,attachment_count})}).collect::<Result<Vec<_>>>()?;
            let has_more=drafts.len()>limit;drafts.truncate(limit);let before=if has_more{drafts.last().map(|draft|DraftCursor{updated_at:draft.updated_at,session_id:draft.key.session_id,workspace:workspace.clone()})}else{None};Ok(DraftPage{workspace,drafts,before,has_more})
        })
    }
    /// Call only after validating the run, starting its worker and reserving its
    /// dispatch capacity. Registration and draft clear commit together. A failed
    /// DB commit leaves the draft intact. Newer edits win the clear CAS.
    pub fn admit_draft_turn(
        &self,
        turn: &TurnRequest,
        submission: &DraftSubmission,
    ) -> Result<DraftAdmission> {
        ensure!(
            turn.session_id == submission.key.session_id
                && Some(&turn.workspace) == submission.key.workspace.as_ref(),
            "Draft belongs to another chat or project"
        );
        ensure!(
            turn.prompt == submission.text.trim() && turn.attachments == submission.attachments,
            "Submitted draft differs from the user turn"
        );
        ensure!(submission.expected_revision >= 0, "Invalid draft revision");
        validate(&submission.key, &submission.text, &submission.attachments)?;
        let turn = turn.clone();
        let submission = submission.clone();
        let workspace = scope(&submission.key.workspace)?;
        self.call(move |conn| {
            initialize(conn)?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let message_id = Uuid::new_v5(&turn.turn_id, b"user-message");
            let existing: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM messages WHERE id=?1)",
                [message_id.to_string()],
                |row| row.get(0),
            )?;
            let prior_scope: Option<String> = tx
                .query_row(
                    "SELECT json_extract(json,'$.workspace') FROM conversations WHERE id=?1",
                    [turn.session_id.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            ensure!(
                prior_scope
                    .as_deref()
                    .is_none_or(|scope| Some(scope) == turn.workspace.to_str()),
                "This chat belongs to another project"
            );
            crate::store::register_user_turn_in_transaction(&tx, &turn)?;
            let current = read(&tx, &submission.key, &workspace)?
                .map(|row| row.0)
                .unwrap_or_else(|| absent(submission.key.clone()));
            let mut cleared = false;
            let mut draft = current.clone();
            if !existing
                && current.revision == submission.expected_revision
                && (current.revision == 0
                    || (current.text == submission.text
                        && current.attachments == submission.attachments))
            {
                let request = DraftWrite {
                    key: submission.key,
                    expected_revision: current.revision,
                    mutation_id: turn.turn_id,
                    text: String::new(),
                    attachments: Vec::new(),
                };
                if let DraftSave::Saved(saved) = write(&tx, &request, &workspace, "[]")? {
                    draft = saved;
                    cleared = true;
                }
            }
            tx.commit()?;
            Ok(DraftAdmission {
                turn_id: turn.turn_id,
                draft,
                cleared,
            })
        })
    }
}
#[cfg(test)]
#[path = "drafts/tests.rs"]
mod tests;

/// Shared only by application-owned atomic admission services.
pub(crate) fn snapshot_in_transaction(conn: &Connection, key: &DraftKey) -> Result<DraftSnapshot> {
    let workspace = scope(&key.workspace)?;
    Ok(read(conn, key, &workspace)?
        .map(|row| row.0)
        .unwrap_or_else(|| absent(key.clone())))
}
pub(crate) fn clear_submission_in_transaction(
    conn: &Connection,
    submission: &DraftSubmission,
    mutation_id: Uuid,
) -> Result<DraftSnapshot> {
    let workspace = scope(&submission.key.workspace)?;
    let current = snapshot_in_transaction(conn, &submission.key)?;
    if current.revision == submission.expected_revision
        && current.text == submission.text
        && current.attachments == submission.attachments
    {
        let request = DraftWrite {
            key: submission.key.clone(),
            expected_revision: current.revision,
            mutation_id,
            text: String::new(),
            attachments: Vec::new(),
        };
        if let DraftSave::Saved(snapshot) = write(conn, &request, &workspace, "[]")? {
            return Ok(snapshot);
        }
    }
    Ok(current)
}
