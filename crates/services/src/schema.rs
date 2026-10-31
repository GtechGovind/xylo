//! The one database schema, created whole when a database is first opened.
//!
//! There are no migrations. A database is accepted only when its
//! `user_version` is [`VERSION`] and its schema objects are exactly the ones
//! [`SCHEMA`] creates (plus the per-model vector tables that semantic memory
//! adds at run time). Anything else, including a file that is not a database,
//! is closed and deleted together with its WAL and shared-memory files, and a
//! fresh database is created in its place. Old data is never kept or read.
//!
//! Only the owner of the database (the host) creates or resets it, through
//! [`OpenMode::Owner`]. Every other opener (workers, tools) uses
//! [`OpenMode::Existing`], which validates the same way but fails with an
//! error instead of creating or deleting anything, because the owner may
//! have the file open.
use anyhow::{Context, Result};
use rusqlite::{Connection, ErrorCode, OpenFlags, TransactionBehavior};
use std::{path::Path, sync::OnceLock, time::Duration};

/// Schema generation stored in `PRAGMA user_version`.
pub(crate) const VERSION: i64 = 2;

/// Every table, index and trigger of the application database.
pub(crate) const SCHEMA: &str = "
CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);
CREATE TABLE approval_rules(workspace TEXT NOT NULL,key TEXT NOT NULL,label TEXT NOT NULL,created_at INTEGER NOT NULL,PRIMARY KEY(workspace,key)) WITHOUT ROWID;

CREATE TABLE conversations(id TEXT PRIMARY KEY,json TEXT NOT NULL,updated_at INTEGER NOT NULL);
CREATE INDEX conversations_keyset ON conversations(updated_at DESC,id DESC);
CREATE TABLE messages(id TEXT PRIMARY KEY,session_id TEXT NOT NULL REFERENCES conversations(id),json TEXT NOT NULL,created_at INTEGER NOT NULL);
CREATE INDEX messages_session ON messages(session_id,created_at);
CREATE INDEX messages_session_append ON messages(session_id);
CREATE TABLE checkpoints(session_id TEXT PRIMARY KEY REFERENCES conversations(id),turn_id TEXT NOT NULL,length INTEGER NOT NULL,revision INTEGER NOT NULL,updated_at INTEGER NOT NULL);
CREATE TABLE checkpoint_messages(session_id TEXT NOT NULL REFERENCES checkpoints(session_id),position INTEGER NOT NULL,json TEXT NOT NULL,PRIMARY KEY(session_id,position)) WITHOUT ROWID;
CREATE TABLE completed_turns(session_id TEXT NOT NULL REFERENCES conversations(id),turn_id TEXT NOT NULL,json TEXT NOT NULL,completed_at INTEGER NOT NULL,PRIMARY KEY(session_id,turn_id));

CREATE TABLE actions(id TEXT PRIMARY KEY,session_id TEXT NOT NULL REFERENCES conversations(id),turn_id TEXT NOT NULL,action_key TEXT NOT NULL,intent TEXT NOT NULL,state TEXT NOT NULL,result TEXT,error TEXT,updated_at INTEGER NOT NULL,UNIQUE(session_id,turn_id,action_key));
CREATE INDEX actions_state ON actions(state);
CREATE INDEX actions_name_time ON actions(json_extract(intent,'$.name'),updated_at DESC);
CREATE INDEX action_review_page ON actions(session_id,state,updated_at DESC,id DESC);
CREATE TABLE action_review_clock(id INTEGER PRIMARY KEY CHECK(id=1),seq INTEGER NOT NULL);
INSERT INTO action_review_clock VALUES(1,0);
CREATE TABLE action_review_revisions(action_id TEXT PRIMARY KEY,seq INTEGER NOT NULL);
CREATE TRIGGER action_review_insert AFTER INSERT ON actions BEGIN UPDATE action_review_clock SET seq=seq+1 WHERE id=1;INSERT OR REPLACE INTO action_review_revisions VALUES(new.id,(SELECT seq FROM action_review_clock WHERE id=1));END;
CREATE TRIGGER action_review_update AFTER UPDATE ON actions BEGIN UPDATE action_review_clock SET seq=seq+1 WHERE id=1;INSERT OR REPLACE INTO action_review_revisions VALUES(new.id,(SELECT seq FROM action_review_clock WHERE id=1));END;
CREATE TABLE action_execution_protocol(action_id TEXT PRIMARY KEY,version INTEGER NOT NULL);
CREATE TABLE action_resolutions(operation_id TEXT PRIMARY KEY,action_id TEXT NOT NULL UNIQUE,request TEXT NOT NULL,receipt TEXT NOT NULL,original TEXT NOT NULL,original_sha256 TEXT NOT NULL);
CREATE TRIGGER action_resolution_immutable_update BEFORE UPDATE ON action_resolutions BEGIN SELECT RAISE(ABORT,'Action resolution audit is immutable');END;
CREATE TRIGGER action_resolution_immutable_delete BEFORE DELETE ON action_resolutions BEGIN SELECT RAISE(ABORT,'Action resolution audit is immutable');END;
CREATE TABLE action_review_forms(session_id TEXT PRIMARY KEY,workspace TEXT NOT NULL,revision INTEGER NOT NULL,json TEXT NOT NULL CHECK(length(CAST(json AS BLOB))<=16384));
CREATE TABLE extension_action_bindings(action_id TEXT PRIMARY KEY REFERENCES actions(id) ON DELETE CASCADE,binding TEXT NOT NULL);
CREATE TABLE extension_execution_receipts(id TEXT PRIMARY KEY,intent TEXT NOT NULL,state TEXT NOT NULL,result TEXT,error TEXT,updated_at INTEGER NOT NULL);

