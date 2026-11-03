//! Settings-triggered execution tracking lives only for the current Store.
use crate::Store;
use anyhow::{Result, anyhow, bail, ensure};
use serde_json::{Value, json};
use uuid::Uuid;
impl Store {
    pub fn record_extension_action_binding(&self, id: Uuid, binding: &Value) -> Result<()> {
        let action = self
            .action(id)?
            .ok_or_else(|| anyhow!("Binding has no live action"))?;
        ensure!(
            !action.imported,
            "Imported activity cannot bind an execution"
        );
        if crate::live_actions::serialized_size(binding)? > 16 * 1024 {
            self.mark_live_action_incomplete(action.intent.session_id, action.intent.turn_id)?;
            return Ok(());
        }
        let mut bindings = self
            .extension_bindings
            .lock()
            .map_err(|_| anyhow!("Extension tracking unavailable"))?;
        if let Some(old) = bindings.get(&id) {
            ensure!(old == binding, "Extension action binding cannot change");
        }
        ensure!(
            bindings.len() < 1000 || bindings.contains_key(&id),
            "Too many live extension bindings"
        );
        bindings.insert(id, binding.clone());
        let _ = self
            .live_action_updates
            .send(crate::LiveActionUpdate::Binding {
                session_id: action.intent.session_id,
                turn_id: action.intent.turn_id,
                id,
                binding: binding.clone(),
            });
        Ok(())
    }
    pub fn extension_action_binding(&self, id: Uuid) -> Result<Option<Value>> {
        Ok(self
            .extension_bindings
            .lock()
            .map_err(|_| anyhow!("Extension tracking unavailable"))?
            .get(&id)
            .cloned())
    }
    pub fn begin_extension_execution(&self, id: Uuid, intent: &Value) -> Result<Option<Value>> {
        let oversized = crate::live_actions::serialized_size(intent)? > 16 * 1024;
        let intent_fingerprint = oversized
            .then(|| crate::live_actions::fingerprint(intent))
            .transpose()?;
        let mut executions = self
            .extension_executions
            .lock()
            .map_err(|_| anyhow!("Extension tracking unavailable"))?;
        if let Some(old) = executions.iter().find(|r| r["id"] == id.to_string()) {
            ensure!(
                old.get("intent_fingerprint")
                    .and_then(Value::as_str)
                    .map_or_else(
                        || &old["intent"] == intent,
                        |expected| crate::live_actions::fingerprint(intent)
                            .is_ok_and(|digest| digest == expected)
                    ),
                "Extension attempt ID reused with different input"
            );
            ensure!(
                old["evidence_complete"] != false,
                "Extension result exceeded the live budget and cannot be reused"
            );
            return match old["state"].as_str() {
                Some("succeeded") => Ok(Some(old["result"].clone())),
                Some("running") => bail!("Extension attempt is already running"),
                _ => bail!(
                    "Extension attempt did not finish successfully; use a new explicit attempt"
                ),
            };
        }
        ensure!(executions.len() < 1000, "Too many live extension attempts");
        executions.push(json!({"id":id,"intent":if oversized { &Value::Null } else { intent },"intent_fingerprint":intent_fingerprint,"state":"running","result":null,"error":null,"updated_at":xylo_core::now_ms()}));
        Ok(None)
    }
    pub fn finish_extension_execution(&self, id: Uuid, result: &Value) -> Result<()> {
        let mut executions = self
            .extension_executions
            .lock()
            .map_err(|_| anyhow!("Extension tracking unavailable"))?;
        let execution = executions
            .iter_mut()
            .find(|r| r["id"] == id.to_string() && r["state"] == "running")
            .ok_or_else(|| anyhow!("Extension attempt is no longer running"))?;
        execution["state"] = json!("succeeded");
        let complete = serde_json::to_vec(result)?.len() <= 16 * 1024;
        execution["result"] = if complete {
            result.clone()
        } else {
            Value::Null
        };
        execution["evidence_complete"] = json!(complete);
        execution["updated_at"] = json!(xylo_core::now_ms());
        Ok(())
    }
    pub fn mark_extension_execution_unknown(&self, id: Uuid, message: &str) -> Result<()> {
        let mut executions = self
            .extension_executions
            .lock()
            .map_err(|_| anyhow!("Extension tracking unavailable"))?;
        if let Some(execution) = executions
            .iter_mut()
            .find(|r| r["id"] == id.to_string() && r["state"] == "running")
        {
            execution["state"] = json!("outcome_unknown");
            execution["error"] = json!(message.chars().take(2048).collect::<String>());
            execution["updated_at"] = json!(xylo_core::now_ms());
        }
        Ok(())
    }
    pub fn extension_execution_receipts(&self, limit: usize) -> Result<Vec<Value>> {
        Ok(self
            .extension_executions
            .lock()
            .map_err(|_| anyhow!("Extension tracking unavailable"))?
            .iter()
            .rev()
            .take(limit.min(200))
            .cloned()
            .collect())
    }
}
#[cfg(test)]
mod tests;
