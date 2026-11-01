//! Activity queries use the existing journal, off the UI thread.
use super::*;
impl HostClient {
    pub fn subagent_activity(&self, id: Uuid, session: Uuid) -> Result<()> {
        self.enqueue(HostCommand::SubagentActivity(id, session))
    }
    pub fn turn_activity(
        &self,
        id: Uuid,
        session: Uuid,
        turn: Uuid,
        after: Option<i64>,
    ) -> Result<()> {
        self.enqueue(HostCommand::TurnActivity(id, session, turn, after))
    }
    pub fn turn_action(&self, id: Uuid, session: Uuid, turn: Uuid, action: Uuid) -> Result<()> {
        self.enqueue(HostCommand::TurnAction(id, session, turn, action))
    }
}
impl HostState {
    pub(super) async fn subagent_activity(&self, id: Uuid, session: Uuid) -> Result<()> {
        let store = self.store.clone();
        let result = tokio::task::spawn_blocking(move || store.subagent_actions(session))
            .await?
            .map_err(|error| error.to_string());
        self.events
            .send(HostEvent::SubagentActivity {
                request_id: id,
                session,
                result,
            })
            .await?;
        Ok(())
    }
    pub(super) async fn turn_activity(
        &self,
        id: Uuid,
        session: Uuid,
        turn: Uuid,
        after: Option<i64>,
    ) -> Result<()> {
        let store = self.store.clone();
        let result =
            tokio::task::spawn_blocking(move || store.turn_activity_page(session, turn, after, 50))
                .await?
                .map_err(|error| error.to_string());
        self.events
            .send(HostEvent::TurnActivity {
                request_id: id,
                session,
                turn,
                result,
            })
            .await?;
        Ok(())
    }
    pub(super) async fn turn_action(
        &self,
        id: Uuid,
        session: Uuid,
        turn: Uuid,
        action: Uuid,
    ) -> Result<()> {
        let store = self.store.clone();
        let artifacts = self.paths.artifacts.clone();
        let result = tokio::task::spawn_blocking(move || {
            let mut record = store.turn_action(session, turn, action)?;
            if let Some(record) = record.as_mut() {
                resolve_command_origin(record, &artifacts, session)?;
            }
            anyhow::Ok(record)
        })
        .await?
        .map_err(|error| error.to_string());
        self.events
            .send(HostEvent::TurnAction {
                request_id: id,
                session,
                turn,
                action,
                result,
            })
            .await?;
        Ok(())
    }
}

/// Old journal entries lack job_turn_id. Resolve it from retained app-owned
/// metadata only after the caller's session, turn and action were validated.
fn resolve_command_origin(
    record: &mut xylo_services::JournalAction,
    artifacts: &std::path::Path,
    session: Uuid,
) -> Result<()> {
    if !matches!(
        record.intent.name.as_str(),
        "shell_command" | "run_command" | "shell_output"
    ) {
        return Ok(());
    }
    if let Some(result) = record.result.as_mut() {
        let summary = if result.get("summary").is_some() {
            &mut result["summary"]
        } else {
            result
        };
        if let Some(job) = summary
            .get("job_id")
            .and_then(serde_json::Value::as_str)
            .and_then(|id| Uuid::parse_str(id).ok())
        {
            let (_, origin) = super::output_control::job_identity(artifacts, session, job)?;
            summary["job_turn_id"] = serde_json::json!(origin);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn saved_inspection_resolves_original_job_turn_without_cross_chat_access() {
        let directory = tempfile::tempdir().unwrap();
        let session = Uuid::new_v4();
        let origin = Uuid::new_v4();
        let later = Uuid::new_v4();
        let job = Uuid::new_v4();
        let store = xylo_services::Store::open(directory.path().join("test.db")).unwrap();
        store
            .upsert_conversation(&xylo_core::Conversation {
                id: session,
                title: "Background output".into(),
                workspace: directory.path().into(),
                updated_at: 1,
                title_source: Default::default(),
                pinned: false,
                archived: false,
            })
            .unwrap();
        let action = xylo_services::ActionIntent {
            id: Uuid::new_v4(),
            session_id: session,
            turn_id: later,
            name: "shell_output".into(),
            arguments: serde_json::json!({"job_id":job}),
            writes: false,
            idempotency_key: "later-inspection".into(),
        };
        store.begin_action(&action).unwrap();
        store.mark_action_running(action.id).unwrap();
        store
            .finish_action(
                action.id,
                &serde_json::json!({"summary":{"job_id":job,"output":"old tail"}}),
            )
            .unwrap();
        let logs = directory
            .path()
            .join("shell-jobs")
            .join(session.to_string());
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(
            logs.join(format!("{job}.json")),
            serde_json::to_vec(&(session, origin)).unwrap(),
        )
        .unwrap();
        std::fs::write(logs.join(format!("{job}.log")), "complete original output").unwrap();
        let mut record = store
            .turn_action(session, later, action.id)
            .unwrap()
            .unwrap();
        resolve_command_origin(&mut record, directory.path(), session).unwrap();
        let resolved: Uuid =
            serde_json::from_value(record.result.unwrap()["summary"]["job_turn_id"].clone())
                .unwrap();
        assert_eq!(resolved, origin);
        let page =
            super::super::output_control::read_page(directory.path(), session, resolved, job, 0)
                .unwrap();
        assert_eq!(page.text, "complete original output");
        assert!(
            super::super::output_control::read_page(directory.path(), session, later, job, 0)
                .is_err()
        );
        assert!(store.turn_action(Uuid::new_v4(), later, action.id).is_err());
        assert!(
            super::super::output_control::read_page(
                directory.path(),
                Uuid::new_v4(),
                resolved,
                job,
                0
            )
            .is_err()
        );
    }
}
