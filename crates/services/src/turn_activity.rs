//! Live activity detail with compact summaries stored in chat messages.
use crate::{ActiveAction, Store};
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
pub(crate) fn hydrate_activity(
    connection: &rusqlite::Connection,
    message: &mut xylo_core::ChatMessage,
) -> Result<()> {
    use rusqlite::OptionalExtension;
    if message.role == "assistant" && message.turn_id.is_none() {
        let turn: Option<String> = connection.query_row(
            "SELECT turn_id FROM completed_turns WHERE session_id=?1 AND json_extract(json,'$.reply_id')=?2 LIMIT 1",
            params![message.session_id.to_string(), message.id.to_string()], |row| row.get(0),
        ).optional()?;
        message.turn_id = turn.map(|turn| Uuid::parse_str(&turn)).transpose()?;
    }
    Ok(())
}
pub(crate) fn summarize_activity(actions: &[ActiveAction], message: &mut xylo_core::ChatMessage) {
    use std::collections::HashSet;
    let mut read = HashSet::new();
    let mut edited = HashSet::new();
    let (mut ran, mut other) = (0, 0);
    message.activity_count = 0;
    for action in actions.iter().filter(|a| {
        a.intent.session_id == message.session_id && Some(a.intent.turn_id) == message.turn_id
    }) {
        message.activity_count += 1;
        let path = action
            .intent
            .arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| action.intent.id.to_string());
        match (action.intent.name.as_str(), action.state.as_str()) {
            ("file_read", "succeeded") => {
                read.insert(path);
            }
            ("file_edit" | "file_write" | "notebook_edit", "succeeded") => {
                edited.insert(path);
            }
            ("run_command" | "shell_command", state)
                if state == "succeeded"
                    || (state == "failed"
                        && action.result.as_ref().is_some_and(|r| {
                            r.get("exit_code").is_some()
                                || r.pointer("/summary/exit_code").is_some()
                        })) =>
            {
                ran += 1
            }
            _ => other += 1,
        }
    }
    let parts: Vec<_> = [
        (read.len(), "Read", "file"),
        (ran, "Ran", "command"),
        (edited.len(), "Edited", "file"),
        (other, "Recorded", "action"),
    ]
    .into_iter()
    .filter(|(n, _, _)| *n > 0)
    .map(|(n, v, o)| format!("{v} {n} {o}{}", if n == 1 { "" } else { "s" }))
    .collect();
    message.activity_summary = (!parts.is_empty()).then(|| parts.join(" · "));
}
impl Store {
    pub fn subagent_actions(&self, session: Uuid) -> Result<Vec<ActiveAction>> {
        Ok(self
            .active_actions
            .lock()
            .map_err(|_| anyhow::anyhow!("Action tracking unavailable"))?
            .iter()
            .rev()
            .filter(|a| a.intent.session_id == session && a.intent.name == "agent_delegate")
            .take(64)
            .cloned()
            .collect())
    }
    pub fn turn_activity_page(
        &self,
        session_id: Uuid,
        turn_id: Uuid,
        after: Option<i64>,
        limit: usize,
    ) -> Result<TurnActivityPage> {
        let limit = limit.clamp(1, 50);
        let held = self
            .active_actions
            .lock()
            .map_err(|_| anyhow::anyhow!("Action tracking unavailable"))?;
        let mut items: Vec<_> = held
            .iter()
            .filter(|a| {
                a.intent.session_id == session_id
                    && a.intent.turn_id == turn_id
                    && a.cursor > after.unwrap_or(0)
            })
            .take(limit + 1)
            .map(|a| TurnActivityItem {
                id: a.intent.id,
                name: a.intent.name.clone(),
                state: a.state.clone(),
                error: a.error.as_ref().map(|e| e.chars().take(512).collect()),
                cursor: a.cursor,
            })
            .collect();
        let more = items.len() > limit;
        items.truncate(limit);
        let next = more.then(|| items.last().unwrap().cursor);
        Ok(TurnActivityPage {
            session_id,
            turn_id,
            items,
            next,
        })
    }
    pub fn turn_action(
        &self,
        session: Uuid,
        turn: Uuid,
        action: Uuid,
    ) -> Result<Option<ActiveAction>> {
        let action = self.action(action)?;
        ensure!(
            action
                .as_ref()
                .is_none_or(|a| a.intent.session_id == session && a.intent.turn_id == turn),
            "This activity belongs to another chat or task"
        );
        Ok(action)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ActionIntent;
    use xylo_core::*;
    #[test]
    fn live_subagent_results_stay_in_their_chat_and_disappear_on_reopen() {
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
        let results = store.subagent_actions(session).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(Some(results[0].intent.id), saved);
        assert_eq!(
            results[0].result.as_ref().unwrap()["response"],
            "A saved report"
        );
        drop(store);
        let reopened = Store::open(&path).unwrap();
        assert!(reopened.subagent_actions(session).unwrap().is_empty());
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
        let page = store.turn_activity_page(session, first, None, 50).unwrap();
        assert_eq!(page.items.len(), 50);
        let rest = store
            .turn_activity_page(session, first, page.next, 50)
            .unwrap();
        assert_eq!(rest.items.len(), 5);
        assert!(
            store
                .turn_action(session, second, page.items[0].id)
                .is_err()
        );
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
        // Legacy ownership can be hydrated; missing activity details cannot be reconstructed.
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
        assert_eq!(old.activity_count, 0);
        assert!(old.activity_summary.is_none());
        assert!(
            messages
                .iter()
                .any(|message| message.turn_id == Some(second)
                    && message.status == MessageStatus::Interrupted)
        );
        assert!(
            store
                .turn_activity_page(session, first, None, 1000)
                .unwrap()
                .items
                .is_empty()
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
