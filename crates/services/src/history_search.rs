//! Database-wide history search and message windows. Initialization/backfill is
//! lazy and bounded; incremental triggers keep later writes searchable atomically.
use crate::Store;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use workbench_core::{ChatMessage, Conversation, ConversationCursor};

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
    pub indexing: bool,
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

fn initialize_schema(conn: &Connection) -> Result<()> {
    // These tables are an optional derived index, not a new application schema
    // version. Every statement is idempotent for cooperating Store connections.
    conn.execute_batch("CREATE TABLE IF NOT EXISTS history_search_state(id INTEGER PRIMARY KEY CHECK(id=1),message_cursor INTEGER NOT NULL,conversation_cursor INTEGER NOT NULL,message_target INTEGER NOT NULL,conversation_target INTEGER NOT NULL,ready INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS history_search_documents(id INTEGER PRIMARY KEY,session_id TEXT NOT NULL,message_id TEXT,message_rowid INTEGER,content TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS history_search_documents_session ON history_search_documents(session_id,message_rowid);
CREATE INDEX IF NOT EXISTS messages_session_append ON messages(session_id);
CREATE VIRTUAL TABLE IF NOT EXISTS history_search_fts USING fts5(content,content='history_search_documents',content_rowid='id',tokenize='unicode61');
CREATE TRIGGER IF NOT EXISTS history_docs_insert AFTER INSERT ON history_search_documents BEGIN INSERT INTO history_search_fts(rowid,content) VALUES(new.id,new.content); END;
CREATE TRIGGER IF NOT EXISTS history_docs_delete AFTER DELETE ON history_search_documents BEGIN INSERT INTO history_search_fts(history_search_fts,rowid,content) VALUES('delete',old.id,old.content); END;
CREATE TRIGGER IF NOT EXISTS history_docs_update AFTER UPDATE ON history_search_documents BEGIN INSERT INTO history_search_fts(history_search_fts,rowid,content) VALUES('delete',old.id,old.content); INSERT INTO history_search_fts(rowid,content) VALUES(new.id,new.content); END;
CREATE TRIGGER IF NOT EXISTS history_messages_insert AFTER INSERT ON messages BEGIN INSERT INTO history_search_documents(id,session_id,message_id,message_rowid,content) VALUES(new.rowid*2,new.session_id,new.id,new.rowid,COALESCE(json_extract(new.json,'$.content'),'')) ON CONFLICT(id) DO UPDATE SET session_id=excluded.session_id,message_id=excluded.message_id,message_rowid=excluded.message_rowid,content=excluded.content; END;
CREATE TRIGGER IF NOT EXISTS history_messages_update AFTER UPDATE ON messages BEGIN DELETE FROM history_search_documents WHERE id=old.rowid*2; INSERT INTO history_search_documents(id,session_id,message_id,message_rowid,content) VALUES(new.rowid*2,new.session_id,new.id,new.rowid,COALESCE(json_extract(new.json,'$.content'),'')) ON CONFLICT(id) DO UPDATE SET session_id=excluded.session_id,message_id=excluded.message_id,message_rowid=excluded.message_rowid,content=excluded.content; END;
CREATE TRIGGER IF NOT EXISTS history_messages_delete AFTER DELETE ON messages BEGIN DELETE FROM history_search_documents WHERE id=old.rowid*2; END;
CREATE TRIGGER IF NOT EXISTS history_conversations_insert AFTER INSERT ON conversations BEGIN INSERT INTO history_search_documents(id,session_id,message_id,message_rowid,content) VALUES(new.rowid*2+1,new.id,NULL,NULL,COALESCE(json_extract(new.json,'$.title'),'')) ON CONFLICT(id) DO UPDATE SET session_id=excluded.session_id,content=excluded.content; END;
CREATE TRIGGER IF NOT EXISTS history_conversations_update AFTER UPDATE ON conversations WHEN old.id IS NOT new.id OR old.rowid IS NOT new.rowid OR json_extract(old.json,'$.title') IS NOT json_extract(new.json,'$.title') BEGIN DELETE FROM history_search_documents WHERE id=old.rowid*2+1; INSERT INTO history_search_documents(id,session_id,message_id,message_rowid,content) VALUES(new.rowid*2+1,new.id,NULL,NULL,COALESCE(json_extract(new.json,'$.title'),'')) ON CONFLICT(id) DO UPDATE SET session_id=excluded.session_id,content=excluded.content; END;
CREATE TRIGGER IF NOT EXISTS history_conversations_delete AFTER DELETE ON conversations BEGIN DELETE FROM history_search_documents WHERE session_id=old.id; END;
INSERT OR IGNORE INTO history_search_state(id,message_cursor,conversation_cursor,message_target,conversation_target,ready) SELECT 1,0,0,COALESCE((SELECT MAX(rowid) FROM messages),0),COALESCE((SELECT MAX(rowid) FROM conversations),0),0;")?;
    Ok(())
}
fn ensure_initialized(conn: &mut Connection) -> Result<()> {
    let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='history_search_state')",[],|row|row.get(0))?;
    if !exists {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        initialize_schema(&tx)?;
        tx.commit()?;
    }
    Ok(())
}
fn ready(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT ready FROM history_search_state WHERE id=1",
        [],
        |row| row.get(0),
    )?)
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
    /// At most `limit` records (1..500) and approximately 4 MiB of body text per
    /// transaction. True means initial backfill is complete; triggers cover writes.
    pub fn index_history_chunk(&self, limit: usize) -> Result<bool> {
        self.call(move|conn|{
            ensure_initialized(conn)?;let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;if ready(&tx)?{tx.commit()?;return Ok(true);}
            let(mut message_cursor,mut conversation_cursor,message_target,conversation_target):(i64,i64,i64,i64)=tx.query_row("SELECT message_cursor,conversation_cursor,message_target,conversation_target FROM history_search_state WHERE id=1",[],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)))?;
            let mut remaining=limit.clamp(1,500);let mut bytes=0_usize;
            {
                let mut statement=tx.prepare("SELECT rowid,session_id,id,COALESCE(json_extract(json,'$.content'),'') FROM messages WHERE rowid>?1 AND rowid<=?2 ORDER BY rowid LIMIT ?3")?;
                let mut rows=statement.query(params![message_cursor,message_target,remaining as i64])?;
                while let Some(row)=rows.next()? {
                    let id:i64=row.get(0)?;let session:String=row.get(1)?;let message:String=row.get(2)?;let content:String=row.get(3)?;
                    if bytes>0 && bytes.saturating_add(content.len())>4*1024*1024{break;}
                    ensure!(id<=i64::MAX/2,"Message append cursor exceeds index capacity");
                    tx.execute("INSERT OR IGNORE INTO history_search_documents(id,session_id,message_id,message_rowid,content) VALUES(?1,?2,?3,?4,?5)",params![id*2,session,message,id,content])?;
                    message_cursor=id;bytes+=content.len();remaining-=1;
                }
            }
            if remaining>0 && bytes<4*1024*1024 {
                let mut statement=tx.prepare("SELECT rowid,id,COALESCE(json_extract(json,'$.title'),'') FROM conversations WHERE rowid>?1 AND rowid<=?2 ORDER BY rowid LIMIT ?3")?;
                let mut rows=statement.query(params![conversation_cursor,conversation_target,remaining as i64])?;
                while let Some(row)=rows.next()?{
                    let id:i64=row.get(0)?;let session:String=row.get(1)?;let content:String=row.get(2)?;
                    if bytes>0 && bytes.saturating_add(content.len())>4*1024*1024{break;}
                    ensure!(id<i64::MAX/2,"Conversation append cursor exceeds index capacity");
                    tx.execute("INSERT OR IGNORE INTO history_search_documents(id,session_id,message_id,message_rowid,content) VALUES(?1,?2,NULL,NULL,?3)",params![id*2+1,session,content])?;
                    conversation_cursor=id;bytes+=content.len();
                }
            }
            let incomplete:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE rowid>?1 AND rowid<=?2) OR EXISTS(SELECT 1 FROM conversations WHERE rowid>?3 AND rowid<=?4)",params![message_cursor,message_target,conversation_cursor,conversation_target],|row|row.get(0))?;
            tx.execute("UPDATE history_search_state SET message_cursor=?1,conversation_cursor=?2,ready=?3 WHERE id=1",params![message_cursor,conversation_cursor,!incomplete])?;
            tx.commit()?;Ok(!incomplete)
        })
    }
    /// One best source-message hit per conversation, ordered by conversation
    /// (updated_at,id), rather than an unstable floating-point rank cursor.
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
            ensure_initialized(conn)?;let tx=conn.transaction()?;let indexing=!ready(&tx)?;
            if matched.is_empty(){tx.commit()?;return Ok(HistorySearchPage{query,items:Vec::new(),before:None,has_more:false,indexing});}
            let mut statement=tx.prepare("WITH hits AS MATERIALIZED (SELECT d.session_id,COALESCE(MAX(CASE WHEN d.message_id IS NOT NULL THEN d.id END),MAX(d.id)) doc_id FROM history_search_fts JOIN history_search_documents d ON d.id=history_search_fts.rowid WHERE history_search_fts MATCH ?1 GROUP BY d.session_id) SELECT c.json,d.message_id,substr(snippet(history_search_fts,0,'','',' … ',24),1,320) FROM hits h JOIN conversations c ON c.id=h.session_id JOIN history_search_documents d ON d.id=h.doc_id JOIN history_search_fts ON history_search_fts.rowid=d.id WHERE history_search_fts MATCH ?1 AND (?2 IS NULL OR c.updated_at<?2 OR(c.updated_at=?2 AND c.id<?3)) ORDER BY c.updated_at DESC,c.id DESC LIMIT ?4")?;
            let cursor=before.as_ref().map(|cursor|cursor.updated_at);let id=before.as_ref().map(|cursor|cursor.id.to_string());let limit=limit.clamp(1,200);
            let rows=statement.query_map(params![matched,cursor,id,(limit+1)as i64],|row|Ok((row.get::<_,String>(0)?,row.get::<_,Option<String>>(1)?,row.get::<_,String>(2)?)))?;
            let mut items=rows.map(|row|{let(conversation,message,snippet)=row?;Ok(HistoryMatch{conversation:serde_json::from_str(&conversation)?,message_id:message.map(|id|Uuid::parse_str(&id)).transpose()?,snippet})}).collect::<Result<Vec<_>>>()?;drop(statement);
            let has_more=items.len()>limit;items.truncate(limit);let before=if has_more{items.last().map(|item|ConversationCursor{updated_at:item.conversation.updated_at,id:item.conversation.id})}else{None};
            tx.commit()?;Ok(HistorySearchPage{query,items,before,has_more,indexing})
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
            ensure_initialized(conn)?;
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
            ensure_initialized(conn)?;
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
mod tests {
    use super::*;
    use workbench_core::now_ms;
    fn setup() -> (tempfile::TempDir, Store, Uuid) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("history.db")).unwrap();
        let id = Uuid::new_v4();
        store
            .upsert_conversation(&Conversation {
                id,
                title: "Original title".into(),
                workspace: directory.path().into(),
                updated_at: 7,
            })
            .unwrap();
        (directory, store, id)
    }
    fn message(store: &Store, session: Uuid, text: &str) -> Uuid {
        let id = Uuid::new_v4();
        store
            .append_message(&ChatMessage {
                attachments: Vec::new(),
                id,
                session_id: session,
                role: "user".into(),
                content: text.into(),
                created_at: 7,
            })
            .unwrap();
        id
    }
    fn indexed(store: &Store) {
        for _ in 0..100 {
            if store.index_history_chunk(2).unwrap() {
                return;
            }
        }
        panic!("Backfill did not terminate");
    }
    #[test]
    fn lazy_backfill_and_triggers_search_all_history_with_source_ids_and_bounded_snippets() {
        let (_directory, store, session) = setup();
        let first = message(
            &store,
            session,
            "Older needlebody outside the visible sidebar.",
        );
        let pending = store.search_history("needlebody", None, 20).unwrap();
        assert!(pending.indexing);
        assert!(pending.items.is_empty());
        assert!(!store.index_history_chunk(1).unwrap());
        indexed(&store);
        let found = store.search_history("needlebody", None, 20).unwrap();
        assert!(!found.indexing);
        assert_eq!(found.items[0].message_id, Some(first));
        assert!(found.items[0].snippet.contains("needlebody"));
        let second = message(
            &store,
            session,
            "New needlebody captured by an insert trigger.",
        );
        assert_eq!(
            store.search_history("needlebody", None, 20).unwrap().items[0].message_id,
            Some(second)
        );
        store.call(move|conn|{conn.execute("UPDATE messages SET json=json_set(json,'$.content','replacementword') WHERE id=?1",[second.to_string()])?;Ok(())}).unwrap();
        assert_eq!(
            store
                .search_history("replacementword", None, 20)
                .unwrap()
                .items[0]
                .message_id,
            Some(second)
        );
        store
            .call(move |conn| {
                conn.execute("DELETE FROM messages WHERE id=?1", [second.to_string()])?;
                Ok(())
            })
            .unwrap();
        assert!(
            store
                .search_history("replacementword", None, 20)
                .unwrap()
                .items
                .is_empty()
        );
        store
            .upsert_conversation(&Conversation {
                id: session,
                title: "Renamedtitle".into(),
                workspace: "/fixture".into(),
                updated_at: now_ms(),
            })
            .unwrap();
        let titles = store.search_history("Renamedtitle", None, 20).unwrap();
        assert_eq!(titles.items[0].message_id, None);
        assert!(
            store
                .search_history("Original", None, 20)
                .unwrap()
                .items
                .is_empty()
        );
    }
    #[test]
    fn tied_keyset_pages_return_each_conversation_once_and_escape_fts_operators() {
        let (_directory, store, _session) = setup();
        for id in 1..=5 {
            let session = Uuid::from_u128(id);
            store
                .upsert_conversation(&Conversation {
                    id: session,
                    title: "Searchable sharedtitle".into(),
                    workspace: "/fixture".into(),
                    updated_at: 9,
                })
                .unwrap();
            message(&store, session, "Sharedtitle repeated first");
            message(&store, session, "Sharedtitle repeated second");
        }
        indexed(&store);
        let mut cursor = None;
        let mut ids = Vec::new();
        loop {
            let page = store.search_history("sharedtitle", cursor, 2).unwrap();
            ids.extend(page.items.iter().map(|item| item.conversation.id));
            if !page.has_more {
                break;
            }
            cursor = page.before;
        }
        assert_eq!(
            ids,
            vec![5, 4, 3, 2, 1]
                .into_iter()
                .map(Uuid::from_u128)
                .collect::<Vec<_>>()
        );
        assert!(store.search_history("\" OR * NOT () : ^", None, 10).is_ok());
        assert!(store.search_history(&"x".repeat(513), None, 10).is_err());
    }
    #[test]
    fn message_jump_and_later_pages_preserve_append_order_without_gaps_or_cross_session_anchors() {
        let (_directory, store, session) = setup();
        let mut ids = Vec::new();
        for index in 0..10 {
            ids.push(message(&store, session, &index.to_string()));
        }
        let window = store.history_window(session, ids[4], 5).unwrap();
        assert_eq!(
            window
                .messages
                .iter()
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>(),
            ["2", "3", "4", "5", "6"]
        );
        assert!(window.has_older && window.has_newer);
        assert_eq!(window.anchor, Some(ids[4]));
        let later = store
            .history_after(session, window.after.unwrap(), 2)
            .unwrap();
        assert!(later.append);
        assert_eq!(
            later
                .messages
                .iter()
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>(),
            ["7", "8"]
        );
        assert!(later.has_newer);
        let last = store
            .history_after(session, later.after.unwrap(), 200)
            .unwrap();
        assert_eq!(last.messages[0].content, "9");
        assert!(!last.has_newer);
        assert!(store.history_window(Uuid::new_v4(), ids[4], 5).is_err());
        assert_eq!(
            store
                .history_window(session, ids[0], 1)
                .unwrap()
                .messages
                .len(),
            1
        );
        let id = message(&store, session, "new arrival");
        let arrival = store
            .history_after(session, last.after.unwrap(), 10)
            .unwrap();
        assert_eq!(arrival.messages[0].id, id);
    }
    #[test]
    fn interrupted_backfill_resumes_and_new_writes_remain_indexed_after_reopen() {
        let (directory, store, session) = setup();
        for _ in 0..5 {
            message(&store, session, "Persistentneedle");
        }
        assert!(!store.index_history_chunk(1).unwrap());
        drop(store);
        let store = Store::open(directory.path().join("history.db")).unwrap();
        message(&store, session, "Freshneedle during backfill");
        indexed(&store);
        assert_eq!(
            store
                .search_history("Persistentneedle", None, 10)
                .unwrap()
                .items
                .len(),
            1
        );
        assert_eq!(
            store
                .search_history("Freshneedle", None, 10)
                .unwrap()
                .items
                .len(),
            1
        );
    }
}