CREATE TABLE connectors(id TEXT PRIMARY KEY,name TEXT NOT NULL,json TEXT NOT NULL);
CREATE TABLE connector_tools(id TEXT PRIMARY KEY,fingerprint TEXT NOT NULL,json TEXT NOT NULL,updated_at INTEGER NOT NULL);

CREATE TABLE memories(id TEXT PRIMARY KEY,scope TEXT NOT NULL,content TEXT NOT NULL,json TEXT NOT NULL,active INTEGER NOT NULL DEFAULT 1,embedding_model TEXT);
CREATE INDEX memories_scope ON memories(scope,active);
CREATE INDEX memories_recent ON memories(scope,active,json_extract(json,'$.created_at') DESC,id DESC);
CREATE INDEX memory_fact_versions ON memories(scope,json_extract(json,'$.created_at') DESC,id DESC);
CREATE VIRTUAL TABLE memories_fts USING fts5(id UNINDEXED,scope UNINDEXED,content,tokenize='unicode61');
CREATE TABLE memory_vector_tables(name TEXT PRIMARY KEY,model TEXT NOT NULL,dimensions INTEGER NOT NULL);
CREATE TABLE memory_fact_links(id TEXT PRIMARY KEY,scope TEXT NOT NULL,parent TEXT);
CREATE INDEX memory_fact_parents ON memory_fact_links(scope,parent,id);
CREATE TABLE memory_fact_scopes(scope TEXT PRIMARY KEY,revision INTEGER NOT NULL CHECK(revision>=0),generation INTEGER NOT NULL DEFAULT 0);
CREATE TABLE memory_fact_deleted(id TEXT PRIMARY KEY,scope TEXT NOT NULL,deleted_at INTEGER NOT NULL);
CREATE INDEX memory_fact_deleted_age ON memory_fact_deleted(deleted_at);
CREATE TABLE memory_fact_receipts(operation_id TEXT PRIMARY KEY,request TEXT NOT NULL,receipt TEXT NOT NULL,created_at INTEGER NOT NULL);
CREATE TABLE memory_forms(scope TEXT PRIMARY KEY,json TEXT NOT NULL CHECK(length(CAST(json AS BLOB))<=6291456),revision INTEGER NOT NULL CHECK(revision>0),updated_at INTEGER NOT NULL,mutation_id TEXT NOT NULL,expected INTEGER NOT NULL);
CREATE TABLE memory_form_usage(singleton INTEGER PRIMARY KEY CHECK(singleton=1),bytes INTEGER NOT NULL DEFAULT 0,rows INTEGER NOT NULL DEFAULT 0);
INSERT INTO memory_form_usage(singleton) VALUES(1);
CREATE TRIGGER memory_form_insert AFTER INSERT ON memory_forms BEGIN UPDATE memory_form_usage SET bytes=bytes+length(CAST(new.json AS BLOB)),rows=rows+1 WHERE singleton=1; END;
CREATE TRIGGER memory_form_update AFTER UPDATE ON memory_forms BEGIN UPDATE memory_form_usage SET bytes=bytes-length(CAST(old.json AS BLOB))+length(CAST(new.json AS BLOB)) WHERE singleton=1; END;
CREATE TRIGGER memory_form_delete AFTER DELETE ON memory_forms BEGIN UPDATE memory_form_usage SET bytes=bytes-length(CAST(old.json AS BLOB)),rows=rows-1 WHERE singleton=1; END;

