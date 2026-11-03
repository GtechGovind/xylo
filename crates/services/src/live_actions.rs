//! Bounded process-local observations, optionally forwarded over the private worker IPC.
use crate::{ActiveAction, Store};
use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub(crate) const MAX_INTENT_BYTES: usize = 64 * 1024;
pub(crate) const MAX_RESULT_BYTES: usize = 256 * 1024;
pub(crate) const MAX_LIVE_BYTES: usize = 32 * 1024 * 1024;

/// Count serialized input without allocating a second full copy of large files.
pub(crate) fn serialized_size(value: &impl Serialize) -> Result<usize> {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    serde_json::to_writer(&mut count, value)?;
    Ok(count.0)
}
pub(crate) fn fingerprint(value: &impl Serialize) -> Result<String> {
    use sha2::Digest;
    struct Hash(sha2::Sha256);
    impl std::io::Write for Hash {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut hash = Hash(sha2::Sha256::new());
    serde_json::to_writer(&mut hash, value)?;
    Ok(hash
        .0
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
impl ActiveAction {
    pub fn matches_arguments(&self, arguments: &Value) -> bool {
        self.arguments_fingerprint.as_ref().map_or_else(
            || &self.intent.arguments == arguments,
            |expected| fingerprint(arguments).is_ok_and(|actual| actual == *expected),
        )
    }
    pub(crate) fn matches_key(&self, key: &str) -> bool {
        self.key_fingerprint.as_ref().map_or_else(
            || self.intent.idempotency_key == key,
            |expected| fingerprint(&key).is_ok_and(|actual| actual == *expected),
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LiveActionUpdate {
    Action {
        action: Box<ActiveAction>,
    },
    Binding {
        session_id: Uuid,
        turn_id: Uuid,
        id: Uuid,
        binding: Value,
    },
    TurnComplete {
        session_id: Uuid,
        turn_id: Uuid,
    },
    Incomplete {
        session_id: Uuid,
        turn_id: Uuid,
    },
}
impl LiveActionUpdate {
    pub fn scope(&self) -> (Uuid, Uuid) {
        match self {
            Self::Action { action } => (action.intent.session_id, action.intent.turn_id),
            Self::Binding {
                session_id,
                turn_id,
                ..
            }
            | Self::TurnComplete {
                session_id,
                turn_id,
            }
            | Self::Incomplete {
                session_id,
                turn_id,
            } => (*session_id, *turn_id),
        }
    }
}
impl Store {
    pub fn subscribe_live_actions(&self) -> tokio::sync::broadcast::Receiver<LiveActionUpdate> {
        self.live_action_updates.subscribe()
    }
    pub fn mark_live_action_incomplete(&self, session_id: Uuid, turn_id: Uuid) -> Result<()> {
        self.remember_incomplete(session_id, turn_id)?;
        let _ = self.live_action_updates.send(LiveActionUpdate::Incomplete {
            session_id,
            turn_id,
        });
        Ok(())
    }
    fn remember_incomplete(&self, session_id: Uuid, turn_id: Uuid) -> Result<()> {
        self.incomplete_action_turns
            .lock()
            .map_err(|_| anyhow!("Action tracking unavailable"))?
            .insert((session_id, turn_id));
        self.forgotten_action_sessions
            .lock()
            .map_err(|_| anyhow!("Action tracking unavailable"))?
            .insert(session_id);
        Ok(())
    }
    pub fn live_action_evidence_complete(&self, session_id: Uuid, turn_id: Uuid) -> Result<bool> {
        Ok(!self
            .incomplete_action_turns
            .lock()
            .map_err(|_| anyhow!("Action tracking unavailable"))?
            .contains(&(session_id, turn_id)))
    }
    pub(crate) fn learning_actions(&self) -> Result<Vec<ActiveAction>> {
        let actions = self
            .active_actions
            .lock()
            .map_err(|_| anyhow!("Action tracking unavailable"))?;
        let completed = self
            .completed_import_turns
            .lock()
            .map_err(|_| anyhow!("Action tracking unavailable"))?;
        let incomplete = self
            .incomplete_action_turns
            .lock()
            .map_err(|_| anyhow!("Action tracking unavailable"))?;
        Ok(actions
            .iter()
            .filter(|a| {
                a.evidence_complete
                    && (!a.imported || completed.contains(&(a.intent.session_id, a.intent.turn_id)))
                    && !incomplete.contains(&(a.intent.session_id, a.intent.turn_id))
            })
            .cloned()
            .collect())
    }
    pub(crate) fn make_live_action_room(
        &self,
        actions: &mut Vec<ActiveAction>,
        session: Uuid,
        turn: Uuid,
        bytes: usize,
        publish: bool,
    ) -> Result<()> {
        while actions.len() >= 1000
            || actions.iter().map(|a| a.retained_bytes).sum::<usize>() + bytes > MAX_LIVE_BYTES
        {
            let index = actions
                .iter()
                .position(|a| {
                    (a.intent.session_id, a.intent.turn_id) != (session, turn)
                        && !matches!(a.state.as_str(), "pending" | "running")
                })
                .context(
                    "Live action capacity reached; finish this turn before starting more actions",
                )?;
            let removed = actions.remove(index);
            if publish {
                self.mark_live_action_incomplete(
                    removed.intent.session_id,
                    removed.intent.turn_id,
                )?;
            } else {
                self.remember_incomplete(removed.intent.session_id, removed.intent.turn_id)?;
            }
            self.extension_bindings
                .lock()
                .map_err(|_| anyhow!("Extension tracking unavailable"))?
                .remove(&removed.intent.id);
        }
        Ok(())
    }
    /// Cache an authenticated worker observation. These records can inform UI and
    /// learning, but cannot authorize execution, transitions, or duplicate-result reuse.
    /// Import never rebroadcasts and never writes SQLite.
    pub fn import_live_action(&self, update: LiveActionUpdate) -> Result<()> {
        match update {
            LiveActionUpdate::TurnComplete {
                session_id,
                turn_id,
            } => {
                let actions = self
                    .active_actions
                    .lock()
                    .map_err(|_| anyhow!("Action tracking unavailable"))?;
                if actions.iter().any(|a| {
                    a.intent.session_id == session_id
                        && a.intent.turn_id == turn_id
                        && matches!(a.state.as_str(), "pending" | "running")
                }) {
                    self.remember_incomplete(session_id, turn_id)?;
                }
                self.completed_import_turns
                    .lock()
                    .map_err(|_| anyhow!("Action tracking unavailable"))?
                    .insert((session_id, turn_id));
                Ok(())
            }
            LiveActionUpdate::Incomplete {
                session_id,
                turn_id,
            } => self.remember_incomplete(session_id, turn_id),
            LiveActionUpdate::Binding {
                session_id,
                turn_id,
                id,
                binding,
            } => {
                ensure!(
                    serde_json::to_vec(&binding)?.len() <= 16 * 1024,
                    "Extension binding exceeds the live budget"
                );
                let action = self.action(id)?.context("Binding has no live action")?;
                ensure!(
                    action.imported
                        && (action.intent.session_id, action.intent.turn_id)
                            == (session_id, turn_id),
                    "Binding scope does not match imported activity"
                );
                let mut bindings = self
                    .extension_bindings
                    .lock()
                    .map_err(|_| anyhow!("Extension tracking unavailable"))?;
                ensure!(
                    bindings.get(&id).is_none_or(|old| old == &binding),
                    "Extension binding cannot change"
                );
                bindings.insert(id, binding);
                Ok(())
            }
            LiveActionUpdate::Action { action } => {
                let mut action = *action;
                ensure!(
                    [&action.arguments_fingerprint, &action.key_fingerprint]
                        .into_iter()
                        .all(
                            |digest| digest.as_ref().is_none_or(|digest| digest.len() == 64
                                && digest.bytes().all(|b| b.is_ascii_hexdigit()))
                        ),
                    "Invalid activity fingerprint"
                );
                let session = action.intent.session_id;
                let turn = action.intent.turn_id;
                let intent_bytes = serde_json::to_vec(&action.intent)?.len();
                if intent_bytes > MAX_INTENT_BYTES {
                    self.remember_incomplete(session, turn)?;
                    anyhow::bail!("Activity input exceeds the live budget");
                }
                ensure!(
                    matches!(
                        action.state.as_str(),
                        "pending"
                            | "running"
                            | "succeeded"
                            | "failed"
                            | "denied"
                            | "outcome_unknown"
                    ),
                    "Invalid live action state"
                );
                let mut actions = self
                    .active_actions
                    .lock()
                    .map_err(|_| anyhow!("Action tracking unavailable"))?;
                let old = actions.iter().position(|a| a.intent.id == action.intent.id);
                if let Some(index) = old {
                    let previous = &actions[index];
                    ensure!(
                        previous.arguments_fingerprint == action.arguments_fingerprint
                            && previous.key_fingerprint == action.key_fingerprint,
                        "Imported activity fingerprint cannot change"
                    );
                    ensure!(
                        previous.imported
                            && serde_json::to_value(&previous.intent)?
                                == serde_json::to_value(&action.intent)?,
                        "Imported activity cannot replace local execution or change identity"
                    );
                    let rank = |state: &str| match state {
                        "pending" => 0,
                        "running" => 1,
                        _ => 2,
                    };
                    ensure!(
                        rank(&action.state) >= rank(&previous.state)
                            && (rank(&previous.state) != 2 || previous.state == action.state),
                        "Imported activity moved backwards"
                    );
                    action.cursor = previous.cursor;
                    action.evidence_complete &= previous.evidence_complete;
                } else {
                    ensure!(
                        !actions.iter().any(|a| a.intent.session_id == session
                            && a.intent.turn_id == turn
                            && a.intent.idempotency_key == action.intent.idempotency_key
                            && a.key_fingerprint == action.key_fingerprint),
                        "Imported activity key already belongs to another action"
                    );
                    if let Err(error) =
                        self.make_live_action_room(&mut actions, session, turn, intent_bytes, false)
                    {
                        self.remember_incomplete(session, turn)?;
                        return Err(error);
                    }
                    action.cursor = actions.last().map_or(1, |a| a.cursor + 1);
                }
                action.error = action.error.map(|e| e.chars().take(2048).collect());
                let result_bytes = action
                    .result
                    .as_ref()
                    .map(|r| serde_json::to_vec(r).map(|v| v.len()))
                    .transpose()?
                    .unwrap_or(0);
                let used = actions.iter().map(|a| a.retained_bytes).sum::<usize>()
                    - old.map_or(0, |i| actions[i].retained_bytes);
                if used + intent_bytes + action.error.as_ref().map_or(0, String::len)
                    > MAX_LIVE_BYTES
                {
                    action.error = None;
                }
                let base = intent_bytes + action.error.as_ref().map_or(0, String::len);
                if result_bytes > MAX_RESULT_BYTES || used + base + result_bytes > MAX_LIVE_BYTES {
                    action.result = None;
                    action.evidence_complete = false;
                }
                action.retained_bytes = base
                    + if action.result.is_some() {
                        result_bytes
                    } else {
                        0
                    };
                action.imported = true;
                if !action.evidence_complete {
                    self.remember_incomplete(session, turn)?;
                }
                if let Some(index) = old {
                    actions[index] = action;
                } else {
                    actions.push(action);
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ActionDecision, ActionIntent};
    use serde_json::json;
    fn action() -> ActionIntent {
        ActionIntent {
            id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            turn_id: Uuid::new_v4(),
            name: "run_command".into(),
            arguments: json!({"program":"cargo","args":["check"]}),
            writes: true,
            idempotency_key: "call-1".into(),
        }
    }
    #[test]
    fn forwarded_activity_is_observable_but_never_execution_authority() {
        let dir = tempfile::tempdir().unwrap();
        let worker = Store::open(dir.path().join("worker.db")).unwrap();
        let host = Store::open(dir.path().join("host.db")).unwrap();
        let mut updates = worker.subscribe_live_actions();
        let mut host_updates = host.subscribe_live_actions();
        let intent = action();
        worker.begin_action(&intent).unwrap();
        worker.mark_action_running(intent.id).unwrap();
        worker
            .record_extension_action_binding(intent.id, &json!({"digest":"known"}))
            .unwrap();
        worker
            .finish_action(intent.id, &json!({"status":"success"}))
            .unwrap();
        while let Ok(update) = updates.try_recv() {
            assert_eq!(update.scope(), (intent.session_id, intent.turn_id));
            host.import_live_action(update).unwrap();
        }
        assert!(
            host_updates.try_recv().is_err(),
            "Imports never rebroadcast"
        );
        assert_eq!(host.action(intent.id).unwrap().unwrap().state, "succeeded");
        assert!(host.begin_action(&intent).is_err());
        assert!(host.mark_action_running(intent.id).is_err());
        assert!(
            host.action_for_key(intent.session_id, intent.turn_id, &intent.idempotency_key)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            host.extension_action_binding(intent.id).unwrap().unwrap()["digest"],
            "known"
        );
        assert!(
            host.learning_actions().unwrap().is_empty(),
            "Partial transport is not evidence"
        );
        host.import_live_action(LiveActionUpdate::TurnComplete {
            session_id: intent.session_id,
            turn_id: intent.turn_id,
        })
        .unwrap();
        assert_eq!(host.learning_actions().unwrap().len(), 1);
        host.mark_live_action_incomplete(intent.session_id, intent.turn_id)
            .unwrap();
        assert!(host.learning_actions().unwrap().is_empty());
        assert_eq!(
            worker.begin_action(&intent).unwrap(),
            ActionDecision::Reuse(json!({"status":"success"}))
        );
        let reopened = Store::open(dir.path().join("host.db")).unwrap();
        assert!(reopened.action(intent.id).unwrap().is_none());
    }
    #[test]
    fn oversized_results_are_not_retained_or_used_as_complete_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("db")).unwrap();
        let intent = action();
        store.begin_action(&intent).unwrap();
        store.mark_action_running(intent.id).unwrap();
        store
            .finish_action(
                intent.id,
                &json!({"content":"x".repeat(MAX_RESULT_BYTES + 1)}),
            )
            .unwrap();
        let saved = store.action(intent.id).unwrap().unwrap();
        assert!(saved.result.is_none());
        assert!(!saved.evidence_complete);
        assert!(
            !store
                .live_action_evidence_complete(intent.session_id, intent.turn_id)
                .unwrap()
        );
        assert!(store.learning_actions().unwrap().is_empty());
        assert!(
            store.begin_action(&intent).is_err(),
            "A missing output must not be replayed or misreported"
        );
    }
    #[test]
    fn evicted_failed_step_invalidates_its_whole_turn_for_learning() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("db")).unwrap();
        let first = action();
        for index in 0..1000 {
            let mut next = first.clone();
            next.id = Uuid::new_v4();
            next.idempotency_key = index.to_string();
            store.begin_action(&next).unwrap();
            store.mark_action_running(next.id).unwrap();
            if index == 0 {
                store.fail_action(next.id, "failed").unwrap();
            } else {
                store
                    .finish_action(next.id, &json!({"status":"success"}))
                    .unwrap();
            }
        }
        let mut next = action();
        next.session_id = first.session_id;
        store.begin_action(&next).unwrap();
        assert!(
            !store
                .live_action_evidence_complete(first.session_id, first.turn_id)
                .unwrap()
        );
        assert!(
            store
                .learning_actions()
                .unwrap()
                .iter()
                .all(|a| a.intent.turn_id != first.turn_id)
        );
    }
    #[test]
    fn oversized_file_write_and_binding_execute_once_without_retaining_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("large.db")).unwrap();
        let mut intent = action();
        intent.name = "file_write".into();
        intent.arguments = json!({"path":"large.txt","content":"z".repeat(MAX_INTENT_BYTES * 2)});
        assert_eq!(
            store.begin_action(&intent).unwrap(),
            ActionDecision::Execute
        );
        let saved = store.action(intent.id).unwrap().unwrap();
        assert!(saved.intent.arguments.is_null());
        assert!(saved.matches_arguments(&intent.arguments));
        assert!(!saved.matches_arguments(&json!({"path":"large.txt","content":"changed"})));
        assert!(!saved.evidence_complete);
        assert!(serialized_size(&saved).unwrap() < 4096);
        assert_eq!(
            store.begin_action(&intent).unwrap(),
            ActionDecision::InFlight
        );
        store
            .record_extension_action_binding(intent.id, &json!({"snapshot":"q".repeat(32 * 1024)}))
            .unwrap();
        assert!(store.extension_action_binding(intent.id).unwrap().is_none());
        store.mark_action_running(intent.id).unwrap();
        store
            .finish_action(intent.id, &json!({"written":true}))
            .unwrap();
        assert_eq!(
            store.begin_action(&intent).unwrap(),
            ActionDecision::Reuse(json!({"written":true}))
        );
        let mut changed = intent.clone();
        changed.arguments["content"] = json!("different");
        assert!(store.begin_action(&changed).is_err());
        assert!(
            store
                .action_for_key(intent.session_id, intent.turn_id, &intent.idempotency_key)
                .unwrap()
                .is_some()
        );
        assert!(store.learning_actions().unwrap().is_empty());
    }
}
