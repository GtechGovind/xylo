//! Lazy metadata from the existing action journal; no schema or output duplication.
use crate::{JournalAction, Store};
use anyhow::{Result, ensure};
use rusqlite::params;
use uuid::Uuid;
#[derive(Clone, Debug)]
pub struct TurnActivityItem {
    pub id: Uuid,
    pub name: String,
    pub state: String,
    pub error: Option<String>,
    pub cursor: i64,
}
#[derive(Clone, Debug)]
pub struct TurnActivityPage {
    pub session_id: Uuid,
    pub turn_id: Uuid,
    pub items: Vec<TurnActivityItem>,
    pub next: Option<i64>,
}
/// Hydrate older JSON without rewriting history or guessing from answer text.
/// A legacy reply is associated only through its durable completion receipt.
pub(crate) fn hydrate_activity(
    connection: &rusqlite::Connection,
    message: &mut xylo_core::ChatMessage,
) -> Result<()> {
    use rusqlite::OptionalExtension;
    if message.role != "assistant" {
        return Ok(());
    }
    if message.turn_id.is_none() {
        let turn: Option<String> = connection.query_row(
            "SELECT turn_id FROM completed_turns WHERE session_id=?1 AND json_extract(json,'$.reply_id')=?2 LIMIT 1",
            params![message.session_id.to_string(), message.id.to_string()], |row| row.get(0),
        ).optional()?;
        message.turn_id = turn.map(|turn| Uuid::parse_str(&turn)).transpose()?;
    }
    let Some(turn) = message.turn_id else {
        return Ok(());
    };
    let mut statement = connection.prepare(
        "SELECT CASE
            WHEN state='succeeded' AND json_extract(intent,'$.name')='file_read' THEN 'read'
            WHEN (state='succeeded' OR (state='failed' AND (json_extract(result,'$.exit_code') IS NOT NULL OR json_extract(result,'$.summary.exit_code') IS NOT NULL))) AND json_extract(intent,'$.name') IN ('run_command','shell_command') THEN 'run'
            WHEN state='succeeded' AND json_extract(intent,'$.name') IN ('file_edit','file_write','notebook_edit') THEN 'edit'
            ELSE 'other' END AS kind,
            COUNT(*),COUNT(DISTINCT COALESCE(json_extract(intent,'$.arguments.path'),id))
            FROM actions WHERE session_id=?1 AND turn_id=?2 GROUP BY kind",
    )?;
    let groups = statement
        .query_map(
            params![message.session_id.to_string(), turn.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u32>(1)?,
                    row.get::<_, u32>(2)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    message.activity_count = groups.iter().map(|(_, count, _)| count).copied().sum();
    let (mut read, mut ran, mut edited, mut other) = (0, 0, 0, 0);
    for (name, count, files) in groups {
        match name.as_str() {
            "read" => read += files,
            "run" => ran += count,
            "edit" => edited += files,
            _ => other += count,
        }
    }
    let mut parts = Vec::new();
    for (count, verb, noun) in [
        (read, "Read", "file"),
        (ran, "Ran", "command"),
        (edited, "Edited", "file"),
        (other, "Recorded", "action"),
    ] {
        if count > 0 {
            parts.push(format!(
                "{verb} {count} {noun}{}",
                if count == 1 { "" } else { "s" }
            ));
        }
    }
    message.activity_summary = (!parts.is_empty()).then(|| parts.join(" · "));
    Ok(())
}
impl Store {
    /// Recent helper results from the existing journal, scoped to one chat.
    pub fn subagent_actions(&self, session: Uuid) -> Result<Vec<JournalAction>> {
        self.call(move |connection| {
            let mut query = connection.prepare("SELECT intent,state,result,error,updated_at FROM actions WHERE session_id=?1 AND json_extract(intent,'$.name')='agent_delegate' ORDER BY updated_at DESC,rowid DESC LIMIT 64")?;
            let rows = query.query_map([session.to_string()], |row| Ok((
                row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?, row.get::<_, i64>(4)?,
            )))?;
            rows.map(|row| {
                let (intent, state, result, error, updated_at) = row?;
                Ok(JournalAction { intent: serde_json::from_str(&intent)?, state,
                    result: result.map(|json| serde_json::from_str(&json)).transpose()?, error, updated_at })
            }).collect()
        })
    }
    pub fn turn_activity_page(
        &self,
        session_id: Uuid,
        turn_id: Uuid,
        after: Option<i64>,
        limit: usize,
    ) -> Result<TurnActivityPage> {
        self.call(move |connection| {
            let limit = limit.clamp(1, 50);
            let mut query = connection.prepare("SELECT rowid,id,json_extract(intent,'$.name'),state,substr(error,1,512) FROM actions WHERE session_id=?1 AND turn_id=?2 AND rowid>?3 ORDER BY rowid LIMIT ?4")?;
            let rows = query.query_map(params![session_id.to_string(), turn_id.to_string(), after.unwrap_or(0), limit as i64 + 1], |row| Ok((row.get::<_,i64>(0)?, row.get::<_,String>(1)?, row.get::<_,String>(2)?, row.get::<_,String>(3)?, row.get::<_,Option<String>>(4)?)))?;
            let mut items = Vec::new();
            for row in rows { let (cursor,id,name,state,error) = row?; items.push(TurnActivityItem { id: Uuid::parse_str(&id)?, name, state, error, cursor }); }
            let more = items.len() > limit;
            items.truncate(limit);
            let next = more.then(|| items.last().unwrap().cursor);
            Ok(TurnActivityPage { session_id, turn_id, items, next })
        })
    }
    pub fn turn_action(
        &self,
        session: Uuid,
        turn: Uuid,
        action: Uuid,
    ) -> Result<Option<JournalAction>> {
        let owned = self.call(move |connection| {
            Ok(connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM actions WHERE id=?1 AND session_id=?2 AND turn_id=?3)",
                params![action.to_string(), session.to_string(), turn.to_string()],
                |row| row.get::<_, bool>(0),
            )?)
        })?;
        ensure!(owned, "This activity belongs to another chat or task");
        self.action(action)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ActionIntent;
    use xylo_core::*;
    #[test]
    fn saved_subagent_results_survive_reopen_and_stay_in_their_chat() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("agents.db");
        let store = Store::open(&path).unwrap();
        let session = Uuid::new_v4();
        let other = Uuid::new_v4();
        let mut saved = None;
        for (chat, tool) in [
            (session, "agent_delegate"),
            (session, "file_read"),
            (other, "agent_delegate"),
        ] {
            store
                .upsert_conversation(&Conversation {
                    id: chat,
                    title: "Review".into(),
                    workspace: temp.path().into(),
                    updated_at: 1,
                    title_source: Default::default(),
                    pinned: false,
                    archived: false,
                })
                .unwrap();
            let action = ActionIntent {
                id: Uuid::new_v4(),
                session_id: chat,
                turn_id: Uuid::new_v4(),
                name: tool.into(),
                arguments: serde_json::json!({"task":"Review layout"}),
                writes: false,
                idempotency_key: tool.into(),
            };
            store.begin_action(&action).unwrap();
            store.mark_action_running(action.id).unwrap();
            store
                .finish_action(action.id, &serde_json::json!({"response":"A saved report"}))
                .unwrap();
            if chat == session && tool == "agent_delegate" {
                saved = Some(action.id);
            }
        }
        drop(store);
        let reopened = Store::open(&path).unwrap();
        let results = reopened.subagent_actions(session).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(Some(results[0].intent.id), saved);
        assert_eq!(
            results[0].result.as_ref().unwrap()["response"],
            "A saved report"
        );
        assert!(
            reopened
                .subagent_actions(Uuid::new_v4())
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn failed_commands_require_execution_evidence_in_saved_summary() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("test.db")).unwrap();
        let session = Uuid::new_v4();
        let turn = Uuid::new_v4();
        store
            .upsert_conversation(&Conversation {
                id: session,
                title: "Commands".into(),
                workspace: temp.path().into(),
                updated_at: 1,
                title_source: Default::default(),
                pinned: false,
                archived: false,
            })
            .unwrap();
        for (index, observed) in [false, true].into_iter().enumerate() {
            let action = ActionIntent {
                id: Uuid::new_v4(),
                session_id: session,
                turn_id: turn,
                name: "shell_command".into(),
                arguments: serde_json::json!({"command":"check"}),
                writes: true,
                idempotency_key: index.to_string(),
            };
            store.begin_action(&action).unwrap();
            store.mark_action_running(action.id).unwrap();
            if observed {
                store
                    .fail_action_with_result(
                        action.id,
                        &serde_json::json!({"summary":{"exit_code":1}}),
                        "Process failed",
                    )
                    .unwrap();
            } else {
                store
                    .fail_action(action.id, "Process could not start")
                    .unwrap();
            }
        }
        store
            .complete_turn(
                session,
                turn,
                &serde_json::json!([]),
                "Done",
                &Usage::default(),
            )
            .unwrap();
        let messages = store.messages(session, 100).unwrap();
        let reply = messages
            .iter()
            .find(|message| message.role == "assistant")
            .unwrap();
        assert_eq!(reply.activity_count, 2);
        assert_eq!(
            reply.activity_summary.as_deref(),
            Some("Ran 1 command · Recorded 1 action")
        );
    }
    #[test]
    fn activity_and_replies_survive_reopen_without_schema_changes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("test.db");
        let store = Store::open(&path).unwrap();
        let session = Uuid::new_v4();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        store
            .upsert_conversation(&Conversation {
                id: session,
                title: "History".into(),
                workspace: temp.path().into(),
                updated_at: 1,
                title_source: Default::default(),
                pinned: false,
                archived: false,
            })
            .unwrap();
        for n in 0..55 {
            let action = ActionIntent {
                id: Uuid::new_v4(),
                session_id: session,
                turn_id: first,
                name: "file_read".into(),
                arguments: serde_json::json!({"path":"a.rs","line":n}),
                writes: false,
                idempotency_key: format!("read-{n}"),
            };
            store.begin_action(&action).unwrap();
            store.mark_action_running(action.id).unwrap();
            store
                .finish_action(action.id, &serde_json::json!({"status":"success"}))
                .unwrap();
        }
        store
            .complete_turn(
                session,
                first,
                &serde_json::json!([]),
                "First answer",
                &Usage::default(),
            )
            .unwrap();
        let denied = ActionIntent {
            id: Uuid::new_v4(),
            session_id: session,
            turn_id: second,
            name: "file_edit".into(),
            arguments: serde_json::json!({"path":"not-written.rs"}),
            writes: true,
            idempotency_key: "denied-edit".into(),
        };
        store.begin_action(&denied).unwrap();
        store.deny_action(denied.id).unwrap();
        store
            .record_interrupted_reply(session, second, "", MessageStatus::Interrupted)
            .unwrap();
        let greeting = Uuid::new_v4();
        store
            .complete_turn(
                session,
                greeting,
                &serde_json::json!([]),
                "Hello",
                &Usage::default(),
            )
            .unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert!(store.reset_notice().is_none());
        let messages = store.messages(session, 100).unwrap();
        let hello = messages
            .iter()
            .find(|message| message.turn_id == Some(greeting))
            .unwrap();
        assert_eq!(hello.activity_count, 0);
        assert!(hello.activity_summary.is_none());
        let saved = messages
            .iter()
            .find(|message| message.turn_id == Some(first))
            .unwrap();
        assert_eq!(saved.content, "First answer");
        assert_eq!(saved.activity_count, 55);
        assert_eq!(saved.activity_summary.as_deref(), Some("Read 1 file"));
        let interrupted = messages
            .iter()
            .find(|message| message.turn_id == Some(second))
            .unwrap();
        assert_eq!(interrupted.activity_count, 1);
        assert_eq!(
            interrupted.activity_summary.as_deref(),
            Some("Recorded 1 action")
        );
        // Metadata is written with the answer, rather than only computed in UI.
        let raw = store
            .call(move |connection| {
                Ok(connection.query_row(
                    "SELECT json FROM messages WHERE id=?1",
                    [crate::store::assistant_reply_id(first).to_string()],
                    |row| row.get::<_, String>(0),
                )?)
            })
            .unwrap();
        let raw: ChatMessage = serde_json::from_str(&raw).unwrap();
        assert_eq!(raw.activity_count, 55);
        // Old JSON retains receipt-backed ownership and journal evidence.
        store.call(move |connection| {
            connection.execute("UPDATE messages SET json=json_remove(json,'$.turn_id','$.activity_count','$.activity_summary') WHERE id=?1", [crate::store::assistant_reply_id(first).to_string()])?;
            Ok(())
        }).unwrap();
        let hydrated = store.messages_page(session, None, 100).unwrap();
        let old = hydrated
            .messages
            .iter()
            .find(|message| message.id == saved.id)
            .unwrap();
        assert_eq!(old.turn_id, Some(first));
        assert_eq!(old.activity_count, 55);
        assert_eq!(old.activity_summary.as_deref(), Some("Read 1 file"));
        assert!(
            messages
                .iter()
                .any(|message| message.turn_id == Some(second)
                    && message.status == MessageStatus::Interrupted)
        );
        let page = store
            .turn_activity_page(session, first, None, 1000)
            .unwrap();
        assert_eq!(page.items.len(), 50);
        let rest = store
            .turn_activity_page(session, first, page.next, 50)
            .unwrap();
        assert_eq!(rest.items.len(), 5);
        assert!(rest.next.is_none());
        assert!(
            store
                .turn_action(session, second, page.items[0].id)
                .is_err()
        );
        assert!(
            store
                .turn_action(session, first, page.items[0].id)
                .unwrap()
                .is_some()
        );
        let mut legacy = serde_json::to_value(&messages[0]).unwrap();
        legacy.as_object_mut().unwrap().remove("turn_id");
        legacy.as_object_mut().unwrap().remove("activity_count");
        legacy.as_object_mut().unwrap().remove("activity_summary");
        let legacy = serde_json::from_value::<ChatMessage>(legacy).unwrap();
        assert!(legacy.turn_id.is_none());
        assert_eq!(legacy.activity_count, 0);
    }
}