CREATE TABLE skills(name TEXT NOT NULL,version INTEGER NOT NULL,json TEXT NOT NULL,PRIMARY KEY(name,version));
CREATE TABLE skill_promotions(id INTEGER PRIMARY KEY,name TEXT NOT NULL,version INTEGER NOT NULL,created_at INTEGER NOT NULL,FOREIGN KEY(name,version) REFERENCES skills(name,version));
CREATE TABLE learning_jobs(id INTEGER PRIMARY KEY,session_id TEXT NOT NULL,turn_id TEXT NOT NULL,state TEXT NOT NULL,owner TEXT,lease_until INTEGER NOT NULL DEFAULT 0,error TEXT,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL,UNIQUE(session_id,turn_id));
CREATE INDEX learning_jobs_pending ON learning_jobs(state,id);
CREATE TABLE learning_cases(id INTEGER PRIMARY KEY,session_id TEXT NOT NULL,turn_id TEXT NOT NULL,scope TEXT NOT NULL,workflow_key TEXT NOT NULL,json TEXT NOT NULL,UNIQUE(session_id,turn_id));
CREATE INDEX learning_cases_match ON learning_cases(scope,workflow_key,id DESC);
CREATE INDEX learning_cases_negative ON learning_cases(scope,workflow_key,json_extract(json,'$.eligible'),id);
CREATE TABLE learning_candidates(name TEXT NOT NULL,version INTEGER NOT NULL,scope TEXT NOT NULL,workflow_key TEXT,state TEXT NOT NULL,json TEXT NOT NULL,PRIMARY KEY(name,version),FOREIGN KEY(name,version) REFERENCES skills(name,version));
CREATE INDEX learning_candidates_scope ON learning_candidates(scope,state);
CREATE VIRTUAL TABLE learning_fts USING fts5(name UNINDEXED,version UNINDEXED,scope UNINDEXED,content,tokenize='unicode61');
CREATE TABLE learning_retrievals(session_id TEXT NOT NULL,turn_id TEXT NOT NULL,name TEXT NOT NULL,version INTEGER NOT NULL,scope TEXT NOT NULL,outcome TEXT NOT NULL,created_at INTEGER NOT NULL,PRIMARY KEY(session_id,turn_id,name,version));
CREATE INDEX learning_retrievals_scope ON learning_retrievals(scope,created_at DESC);

CREATE TABLE composer_draft_usage(id INTEGER PRIMARY KEY CHECK(id=1),total_bytes INTEGER NOT NULL DEFAULT 0,row_count INTEGER NOT NULL DEFAULT 0);
INSERT INTO composer_draft_usage(id) VALUES(1);
CREATE TABLE composer_drafts(session_id TEXT NOT NULL,workspace TEXT NOT NULL,text TEXT NOT NULL CHECK(length(CAST(text AS BLOB))<=262144),attachments TEXT NOT NULL,revision INTEGER NOT NULL CHECK(revision>0),updated_at INTEGER NOT NULL,last_mutation TEXT NOT NULL,last_expected INTEGER NOT NULL,PRIMARY KEY(session_id,workspace));
CREATE INDEX composer_drafts_recent ON composer_drafts(workspace,updated_at DESC,session_id DESC);
CREATE TRIGGER composer_drafts_insert AFTER INSERT ON composer_drafts BEGIN UPDATE composer_draft_usage SET total_bytes=total_bytes+length(CAST(new.text AS BLOB))+length(CAST(new.attachments AS BLOB)),row_count=row_count+1 WHERE id=1; END;
CREATE TRIGGER composer_drafts_update AFTER UPDATE ON composer_drafts BEGIN UPDATE composer_draft_usage SET total_bytes=total_bytes-length(CAST(old.text AS BLOB))-length(CAST(old.attachments AS BLOB))+length(CAST(new.text AS BLOB))+length(CAST(new.attachments AS BLOB)) WHERE id=1; END;
CREATE TRIGGER composer_drafts_delete AFTER DELETE ON composer_drafts BEGIN UPDATE composer_draft_usage SET total_bytes=total_bytes-length(CAST(old.text AS BLOB))-length(CAST(old.attachments AS BLOB)),row_count=row_count-1 WHERE id=1; END;

