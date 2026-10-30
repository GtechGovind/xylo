//! Durable, revisioned unsaved Memory forms. No form is a fact until the user
//! saves it. Composer drafts use a different table/key/API.
use crate::Store;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use xylo_core::{MemoryRecord, now_ms};

const MAX_FORM_BYTES: usize = 6 * 1024 * 1024;
const MAX_TOTAL_BYTES: i64 = 64 * 1024 * 1024;
const MAX_FORM_ROWS: i64 = 4096;
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryFormState {
    pub text: String,
    pub editing: Option<Uuid>,
    pub retry: Option<MemoryRecord>,
    pub retry_input: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoryFormSnapshot {
    pub scope: String,
    pub form: MemoryFormState,
    pub revision: i64,
    pub updated_at: i64,
    /// A committed fact was reconciled before restoring this form. Its newer
    /// text is preserved; an identical submitted input is cleared once.
    pub recovered_commit: Option<Uuid>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoryFormWrite {
    pub scope: String,
    pub expected_revision: i64,
    pub mutation_id: Uuid,
    pub form: MemoryFormState,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", content = "form", rename_all = "snake_case")]
pub enum MemoryFormSave {
    Saved(MemoryFormSnapshot),
    Conflict(MemoryFormSnapshot),
}

fn initialize(conn: &Connection) -> Result<()> {
    let ready:bool=conn.query_row("SELECT count(*)=5 FROM sqlite_schema WHERE name IN('memory_forms','memory_form_usage','memory_form_insert','memory_form_update','memory_form_delete')",[],|row|row.get(0))?;
    let schema: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type='table' AND name='memory_forms'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let latest = schema.as_ref().is_some_and(|sql| sql.contains("6291456"));
    if ready && latest {
        return Ok(());
    }
    conn.execute_batch("SAVEPOINT memory_form_setup")?;
    if schema.is_some() && !latest {
        let migration=conn.execute_batch("DROP TRIGGER IF EXISTS memory_form_insert;DROP TRIGGER IF EXISTS memory_form_update;DROP TRIGGER IF EXISTS memory_form_delete;
ALTER TABLE memory_forms RENAME TO memory_forms_previous_bound;
CREATE TABLE memory_forms(scope TEXT PRIMARY KEY,json TEXT NOT NULL CHECK(length(CAST(json AS BLOB))<=6291456),revision INTEGER NOT NULL CHECK(revision>0),updated_at INTEGER NOT NULL,mutation_id TEXT NOT NULL,expected INTEGER NOT NULL);
INSERT INTO memory_forms SELECT scope,json,revision,updated_at,mutation_id,expected FROM memory_forms_previous_bound;
DROP TABLE memory_forms_previous_bound;");
        if let Err(error) = migration {
            let _ = conn.execute_batch("ROLLBACK TO memory_form_setup;RELEASE memory_form_setup");
            return Err(error.into());
        }
    }
    let setup=conn.execute_batch("CREATE TABLE IF NOT EXISTS memory_forms(scope TEXT PRIMARY KEY,json TEXT NOT NULL CHECK(length(CAST(json AS BLOB))<=6291456),revision INTEGER NOT NULL CHECK(revision>0),updated_at INTEGER NOT NULL,mutation_id TEXT NOT NULL,expected INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS memory_form_usage(singleton INTEGER PRIMARY KEY CHECK(singleton=1),bytes INTEGER NOT NULL DEFAULT 0,rows INTEGER NOT NULL DEFAULT 0);
INSERT OR IGNORE INTO memory_form_usage(singleton) VALUES(1);
CREATE TRIGGER IF NOT EXISTS memory_form_insert AFTER INSERT ON memory_forms BEGIN UPDATE memory_form_usage SET bytes=bytes+length(CAST(new.json AS BLOB)),rows=rows+1 WHERE singleton=1; END;
CREATE TRIGGER IF NOT EXISTS memory_form_update AFTER UPDATE ON memory_forms BEGIN UPDATE memory_form_usage SET bytes=bytes-length(CAST(old.json AS BLOB))+length(CAST(new.json AS BLOB)) WHERE singleton=1; END;
CREATE TRIGGER IF NOT EXISTS memory_form_delete AFTER DELETE ON memory_forms BEGIN UPDATE memory_form_usage SET bytes=bytes-length(CAST(old.json AS BLOB)),rows=rows-1 WHERE singleton=1; END;
UPDATE memory_form_usage SET bytes=(SELECT coalesce(sum(length(CAST(json AS BLOB))),0) FROM memory_forms),rows=(SELECT count(*) FROM memory_forms) WHERE singleton=1;");
    if let Err(error) = setup {
        let _ = conn.execute_batch("ROLLBACK TO memory_form_setup; RELEASE memory_form_setup");
        return Err(error.into());
    }
    conn.execute_batch("RELEASE memory_form_setup")?;
    Ok(())
}
fn validate(scope: &str, form: &MemoryFormState) -> Result<String> {
    crate::memory_facts::validate_scope(scope)?;
    ensure!(
        form.text.len() <= 262144,
        "This memory entry is too large; shorten it or copy it to a file"
    );
    if let Some(retry) = &form.retry {
        ensure!(
            retry.scope == scope
                && retry.content.len() <= 262144
                && retry.source.len() <= 8192
                && retry.supersedes != Some(retry.id),
            "Memory save recovery is inconsistent"
        );
    }
    ensure!(
        form.retry_input
            .as_ref()
            .is_none_or(|input| input.len() <= 262144
                && form
                    .retry
                    .as_ref()
                    .is_some_and(|retry| input.trim() == retry.content)),
        "Memory save recovery input is inconsistent"
    );
    let json = serde_json::to_string(form)?;
    ensure!(
        json.len() <= MAX_FORM_BYTES,
        "This memory entry is too large to recover safely"
    );
    Ok(json)
}
fn absent(scope: &str) -> MemoryFormSnapshot {
    MemoryFormSnapshot {
        scope: scope.into(),
        form: MemoryFormState::default(),
        revision: 0,
        updated_at: 0,
        recovered_commit: None,
    }
}
type Stored = (MemoryFormSnapshot, String, i64, usize);
fn read(conn: &Connection, scope: &str) -> Result<Option<Stored>> {
    let row: Option<(String, i64, i64, String, i64)> = conn
        .query_row(
            "SELECT json,revision,updated_at,mutation_id,expected FROM memory_forms WHERE scope=?1",
            [scope],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    row.map(|(json, revision, updated_at, mutation, expected)| {
        ensure!(
            json.len() <= MAX_FORM_BYTES,
            "Saved memory form exceeds its size limit"
        );
        let form: MemoryFormState = serde_json::from_str(&json)?;
        validate(scope, &form)?;
        Ok((
            MemoryFormSnapshot {
                scope: scope.into(),
                form,
                revision,
                updated_at,
                recovered_commit: None,
            },
            mutation,
            expected,
            json.len(),
        ))
    })
    .transpose()
}
fn put(
    conn: &Connection,
    write: &MemoryFormWrite,
    json: &str,
    revision: i64,
) -> Result<MemoryFormSnapshot> {
    let updated_at = now_ms();
    conn.execute("INSERT INTO memory_forms(scope,json,revision,updated_at,mutation_id,expected) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(scope) DO UPDATE SET json=excluded.json,revision=excluded.revision,updated_at=excluded.updated_at,mutation_id=excluded.mutation_id,expected=excluded.expected",params![write.scope,json,revision,updated_at,write.mutation_id.to_string(),write.expected_revision])?;
    Ok(MemoryFormSnapshot {
        scope: write.scope.clone(),
        form: write.form.clone(),
        revision,
        updated_at,
        recovered_commit: None,
    })
}
impl Store {
    pub fn memory_form(&self, scope: &str) -> Result<MemoryFormSnapshot> {
        crate::memory_facts::validate_scope(scope)?;
        let scope = scope.to_owned();
        self.call(move |conn| {
            initialize(conn)?;
            crate::memory_facts::initialize(conn)?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let Some((mut snapshot, _, _, _)) = read(&tx, &scope)? else {
                tx.commit()?;
                return Ok(absent(&scope));
            };
            if let Some(retry) = snapshot.form.retry.clone() {
                let canonical: Option<String> = tx
                    .query_row(
                        "SELECT json FROM memories WHERE scope=?1 AND id=?2",
                        params![scope, retry.id.to_string()],
                        |row| row.get(0),
                    )
                    .optional()?;
                if canonical
                    .as_deref()
                    .map(serde_json::from_str::<serde_json::Value>)
                    .transpose()?
                    == Some(serde_json::to_value(&retry)?)
                {
                    if snapshot
                        .form
                        .retry_input
                        .as_deref()
                        .is_some_and(|input| input == snapshot.form.text)
                        && snapshot.form.editing == retry.supersedes
                    {
                        snapshot.form.text.clear();
                        snapshot.form.editing = None;
                    } else if retry.supersedes.is_some()
                        && snapshot.form.editing == retry.supersedes
                    {
                        snapshot.form.editing = Some(retry.id);
                    }
                    if snapshot.form.retry_input.is_none()
                        && snapshot.form.text.trim() == retry.content
                    {
                        tx.commit()?;
                        return Ok(snapshot);
                    }
                    snapshot.form.retry = None;
                    snapshot.form.retry_input = None;
                    let write = MemoryFormWrite {
                        scope: scope.clone(),
                        expected_revision: snapshot.revision,
                        mutation_id: Uuid::new_v4(),
                        form: snapshot.form,
                    };
                    let json = validate(&scope, &write.form)?;
                    let next = snapshot
                        .revision
                        .checked_add(1)
                        .context("Memory form revision limit reached")?;
                    snapshot = put(&tx, &write, &json, next)?;
                    snapshot.recovered_commit = Some(retry.id);
                }
            }
            tx.commit()?;
            Ok(snapshot)
        })
    }
    pub fn save_memory_form(&self, write: MemoryFormWrite) -> Result<MemoryFormSave> {
        let json = validate(&write.scope, &write.form)?;
        ensure!(write.expected_revision >= 0, "Invalid memory form revision");
        self.call(move |conn| {
            initialize(conn)?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let stored = read(&tx, &write.scope)?;
            let current = stored
                .as_ref()
                .map(|row| row.0.clone())
                .unwrap_or_else(|| absent(&write.scope));
            if let Some((_, mutation, expected, _)) = &stored
                && *mutation == write.mutation_id.to_string()
            {
                ensure!(
                    *expected == write.expected_revision
                        && serde_json::to_string(&current.form)? == json,
                    "Memory form save identity was reused for different content"
                );
                tx.commit()?;
                return Ok(MemoryFormSave::Saved(current));
            }
            if current.revision != write.expected_revision {
                tx.commit()?;
                return Ok(MemoryFormSave::Conflict(current));
            }
            let (bytes, rows): (i64, i64) = tx.query_row(
                "SELECT bytes,rows FROM memory_form_usage WHERE singleton=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            ensure!(
                stored.is_some() || rows < MAX_FORM_ROWS,
                "Saved memory forms have reached their limit; existing entries remain saved"
            );
            ensure!(
                bytes
                    .saturating_sub(stored.as_ref().map_or(0, |row| row.3 as i64))
                    .saturating_add(json.len() as i64)
                    <= MAX_TOTAL_BYTES,
                "Saved memory forms have reached their size limit; existing entries remain saved"
            );
            let revision = current
                .revision
                .checked_add(1)
                .context("Memory form revision limit reached")?;
            let snapshot = put(&tx, &write, &json, revision)?;
            tx.commit()?;
            Ok(MemoryFormSave::Saved(snapshot))
        })
    }
}

#[cfg(test)]
#[path = "memory_forms_tests.rs"]
mod tests;
