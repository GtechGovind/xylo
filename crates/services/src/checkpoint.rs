//! The engine checkpoint: the conversation a turn resumes from.
//!
//! A checkpoint is stored one row per message (`checkpoint_messages`, keyed
//! by position) under a header row (`checkpoints`) holding the turn, the
//! message count and a revision that every write increments. A write
//! rewrites only the rows from the first message that changed: appending a
//! tool round writes that round, not the whole history. A writer that knows
//! what it wrote last (its [`CheckpointBase`]) names the first changed
//! message itself; any other write, or one whose base is stale because
//! someone else wrote in between, compares against the stored rows to find
//! it. Either way the stored rows always equal the messages last written.
use crate::store::{Checkpoint, Store, write_transaction};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;
use xylo_core::now_ms;

/// What a writer last stored: the revision its write produced, and the
/// position of the first message that differs from that write now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CheckpointBase {
    /// The revision returned by the writer's previous save.
    pub revision: u64,
    /// Messages before this position are unchanged since that save.
    pub unchanged: usize,
}

impl Store {
    /// Replaces the checkpoint of `session` with `messages` (a JSON array).
    pub fn save_checkpoint(&self, session_id: Uuid, turn_id: Uuid, messages: &Value) -> Result<()> {
        let rows = message_rows(messages)?;
        self.call(move |conn| {
            let tx = write_transaction(conn)?;
            write_rows(&tx, session_id, turn_id, &rows, None, now_ms())?;
            tx.commit()?;
            Ok(())
        })
    }

    /// Replaces the checkpoint of `session` with `messages`, each already
    /// serialized as one JSON value. With a `base` that is still current,
    /// only the messages from `base.unchanged` on are written. Returns the
    /// new revision, the base of the caller's next save.
    pub fn save_checkpoint_messages(
        &self,
        session_id: Uuid,
        turn_id: Uuid,
        messages: Arc<[String]>,
        base: Option<CheckpointBase>,
    ) -> Result<u64> {
        self.call(move |conn| {
            let tx = write_transaction(conn)?;
            let revision = write_rows(&tx, session_id, turn_id, &messages, base, now_ms())?;
            tx.commit()?;
            Ok(revision)
        })
    }

    /// The checkpoint of `session`, if it has one.
    pub fn checkpoint(&self, session_id: Uuid) -> Result<Option<Checkpoint>> {
        self.call(move |conn| {
            let Some(header) = header(conn, session_id)? else {
                return Ok(None);
            };
            let messages = stored_rows(conn, session_id, header.length)?
                .iter()
                .map(|row| serde_json::from_str(row))
                .collect::<serde_json::Result<Vec<Value>>>()
                .context("Checkpoint message is corrupt")?;
            Ok(Some(Checkpoint {
                session_id,
                turn_id: Uuid::parse_str(&header.turn_id)?,
                messages: Value::Array(messages),
                updated_at: header.updated_at,
            }))
        })
    }
}

/// Each message of a JSON array checkpoint, serialized.
pub(crate) fn message_rows(messages: &Value) -> Result<Vec<String>> {
    messages
        .as_array()
        .context("A checkpoint is a list of messages")?
        .iter()
        .map(|message| serde_json::to_string(message).map_err(Into::into))
        .collect()
}

struct Header {
    turn_id: String,
    length: usize,
    revision: u64,
    updated_at: i64,
}

fn header(conn: &Connection, session_id: Uuid) -> Result<Option<Header>> {
    Ok(conn
        .query_row(
            "SELECT turn_id,length,revision,updated_at FROM checkpoints WHERE session_id=?1",
            [session_id.to_string()],
            |row| {
                Ok(Header {
                    turn_id: row.get(0)?,
                    length: crate::store::row_usize(row, 1)?,
                    revision: row.get::<_, i64>(2)? as u64,
                    updated_at: row.get(3)?,
                })
            },
        )
        .optional()?)
}

/// The stored message rows in order; exactly `length` of them, or an error.
fn stored_rows(conn: &Connection, session_id: Uuid, length: usize) -> Result<Vec<String>> {
    let mut statement = conn.prepare_cached(
        "SELECT json FROM checkpoint_messages WHERE session_id=?1 ORDER BY position",
    )?;
    let rows = statement
        .query_map([session_id.to_string()], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(rows.len() == length, "Checkpoint messages are incomplete");
    Ok(rows)
}

/// Writes `messages` as the checkpoint of `session` inside the caller's
/// transaction, rewriting only from the first changed message, and returns
/// the new revision.
pub(crate) fn write_rows(
    conn: &Connection,
    session_id: Uuid,
    turn_id: Uuid,
    messages: &[String],
    base: Option<CheckpointBase>,
    timestamp: i64,
) -> Result<u64> {
    let current = header(conn, session_id)?;
    let first = match &current {
        None => 0,
        Some(current) => match base {
            Some(base) if base.revision == current.revision => {
                base.unchanged.min(current.length).min(messages.len())
            }
            _ => {
                let stored = stored_rows(conn, session_id, current.length)?;
                stored
                    .iter()
                    .zip(messages)
                    .take_while(|(stored, message)| stored == message)
                    .count()
            }
        },
    };
    let revision = current.as_ref().map_or(1, |current| current.revision + 1);
    let session = session_id.to_string();
    conn.execute(
        "INSERT INTO checkpoints(session_id,turn_id,length,revision,updated_at) VALUES(?1,?2,?3,?4,?5)
         ON CONFLICT(session_id) DO UPDATE SET turn_id=excluded.turn_id,length=excluded.length,revision=excluded.revision,updated_at=excluded.updated_at",
        params![session, turn_id.to_string(), messages.len() as i64, revision as i64, timestamp],
    )?;
    conn.execute(
        "DELETE FROM checkpoint_messages WHERE session_id=?1 AND position>=?2",
        params![session, first as i64],
    )?;
    let mut insert = conn.prepare_cached(
        "INSERT INTO checkpoint_messages(session_id,position,json) VALUES(?1,?2,?3)",
    )?;
    for (position, message) in messages.iter().enumerate().skip(first) {
        insert.execute(params![session, position as i64, message])?;
    }
    Ok(revision)
}

/// The turn the checkpoint of `session` belongs to, if it has one.
pub(crate) fn checkpoint_turn(conn: &Connection, session_id: Uuid) -> Result<Option<String>> {
    Ok(header(conn, session_id)?.map(|header| header.turn_id))
}

#[cfg(test)]
mod tests;