CREATE TABLE turn_queue(sequence INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT NOT NULL UNIQUE,session_id TEXT NOT NULL,workspace TEXT NOT NULL,revision INTEGER NOT NULL,turn TEXT,submission TEXT,state TEXT NOT NULL CHECK(state IN('queued','removed','started')),paused INTEGER NOT NULL DEFAULT 0,reason TEXT,cleared_revision INTEGER,updated_at INTEGER NOT NULL,UNIQUE(session_id,workspace,revision));
CREATE INDEX turn_queue_fifo ON turn_queue(session_id,state,sequence);
CREATE INDEX turn_queue_tombstones ON turn_queue(state,updated_at);
CREATE TABLE user_guidance(id TEXT PRIMARY KEY,session_id TEXT NOT NULL,turn_id TEXT NOT NULL,workspace TEXT NOT NULL,draft_revision INTEGER NOT NULL,text TEXT NOT NULL,submission TEXT NOT NULL,state TEXT NOT NULL CHECK(state IN('queued','applied','cancelled')),reason TEXT,created_at INTEGER NOT NULL,applied_at INTEGER,cleared_revision INTEGER,updated_at INTEGER NOT NULL,UNIQUE(session_id,turn_id,workspace,draft_revision));
CREATE INDEX user_guidance_pending ON user_guidance(session_id,turn_id,state,created_at,id);
CREATE INDEX user_guidance_finished ON user_guidance(state,updated_at);

CREATE TABLE document_drafts(workspace TEXT NOT NULL,source TEXT NOT NULL,json TEXT NOT NULL CHECK(json_valid(json) AND length(CAST(json AS BLOB))<=6291456),revision INTEGER NOT NULL CHECK(revision>0),updated_at INTEGER NOT NULL,mutation_id TEXT NOT NULL,expected INTEGER NOT NULL,PRIMARY KEY(workspace,source));
CREATE INDEX document_draft_recent ON document_drafts(workspace,updated_at DESC,source ASC);
CREATE TABLE document_draft_usage(singleton INTEGER PRIMARY KEY CHECK(singleton=1),bytes INTEGER NOT NULL DEFAULT 0,rows INTEGER NOT NULL DEFAULT 0);
INSERT INTO document_draft_usage(singleton) VALUES(1);
CREATE TRIGGER document_draft_insert AFTER INSERT ON document_drafts BEGIN UPDATE document_draft_usage SET bytes=bytes+length(CAST(new.json AS BLOB)),rows=rows+1 WHERE singleton=1; END;
CREATE TRIGGER document_draft_update AFTER UPDATE ON document_drafts BEGIN UPDATE document_draft_usage SET bytes=bytes-length(CAST(old.json AS BLOB))+length(CAST(new.json AS BLOB)) WHERE singleton=1; END;
CREATE TRIGGER document_draft_delete AFTER DELETE ON document_drafts BEGIN UPDATE document_draft_usage SET bytes=bytes-length(CAST(old.json AS BLOB)),rows=rows-1 WHERE singleton=1; END;
CREATE TABLE document_draft_relinks(operation_id TEXT PRIMARY KEY,request TEXT NOT NULL CHECK(json_valid(request)),receipt TEXT NOT NULL CHECK(json_valid(receipt)));
CREATE TRIGGER document_relink_immutable_update BEFORE UPDATE ON document_draft_relinks BEGIN SELECT RAISE(ABORT,'Document relink receipt is immutable');END;
CREATE TRIGGER document_relink_immutable_delete BEFORE DELETE ON document_draft_relinks BEGIN SELECT RAISE(ABORT,'Document relink receipt is immutable');END;

