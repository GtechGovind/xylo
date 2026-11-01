//! Catalog generations fence stale discovery results and survive process restart.
use super::*;
use rusqlite::{OptionalExtension, params};

pub(super) fn catalog_generation(store: &Store, id: Uuid) -> Result<i64> {
    store.call(move |conn| {
        Ok(conn
            .query_row(
                "SELECT generation FROM connector_catalog_state WHERE id=?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0))
    })
}
pub(super) fn ensure_catalog_available(store: &Store, id: Uuid) -> Result<()> {
    store.call(move |conn| {
        let unavailable: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM connector_catalog_state WHERE id=?1 AND available=0)",
            [id.to_string()],
            |row| row.get(0),
        )?;
        ensure!(
            !unavailable,
            "MCP tool catalog is unavailable after a change; rediscover tools before calling"
        );
        Ok(())
    })
}
/// Delete schemas and revoke read-only annotations in the same durable transaction.
pub(super) fn invalidate_catalog(store: &Store, id: Uuid) -> Result<()> {
    store.call(move |conn| {
        let tx = conn.transaction()?;
        tx.execute("INSERT INTO connector_catalog_state(id,generation,available) VALUES(?1,1,0) ON CONFLICT(id) DO UPDATE SET generation=generation+1,available=0",[id.to_string()])?;
        tx.execute("DELETE FROM connector_tools WHERE id=?1",[id.to_string()])?;
        tx.commit()?;
        Ok(())
    })
}
/// A notification during an in-flight list invalidates that list too. Only a
/// successful discovery of the latest generation can restore executable authority.
pub(super) fn save_catalog_generation(
    store: &Store,
    id: Uuid,
    fingerprint: String,
    json: String,
    generation: i64,
) -> Result<()> {
    store.call(move |conn| {
        let tx = conn.transaction()?;
        let current = tx.query_row("SELECT generation FROM connector_catalog_state WHERE id=?1",[id.to_string()],|row|row.get::<_, i64>(0)).optional()?.unwrap_or(0);
        ensure!(current == generation, "MCP tools changed during discovery; retry discovery");
        tx.execute("INSERT INTO connector_tools(id,fingerprint,json,updated_at) VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET fingerprint=excluded.fingerprint,json=excluded.json,updated_at=excluded.updated_at",params![id.to_string(),fingerprint,json,xylo_core::now_ms()])?;
        tx.execute("INSERT INTO connector_catalog_state(id,generation,available) VALUES(?1,?2,1) ON CONFLICT(id) DO UPDATE SET available=1",params![id.to_string(),generation])?;
        tx.commit()?;
        Ok(())
    })
}
