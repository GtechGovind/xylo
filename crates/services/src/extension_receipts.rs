//! Settings-triggered executions have durable receipts without creating fake chats.
use crate::Store;
use anyhow::{Result, bail, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use uuid::Uuid;
impl Store {
    pub fn record_extension_action_binding(&self, id: Uuid, binding: &Value) -> Result<()> {
        let binding = serde_json::to_string(binding)?;
        self.call(move |conn| {
            let old: Option<String> = conn
                .query_row(
                    "SELECT binding FROM extension_action_bindings WHERE action_id=?1",
                    [id.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(old) = old {
                ensure!(old == binding, "Extension action binding cannot change");
                return Ok(());
            }
            conn.execute(
                "INSERT INTO extension_action_bindings(action_id,binding) VALUES(?1,?2)",
                params![id.to_string(), binding],
            )?;
            Ok(())
        })
    }
    pub fn extension_action_binding(&self, id: Uuid) -> Result<Option<Value>> {
        self.call(move |conn| {
            let value: Option<String> = conn
                .query_row(
                    "SELECT binding FROM extension_action_bindings WHERE action_id=?1",
                    [id.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            value
                .map(|value| serde_json::from_str(&value).map_err(Into::into))
                .transpose()
        })
    }
    pub fn begin_extension_execution(&self, id: Uuid, intent: &Value) -> Result<Option<Value>> {
        let intent = serde_json::to_string(intent)?;
        self.call(move|conn|{let tx=conn.transaction()?;let old:Option<(String,String,Option<String>)>=tx.query_row("SELECT intent,state,result FROM extension_execution_receipts WHERE id=?1",[id.to_string()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional()?;
            if let Some((old,state,result))=old{ensure!(old==intent,"Extension attempt ID reused with different input");match state.as_str(){"succeeded"=>return Ok(Some(serde_json::from_str(&result.unwrap_or_else(||"null".into()))?)),"running"=>bail!("Extension attempt is already running"),"outcome_unknown"=>bail!("Extension attempt outcome is unknown; inspect its receipt before a new explicit attempt"),_=>bail!("Extension attempt already failed; use a new explicit attempt")}}
            tx.execute("INSERT INTO extension_execution_receipts(id,intent,state,updated_at) VALUES(?1,?2,'running',?3)",params![id.to_string(),intent,xylo_core::now_ms()])?;tx.commit()?;Ok(None)})
    }
    pub fn finish_extension_execution(&self, id: Uuid, result: &Value) -> Result<()> {
        let result = serde_json::to_string(result)?;
        self.call(move|conn|{let changed=conn.execute("UPDATE extension_execution_receipts SET state='succeeded',result=?1,error=NULL,updated_at=?2 WHERE id=?3 AND state='running'",params![result,xylo_core::now_ms(),id.to_string()])?;ensure!(changed==1,"Extension receipt is no longer running");Ok(())})
    }
    pub fn mark_extension_execution_unknown(&self, id: Uuid, message: &str) -> Result<()> {
        let message = message.chars().take(2048).collect::<String>();
        self.call(move|conn|{conn.execute("UPDATE extension_execution_receipts SET state='outcome_unknown',error=?1,updated_at=?2 WHERE id=?3 AND state='running'",params![message,xylo_core::now_ms(),id.to_string()])?;Ok(())})
    }
    pub fn extension_execution_receipts(&self, limit: usize) -> Result<Vec<Value>> {
        self.call(move|conn|{let mut statement=conn.prepare("SELECT id,intent,state,result,error,updated_at FROM extension_execution_receipts ORDER BY updated_at DESC,id DESC LIMIT ?1")?;let rows=statement.query_map([limit.min(200) as i64],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,Option<String>>(3)?,row.get::<_,Option<String>>(4)?,row.get::<_,i64>(5)?)))?;rows.map(|row|{let(id,intent,state,result,error,updated_at)=row?;Ok(json!({"id":id,"intent":serde_json::from_str::<Value>(&intent)?,"state":state,"result":result.map(|value|serde_json::from_str::<Value>(&value)).transpose()?,"error":error,"updated_at":updated_at}))}).collect()})
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn extension_receipts_prevent_replay_and_recover_unknown_effects() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("db")).unwrap();
        let id = Uuid::new_v4();
        let intent = json!({"package":"fixture","component":"hooks","digest":"version1","operation":"verify"});
        assert!(
            store
                .begin_extension_execution(id, &intent)
                .unwrap()
                .is_none()
        );
        assert!(store.begin_extension_execution(id, &intent).is_err());
        assert_eq!(store.recover_interrupted().unwrap(), 1);
        assert!(store.begin_extension_execution(id, &intent).is_err());
        assert_eq!(
            store.extension_execution_receipts(5).unwrap()[0]["state"],
            "outcome_unknown"
        );
        let next = Uuid::new_v4();
        store.begin_extension_execution(next, &intent).unwrap();
        store
            .finish_extension_execution(next, &json!({"executed":true}))
            .unwrap();
        assert_eq!(
            store.begin_extension_execution(next, &intent).unwrap(),
            Some(json!({"executed":true}))
        );
    }
}