CREATE TABLE history_search_documents(id INTEGER PRIMARY KEY,session_id TEXT NOT NULL,message_id TEXT,message_rowid INTEGER,content TEXT NOT NULL);
CREATE INDEX history_search_documents_session ON history_search_documents(session_id,message_rowid);
CREATE VIRTUAL TABLE history_search_fts USING fts5(content,content='history_search_documents',content_rowid='id',tokenize='unicode61');
CREATE TRIGGER history_docs_insert AFTER INSERT ON history_search_documents BEGIN INSERT INTO history_search_fts(rowid,content) VALUES(new.id,new.content); END;
CREATE TRIGGER history_docs_delete AFTER DELETE ON history_search_documents BEGIN INSERT INTO history_search_fts(history_search_fts,rowid,content) VALUES('delete',old.id,old.content); END;
CREATE TRIGGER history_docs_update AFTER UPDATE ON history_search_documents BEGIN INSERT INTO history_search_fts(history_search_fts,rowid,content) VALUES('delete',old.id,old.content); INSERT INTO history_search_fts(rowid,content) VALUES(new.id,new.content); END;
CREATE TRIGGER history_messages_insert AFTER INSERT ON messages BEGIN INSERT INTO history_search_documents(id,session_id,message_id,message_rowid,content) VALUES(new.rowid*2,new.session_id,new.id,new.rowid,COALESCE(json_extract(new.json,'$.content'),'')) ON CONFLICT(id) DO UPDATE SET session_id=excluded.session_id,message_id=excluded.message_id,message_rowid=excluded.message_rowid,content=excluded.content; END;
CREATE TRIGGER history_messages_update AFTER UPDATE ON messages BEGIN DELETE FROM history_search_documents WHERE id=old.rowid*2; INSERT INTO history_search_documents(id,session_id,message_id,message_rowid,content) VALUES(new.rowid*2,new.session_id,new.id,new.rowid,COALESCE(json_extract(new.json,'$.content'),'')) ON CONFLICT(id) DO UPDATE SET session_id=excluded.session_id,message_id=excluded.message_id,message_rowid=excluded.message_rowid,content=excluded.content; END;
CREATE TRIGGER history_messages_delete AFTER DELETE ON messages BEGIN DELETE FROM history_search_documents WHERE id=old.rowid*2; END;
CREATE TRIGGER history_conversations_insert AFTER INSERT ON conversations BEGIN INSERT INTO history_search_documents(id,session_id,message_id,message_rowid,content) VALUES(new.rowid*2+1,new.id,NULL,NULL,COALESCE(json_extract(new.json,'$.title'),'')) ON CONFLICT(id) DO UPDATE SET session_id=excluded.session_id,content=excluded.content; END;
CREATE TRIGGER history_conversations_update AFTER UPDATE ON conversations WHEN old.id IS NOT new.id OR old.rowid IS NOT new.rowid OR json_extract(old.json,'$.title') IS NOT json_extract(new.json,'$.title') BEGIN DELETE FROM history_search_documents WHERE id=old.rowid*2+1; INSERT INTO history_search_documents(id,session_id,message_id,message_rowid,content) VALUES(new.rowid*2+1,new.id,NULL,NULL,COALESCE(json_extract(new.json,'$.title'),'')) ON CONFLICT(id) DO UPDATE SET session_id=excluded.session_id,content=excluded.content; END;
CREATE TRIGGER history_conversations_delete AFTER DELETE ON conversations BEGIN DELETE FROM history_search_documents WHERE session_id=old.id; END;
";

/// Opening replaced a database that did not match [`SCHEMA`] with a new,
/// empty one. Its `Display` text is the one-time notice for the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseReset {
    /// Why the old database could not be used.
    pub reason: String,
}

impl std::fmt::Display for DatabaseReset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "The local database was replaced with a new, empty one because {}. Previous chats, settings and history were deleted.",
            self.reason
        )
    }
}

/// Who is opening the database, which decides what may happen to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpenMode {
    /// The host: creates the schema in a new file and replaces a database
    /// with any other schema.
    Owner,
    /// Any other process: the file must exist with exactly this schema;
    /// anything else is an error and the file is left untouched.
    Existing,
}

