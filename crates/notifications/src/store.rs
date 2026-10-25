//! Lazy bounded notification metadata. No chat text, file paths or secrets.
use crate::*;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub(crate) struct Store {
    path: PathBuf,
}
pub(crate) struct Claim {
    pub accepted: bool,
    pub retract: Vec<String>,
}
impl Store {
    pub fn new(data: &Path) -> Self {
        Self {
            path: data.join("notifications.sqlite3"),
        }
    }
    pub fn exists(&self) -> bool {
        self.path.exists()
    }
    fn open(&self) -> Result<Connection> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(&self.path)?;
        connection.busy_timeout(Duration::from_secs(2))?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
          CREATE TABLE IF NOT EXISTS preferences(singleton INTEGER PRIMARY KEY CHECK(singleton=1), value TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS turns(session TEXT NOT NULL, turn_id TEXT NOT NULL, terminal INTEGER NOT NULL, updated INTEGER NOT NULL, PRIMARY KEY(session,turn_id));
          CREATE TABLE IF NOT EXISTS notices(tag TEXT PRIMARY KEY, session TEXT NOT NULL, turn_id TEXT NOT NULL, category TEXT NOT NULL, valid INTEGER NOT NULL, activated INTEGER NOT NULL DEFAULT 0, created INTEGER NOT NULL);
          CREATE INDEX IF NOT EXISTS notices_turn ON notices(session,turn_id);
          CREATE INDEX IF NOT EXISTS notices_created ON notices(created);")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(connection)
    }
    pub fn preferences(&self) -> Result<Preferences> {
        if !self.path.exists() {
            return Ok(Preferences::default());
        }
        let value: Option<String> = self
            .open()?
            .query_row(
                "SELECT value FROM preferences WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        value
            .map(|value| serde_json::from_str(&value).map_err(anyhow::Error::from))
            .transpose()
            .map(Option::unwrap_or_default)
    }
    pub fn save_preferences(&self, preferences: &Preferences) -> Result<()> {
        self.open()?.execute("INSERT INTO preferences(singleton,value) VALUES(1,?1) ON CONFLICT(singleton) DO UPDATE SET value=excluded.value",[serde_json::to_string(preferences)?])?;
        Ok(())
    }
    pub fn claim(&self, event: &NoticeEvent, tag: &str, at: i64) -> Result<Claim> {
        let mut connection = self.open()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let terminal: Option<bool> = transaction
            .query_row(
                "SELECT terminal FROM turns WHERE session=?1 AND turn_id=?2",
                params![event.session_id.to_string(), event.turn_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if terminal == Some(true) {
            return Ok(Claim {
                accepted: false,
                retract: vec![],
            });
        }
        transaction.execute("INSERT INTO turns(session,turn_id,terminal,updated) VALUES(?1,?2,?3,?4) ON CONFLICT(session,turn_id) DO UPDATE SET terminal=excluded.terminal,updated=excluded.updated",params![event.session_id.to_string(),event.turn_id.to_string(),event.kind.terminal(),at])?;
        let mut retract = vec![];
        if event.kind.terminal() {
            let mut statement = transaction
                .prepare("SELECT tag FROM notices WHERE session=?1 AND turn_id=?2 AND valid=1")?;
            retract = statement
                .query_map(
                    params![event.session_id.to_string(), event.turn_id.to_string()],
                    |row| row.get(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            transaction.execute(
                "UPDATE notices SET valid=0 WHERE session=?1 AND turn_id=?2",
                params![event.session_id.to_string(), event.turn_id.to_string()],
            )?;
        }
        let accepted = if event.kind.text().is_some() {
            transaction.execute("INSERT OR IGNORE INTO notices(tag,session,turn_id,category,valid,created) VALUES(?1,?2,?3,?4,1,?5)",params![tag,event.session_id.to_string(),event.turn_id.to_string(),event.kind.key(),at])?==1
        } else {
            true
        };
        transaction.execute("DELETE FROM notices WHERE created<?1 OR tag IN (SELECT tag FROM notices ORDER BY created DESC,tag LIMIT -1 OFFSET 1024)",[at-30*86400000i64])?;
        transaction.execute("DELETE FROM turns WHERE updated<?1 OR rowid IN (SELECT rowid FROM turns ORDER BY updated DESC,rowid DESC LIMIT -1 OFFSET 4096)",[at-30*86400000i64])?;
        transaction.commit()?;
        Ok(Claim { accepted, retract })
    }
    pub fn invalidate(&self, tag: &str) -> Result<()> {
        self.open()?
            .execute("UPDATE notices SET valid=0 WHERE tag=?1", [tag])?;
        Ok(())
    }
    pub fn invalidate_all(&self) -> Result<Vec<String>> {
        let mut connection = self.open()?;
        let transaction = connection.transaction()?;
        let tags = {
            let mut statement = transaction.prepare("SELECT tag FROM notices WHERE valid=1")?;
            statement
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        transaction.execute("UPDATE notices SET valid=0", [])?;
        transaction.commit()?;
        Ok(tags)
    }
    pub fn activate(&self, tag: &str, at: i64) -> Result<Option<Activation>> {
        let mut connection = self.open()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let route:Option<(String,String)>=transaction.query_row("SELECT session,turn_id FROM notices WHERE tag=?1 AND valid=1 AND activated=0 AND created>=?2",params![tag,at-30*86400000i64],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
        let activation = route
            .map(|(session, turn)| {
                Ok::<_, anyhow::Error>(Activation {
                    session_id: Uuid::parse_str(&session)?,
                    turn_id: Uuid::parse_str(&turn)?,
                })
            })
            .transpose()?;
        if activation.is_some() {
            transaction.execute("UPDATE notices SET activated=1 WHERE tag=?1", [tag])?;
        }
        transaction.commit()?;
        Ok(activation)
    }
}
