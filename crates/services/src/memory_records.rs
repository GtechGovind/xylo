//! Bounded memory browsing and an explicit, streaming user export. Embeddings
//! stay private to the index; exported facts retain provenance and corrections.
use crate::Store;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
#[cfg(any(unix, test))]
use std::fs::File;
use std::{
    io::{BufWriter, Write},
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use xylo_core::{MemoryRecord, now_ms};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoryCursor {
    pub created_at: i64,
    pub id: Uuid,
}
#[derive(Clone, Debug)]
pub struct MemoryPage {
    pub scope: String,
    pub query: String,
    pub records: Vec<MemoryRecord>,
    pub before: Option<MemoryCursor>,
    pub has_more: bool,
}
#[derive(Clone, Debug)]
pub struct MemoryMutation {
    pub scope: String,
    pub id: Uuid,
    pub deleted: bool,
}
/// One journaled action of a conversation, reduced to what decides the
/// origin of a fact written later in that conversation.
#[derive(Clone, Debug)]
pub struct ActionProvenance {
    pub name: String,
    /// The intent's arguments, for the names the caller asked for.
    pub arguments: Option<serde_json::Value>,
    /// The recorded result, for the names the caller asked for.
    pub result: Option<serde_json::Value>,
}
#[derive(Clone, Debug)]
pub struct MemoryExport {
    pub scope: String,
    pub path: PathBuf,
    pub records: usize,
}
impl Store {
    /// Every action of `session` that may have run (pending and denied
    /// actions never did), in the order they were journaled (insertion
    /// order, which a millisecond timestamp cannot break ties in), with arguments only for the names in
    /// `with_arguments` and results only for those in `with_results`, so
    /// large file contents and outputs are not loaded.
    pub fn session_action_provenance(
        &self,
        session: Uuid,
        with_arguments: &[&str],
        with_results: &[&str],
    ) -> Result<Vec<ActionProvenance>> {
        let arguments = serde_json::to_string(with_arguments)?;
        let results = serde_json::to_string(with_results)?;
        self.call(move |conn| {
            let mut statement = conn.prepare(
                "SELECT json_extract(intent,'$.name'),
                    CASE WHEN json_extract(intent,'$.name') IN(SELECT value FROM json_each(?2)) THEN json_extract(intent,'$.arguments') END,
                    CASE WHEN json_extract(intent,'$.name') IN(SELECT value FROM json_each(?3)) THEN result END
                 FROM actions WHERE session_id=?1 AND state NOT IN('pending','denied') ORDER BY rowid",
            )?;
            let rows = statement.query_map(
                params![session.to_string(), arguments, results],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )?;
            rows.map(|row| {
                let (name, arguments, result) = row?;
                Ok(ActionProvenance {
                    name,
                    arguments: arguments.map(|json| serde_json::from_str(&json)).transpose()?,
                    result: result.map(|json| serde_json::from_str(&json)).transpose()?,
                })
            })
            .collect()
        })
    }
    pub fn memory_page(
        &self,
        scope: &str,
        before: Option<MemoryCursor>,
        limit: usize,
    ) -> Result<MemoryPage> {
        ensure!(
            !scope.is_empty() && scope.len() <= 8192,
            "Choose a project for memory"
        );
        let scope = scope.to_owned();
        let limit = limit.clamp(1, 100);
        self.call(move|conn| {
            let mut statement=conn.prepare("SELECT json FROM memories WHERE scope=?1 AND active=1 AND(?2 IS NULL OR json_extract(json,'$.created_at')<?2 OR(json_extract(json,'$.created_at')=?2 AND id<?3)) ORDER BY json_extract(json,'$.created_at') DESC,id DESC LIMIT ?4")?;
            let rows=statement.query_map(params![scope,before.as_ref().map(|v|v.created_at),before.as_ref().map(|v|v.id.to_string()),(limit+1)as i64],|row|row.get::<_,String>(0))?;
            let mut records=rows.map(|row|Ok(serde_json::from_str::<MemoryRecord>(&row?)?)).collect::<Result<Vec<_>>>()?;
            let has_more=records.len()>limit; records.truncate(limit);
            let before=if has_more {records.last().map(|record|MemoryCursor{created_at:record.created_at,id:record.id})}else{None};
            Ok(MemoryPage{scope,query:String::new(),records,before,has_more})
        })
    }
}
/// Explicit UI export destination. A separate read transaction gives a consistent
/// WAL snapshot without occupying the app's database executor during file I/O.
pub fn export_memories(
    database: &Path,
    scope: &str,
    destination: &Path,
    cancel: &CancellationToken,
) -> Result<MemoryExport> {
    export_fact_versions(
        database,
        scope,
        &crate::ExportSelection::Project,
        destination,
        cancel,
    )
}
/// Exports only explicitly selected fact data. An immutable read transaction
/// fixes both the fact selection and provenance at one committed snapshot.
pub fn export_fact_versions(
    database: &Path,
    scope: &str,
    selection: &crate::ExportSelection,
    destination: &Path,
    cancel: &CancellationToken,
) -> Result<MemoryExport> {
    ensure!(
        !scope.is_empty() && scope.len() <= 8192,
        "Choose a project for memory"
    );
    ensure!(
        destination.is_absolute(),
        "Choose an absolute export location"
    );
    ensure!(!cancel.is_cancelled(), "Memory export cancelled");
    let parent = destination
        .parent()
        .context("Choose an export folder")?
        .canonicalize()?;
    let destination_path = parent.join(
        destination
            .file_name()
            .context("Choose an export filename")?,
    );
    let database_path = database.canonicalize()?;
    let database_name = database_path
        .file_name()
        .context("Memory database filename is unavailable")?
        .to_string_lossy();
    let existing_path = destination
        .canonicalize()
        .unwrap_or_else(|_| destination_path.clone());
    ensure!(
        existing_path != database_path
            && destination_path != database_path.with_file_name(format!("{}-wal", database_name))
            && destination_path != database_path.with_file_name(format!("{}-shm", database_name)),
        "Export cannot replace the memory database"
    );
    let mut staged = tempfile::NamedTempFile::new_in(&parent)?;
    let mut conn = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    let stop = cancel.clone();
    conn.progress_handler(1000, Some(move || stop.is_cancelled()))?;
    let tx = conn.transaction()?;
    let anchor = match selection {
        crate::ExportSelection::Project => None,
        crate::ExportSelection::Chain { id } => {
            let known: Option<String> = tx
                .query_row(
                    "SELECT scope FROM memory_fact_links WHERE id=?1",
                    [id.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            ensure!(
                known.as_deref() == Some(scope),
                "Choose a fact from this project to export"
            );
            Some(id.to_string())
        }
    };
    let revision = crate::memory_facts::revision(&tx, scope)?;
    let connected = crate::memory_facts::CONNECTED;
    let mut records = 0;
    {
        let mut writer = BufWriter::with_capacity(64 * 1024, staged.as_file_mut());
        write!(
            writer,
            "{{\"schema_version\":2,\"scope\":{},\"exported_at\":{},\"revision\":{},\"selection\":{},\"records\":[",
            serde_json::to_string(scope)?,
            now_ms(),
            revision,
            serde_json::to_string(selection)?
        )?;
        let mut statement=tx.prepare(&format!("{connected} SELECT active,json FROM memories WHERE scope=?1 AND(?2 IS NULL OR id IN(SELECT id FROM connected)) ORDER BY json_extract(json,'$.created_at'),id"))?;
        let mut rows = statement.query(params![scope, anchor])?;
        while let Some(row) = rows.next()? {
            ensure!(!cancel.is_cancelled(), "Memory export cancelled");
            let active: bool = row.get(0)?;
            let json: String = row.get(1)?;
            ensure!(
                json.len() <= (262_144 + 8192 + 8192) * 6 + 4096,
                "A saved memory exceeds the export limit"
            );
            let record: MemoryRecord = serde_json::from_str(&json)?;
            ensure!(
                record.content.len() <= 262_144
                    && record.source.len() <= 8192
                    && record.scope.len() <= 8192,
                "A saved memory exceeds the decoded export limit"
            );
            ensure!(record.scope == scope, "Memory scope is inconsistent");
            if records > 0 {
                writer.write_all(b",")?;
            }
            write!(writer, "{{\"active\":{active},\"record\":")?;
            serde_json::to_writer(&mut writer, &record)?;
            writer.write_all(b"}")?;
            records += 1;
        }
        writer.write_all(b"]}\n")?;
        writer.flush()?;
    }
    tx.commit()?;
    ensure!(!cancel.is_cancelled(), "Memory export cancelled");
    staged.as_file().sync_all()?;
    staged
        .persist(&destination_path)
        .map_err(|error| error.error)?;
    #[cfg(unix)]
    File::open(&parent)?.sync_all()?;
    Ok(MemoryExport {
        scope: scope.into(),
        path: destination_path,
        records,
    })
}

#[cfg(test)]
mod tests;
