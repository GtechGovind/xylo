//! Database-wide history search and message windows. The search index is part
//! of the schema from the moment a database is created, and triggers keep every
//! later write searchable atomically, so there is no separate indexing phase.
use crate::Store;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use xylo_core::{ChatMessage, Conversation, ConversationCursor};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryMatch {
    pub conversation: Conversation,
    pub snippet: String,
    pub message_id: Option<Uuid>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistorySearchPage {
    pub query: String,
    pub items: Vec<HistoryMatch>,
    pub before: Option<ConversationCursor>,
    pub has_more: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryWindow {
    pub session_id: Uuid,
    pub anchor: Option<Uuid>,
    pub messages: Vec<ChatMessage>,
    /// Append-row cursors, independent of timestamp ties/clock changes.
    pub before: Option<i64>,
    pub after: Option<i64>,
    pub has_older: bool,
    pub has_newer: bool,
    pub append: bool,
    /// Echoes the requested newer-page cursor so the UI can reject stale pages.
    pub request_after: Option<i64>,
}

fn phrase(query: &str) -> String {
    query
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .take(8)
        .map(|word| format!("\"{word}\"*"))
        .collect::<Vec<_>>()
        .join(" AND ")
}
fn rows(
    conn: &Connection,
    session: Uuid,
    predicate: &str,
    cursor: i64,
    descending: bool,
    limit: usize,
) -> Result<Vec<(i64, ChatMessage)>> {
    // Predicate/order are fixed call-site literals, never user query text.
    let sql = format!(
        "SELECT rowid,json FROM messages WHERE session_id=?1 AND rowid{predicate}?2 ORDER BY rowid {} LIMIT ?3",
        if descending { "DESC" } else { "ASC" }
    );
    let mut statement = conn.prepare(&sql)?;
    let result = statement
        .query_map(params![session.to_string(), cursor, limit as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
    result
        .map(|row| {
            let (id, json) = row?;
            Ok((id, serde_json::from_str(&json)?))
        })
        .collect()
}
fn window(
    conn: &Connection,
    session: Uuid,
    anchor: Option<Uuid>,
    data: Vec<(i64, ChatMessage)>,
    append: bool,
) -> Result<HistoryWindow> {
    let before = data.first().map(|row| row.0);
    let after = data.last().map(|row| row.0);
    let has_older = if let Some(before) = before {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE session_id=?1 AND rowid<?2)",
            params![session.to_string(), before],
            |row| row.get(0),
        )?
    } else {
        false
    };
    let has_newer = if let Some(after) = after {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE session_id=?1 AND rowid>?2)",
            params![session.to_string(), after],
            |row| row.get(0),
        )?
    } else {
        false
    };
    Ok(HistoryWindow {
        session_id: session,
        anchor,
        messages: data.into_iter().map(|row| row.1).collect(),
        before,
        after,
        has_older,
        has_newer,
        append,
        request_after: None,
    })
}
impl Store {
    /// The best-ranked document of each matching conversation (a message or the
    /// title; ties go to the newest), with pages ordered by conversation
    /// (updated_at,id) rather than by an unstable floating-point rank cursor.
    pub fn search_history(
        &self,
        query: &str,
        before: Option<ConversationCursor>,
        limit: usize,
    ) -> Result<HistorySearchPage> {
        ensure!(query.len() <= 512, "History search query exceeds 512 bytes");
        let query = query.to_owned();
        let matched = phrase(&query);
        self.call(move|conn|{
            if matched.is_empty(){return Ok(HistorySearchPage{query,items:Vec::new(),before:None,has_more:false});}
            let tx=conn.transaction()?;
            let mut statement=tx.prepare("WITH hits AS MATERIALIZED (SELECT session_id,doc_id FROM (SELECT d.session_id,d.id doc_id,row_number() OVER (PARTITION BY d.session_id ORDER BY history_search_fts.rank,d.id DESC) best FROM history_search_fts JOIN history_search_documents d ON d.id=history_search_fts.rowid WHERE history_search_fts MATCH ?1) WHERE best=1) SELECT c.json,d.message_id,substr(snippet(history_search_fts,0,'','',' … ',24),1,320) FROM hits h JOIN conversations c ON c.id=h.session_id JOIN history_search_documents d ON d.id=h.doc_id JOIN history_search_fts ON history_search_fts.rowid=d.id WHERE history_search_fts MATCH ?1 AND json_extract(c.json,'$.archived') IS NOT 1 AND (?2 IS NULL OR c.updated_at<?2 OR(c.updated_at=?2 AND c.id<?3)) ORDER BY c.updated_at DESC,c.id DESC LIMIT ?4")?;
            let cursor=before.as_ref().map(|cursor|cursor.updated_at);let id=before.as_ref().map(|cursor|cursor.id.to_string());let limit=limit.clamp(1,200);
            let rows=statement.query_map(params![matched,cursor,id,(limit+1)as i64],|row|Ok((row.get::<_,String>(0)?,row.get::<_,Option<String>>(1)?,row.get::<_,String>(2)?)))?;
            let mut items=rows.map(|row|{let(conversation,message,snippet)=row?;Ok(HistoryMatch{conversation:serde_json::from_str(&conversation)?,message_id:message.map(|id|Uuid::parse_str(&id)).transpose()?,snippet})}).collect::<Result<Vec<_>>>()?;drop(statement);
            let has_more=items.len()>limit;items.truncate(limit);let before=if has_more{items.last().map(|item|ConversationCursor{updated_at:item.conversation.updated_at,id:item.conversation.id})}else{None};
            tx.commit()?;Ok(HistorySearchPage{query,items,before,has_more})
        })
    }
    /// Center a bounded page on the exact source message. The anchor must belong
    /// to the requested session; timestamps do not participate in append ordering.
    pub fn history_window(
        &self,
        session: Uuid,
        anchor: Uuid,
        limit: usize,
    ) -> Result<HistoryWindow> {
        self.call(move |conn| {
            let tx = conn.transaction()?;
            let id: i64 = tx
                .query_row(
                    "SELECT rowid FROM messages WHERE id=?1 AND session_id=?2",
                    params![anchor.to_string(), session.to_string()],
                    |row| row.get(0),
                )
                .optional()?
                .context("Matching message is no longer available in this conversation")?;
            let limit = limit.clamp(1, 200);
            let mut older = rows(&tx, session, "<=", id, true, limit / 2 + 1)?;
            older.reverse();
            let newer = rows(&tx, session, ">", id, false, limit - older.len())?;
            older.extend(newer);
            let result = window(&tx, session, Some(anchor), older, false)?;
            tx.commit()?;
            Ok(result)
        })
    }
    /// Next append-order page after a previously returned window/page cursor.
    pub fn history_after(&self, session: Uuid, after: i64, limit: usize) -> Result<HistoryWindow> {
        ensure!(after > 0, "Invalid history append cursor");
        self.call(move |conn| {
            let tx = conn.transaction()?;
            let data = rows(&tx, session, ">", after, false, limit.clamp(1, 200))?;
            let mut result = window(&tx, session, None, data, true)?;
            result.request_after = Some(after);
            tx.commit()?;
            Ok(result)
        })
    }
}

#[cfg(test)]
mod tests;