/// Opens `path` in `mode`. For [`OpenMode::Owner`] the result says whether an
/// unusable database was replaced.
pub(crate) fn open(path: &Path, mode: OpenMode) -> Result<(Connection, Option<DatabaseReset>)> {
    match open_connection(path, mode)? {
        Opened::Ready(conn) => Ok((conn, None)),
        Opened::Unexpected(reason) if mode == OpenMode::Existing => anyhow::bail!(
            "The database at {} cannot be used: {reason}. Only the application host may create or reset it.",
            path.display()
        ),
        Opened::Unexpected(reason) => {
            delete_database(path)?;
            match open_connection(path, mode)? {
                Opened::Ready(conn) => Ok((conn, Some(DatabaseReset { reason }))),
                Opened::Unexpected(reason) => {
                    anyhow::bail!("A fresh database could not be created: {reason}")
                }
            }
        }
    }
}

enum Opened {
    Ready(Connection),
    Unexpected(String),
}

fn open_connection(path: &Path, mode: OpenMode) -> Result<Opened> {
    let conn = match mode {
        OpenMode::Owner => Connection::open(path),
        OpenMode::Existing => Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ),
    }
    .with_context(|| format!("Open xylo database {}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    match prepare(conn, mode) {
        Ok(opened) => Ok(opened),
        Err(error) if is_not_a_database(&error) => {
            Ok(Opened::Unexpected("the file is not a database".into()))
        }
        Err(error) => Err(error),
    }
}

/// Classification happens inside an immediate transaction so two processes
/// opening a new file at once cannot both create the schema.
fn prepare(mut conn: Connection, mode: OpenMode) -> Result<Opened> {
    // WAL is a persistent property of the file, so only the owner sets it;
    // changing the journal mode while other connections race on a new file
    // fails with SQLITE_BUSY instead of waiting.
    if mode == OpenMode::Owner {
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
    }
    conn.execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;")?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let objects = objects(&tx)?;
    if version == 0 && objects.is_empty() {
        if mode == OpenMode::Existing {
            return Ok(Opened::Unexpected("it has not been created yet".into()));
        }
        tx.execute_batch(SCHEMA)?;
        tx.pragma_update(None, "user_version", VERSION)?;
        tx.commit()?;
    } else if version != VERSION {
        return Ok(Opened::Unexpected(format!(
            "it uses schema version {version}"
        )));
    } else if objects != *reference()? {
        return Ok(Opened::Unexpected(
            "its tables do not match this version".into(),
        ));
    } else {
        tx.commit()?;
    }
    let _: String = conn
        .query_row("SELECT vec_version()", [], |row| row.get(0))
        .context("Initialize sqlite-vec")?;
    Ok(Opened::Ready(conn))
}

type SchemaObject = (String, String, Option<String>);

/// Schema objects that define the database. SQLite's internal tables and the
/// per-model vector tables (and their shadow tables) are created at run time.
fn objects(conn: &Connection) -> Result<Vec<SchemaObject>> {
    let mut statement = conn.prepare(
        "SELECT type,name,sql FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*' AND name NOT GLOB 'memory_vec_*' ORDER BY type,name",
    )?;
    let rows = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// The objects [`SCHEMA`] creates, computed once per process in memory.
fn reference() -> Result<&'static Vec<SchemaObject>> {
    static REFERENCE: OnceLock<std::result::Result<Vec<SchemaObject>, String>> = OnceLock::new();
    REFERENCE
        .get_or_init(|| {
            let build = || -> Result<Vec<SchemaObject>> {
                let conn = Connection::open_in_memory()?;
                conn.execute_batch(SCHEMA)?;
                objects(&conn)
            };
            build().map_err(|error| format!("{error:#}"))
        })
        .as_ref()
        .map_err(|error| anyhow::anyhow!("Invalid built-in database schema: {error}"))
}

fn is_not_a_database(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<rusqlite::Error>()
            .and_then(rusqlite::Error::sqlite_error_code)
            .is_some_and(|code| code == ErrorCode::NotADatabase)
    })
}

/// Deletes the database and its WAL/shared-memory companions.
fn delete_database(path: &Path) -> Result<()> {
    let name = path
        .file_name()
        .context("Database has no file name")?
        .to_string_lossy()
        .into_owned();
    for file in [
        path.with_file_name(format!("{name}-wal")),
        path.with_file_name(format!("{name}-shm")),
        path.to_owned(),
    ] {
        match std::fs::remove_file(&file) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("Delete {}", file.display()));
            }
        }
    }
    tracing::warn!(path = %path.display(), "Replaced an unusable database");
    Ok(())
}
