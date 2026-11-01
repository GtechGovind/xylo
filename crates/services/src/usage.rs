//! Recorded turn usage and aggregate totals, queried on the store worker.
use crate::Store;
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::BTreeMap;
use uuid::Uuid;
use xylo_core::Usage;

/// Tokens and estimates for completed, interrupted and active turns.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageTotals {
    pub turns: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub estimated_usd: f64,
    pub unpriced_calls: u64,
}

/// Details for requested visible answers, plus conversation and local-day totals.
#[derive(Clone, Debug, Default)]
pub struct UsageReport {
    pub messages: BTreeMap<Uuid, Usage>,
    pub conversation: UsageTotals,
    pub daily: UsageTotals,
}

impl Store {
    /// Replaces one request's latest token receipt before model output advances.
    pub fn record_turn_usage(
        &self,
        session: Uuid,
        turn: Uuid,
        call: Uuid,
        usage: Option<xylo_core::pricing::UsageCall>,
    ) -> Result<()> {
        let json = serde_json::to_string(&usage)?;
        self.call(move |connection| {
            connection.execute("INSERT INTO turn_usage(session_id,turn_id,call_id,json) SELECT ?1,?2,?3,?4 WHERE NOT EXISTS(SELECT 1 FROM completed_turns WHERE session_id=?1 AND turn_id=?2) ON CONFLICT(session_id,turn_id,call_id) DO UPDATE SET json=excluded.json", params![session.to_string(),turn.to_string(),call.to_string(),json])?;
            Ok(())
        })
    }
    /// Restores receipts for an interrupted turn without affecting other turns.
    pub fn turn_usage(
        &self,
        session: Uuid,
        turn: Uuid,
    ) -> Result<Vec<(Uuid, Option<xylo_core::pricing::UsageCall>)>> {
        self.call(move |connection| {
            let mut statement = connection.prepare_cached("SELECT call_id,json FROM turn_usage WHERE session_id=?1 AND turn_id=?2 ORDER BY rowid")?;
            let rows = statement.query_map(params![session.to_string(),turn.to_string()], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))?;
            rows.map(|row| { let (id,json) = row?; Ok((Uuid::parse_str(&id)?,serde_json::from_str(&json)?)) }).collect()
        })
    }

    /// Gets at most 500 answer details without reading unrelated message bodies.
    pub fn usage_report(
        &self,
        session: Uuid,
        messages: Vec<Uuid>,
        day_start: i64,
        day_end: i64,
    ) -> Result<UsageReport> {
        ensure!(
            messages.len() <= 500,
            "Load usage for at most 500 visible answers"
        );
        ensure!(
            day_start < day_end,
            "The usage day must have a positive duration"
        );
        self.call(move |connection| {
            let mut conversation = totals(connection, "session_id=?1", &[&session.to_string()])?;
            let mut daily = totals(connection, "completed_at>=?1 AND completed_at<?2", &[&day_start, &day_end])?;
            let mut details = BTreeMap::new();
            let mut statement = connection.prepare_cached("SELECT json_extract(json,'$.usage') FROM completed_turns WHERE session_id=?1 AND json_extract(json,'$.reply_id')=?2")?;
            for id in &messages {
                let usage: Option<String> = statement.query_row(params![session.to_string(), id.to_string()], |row| row.get(0)).optional()?;
                if let Some(usage) = usage { details.insert(*id, serde_json::from_str(&usage)?); }
            }
            // Completion atomically replaces these request receipts. Failed and
            // cancelled turns keep them, so excluding this table silently made
            // paid requests disappear from the UI after an error or restart.
            let overrides: Option<String> = connection.query_row(
                "SELECT json_extract(value,'$.pricing_overrides') FROM settings WHERE key='app'", [], |row| row.get(0)
            ).optional()?.flatten();
            let overrides = overrides.map(|json| serde_json::from_str(&json)).transpose()?.unwrap_or_default();
            let mut timestamp = connection.prepare_cached("SELECT MIN(created_at) FROM messages WHERE session_id=?1 AND id IN (?2,?3)")?;
            for ((chat, turn), mut usage) in pending_usage(connection)? {
                // A pending turn has no finalized estimate yet. Apply current
                // configured prices without changing completed receipts.
                usage.estimated_cost = Some(xylo_core::pricing::estimate(&usage.calls, &overrides));
                let reply = Uuid::new_v5(&turn, b"assistant-interrupted");
                if chat == session {
                    add_usage(&mut conversation, &usage);
                    if messages.contains(&reply) { details.insert(reply, usage.clone()); }
                }
                // Pending receipts predate timestamped request accounting; use
                // their stable user-message date until completion finalizes the
                // turn. No transcript body or credential is read for this lookup.
                let started: Option<i64> = timestamp.query_row(params![chat.to_string(), Uuid::new_v5(&turn, b"user-message").to_string(), reply.to_string()], |row| row.get(0))?;
                if started.is_some_and(|time| (day_start..day_end).contains(&time)) {
                    add_usage(&mut daily, &usage);
                }
            }
            Ok(UsageReport {messages:details, conversation, daily})
        })
    }
}

fn pending_usage(connection: &Connection) -> Result<BTreeMap<(Uuid, Uuid), Usage>> {
    let mut statement = connection.prepare_cached("SELECT session_id,turn_id,json FROM turn_usage WHERE NOT EXISTS(SELECT 1 FROM completed_turns WHERE completed_turns.session_id=turn_usage.session_id AND completed_turns.turn_id=turn_usage.turn_id)")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut turns: BTreeMap<(Uuid, Uuid), Usage> = BTreeMap::new();
    for row in rows {
        let (session, turn, json) = row?;
        let usage = turns
            .entry((Uuid::parse_str(&session)?, Uuid::parse_str(&turn)?))
            .or_default();
        let receipt: Option<xylo_core::pricing::UsageCall> = serde_json::from_str(&json)?;
        if let Some(receipt) = receipt {
            usage.input_tokens = usage.input_tokens.saturating_add(receipt.input_tokens);
            usage.output_tokens = usage.output_tokens.saturating_add(receipt.output_tokens);
            usage.cache_read_tokens = Some(
                usage
                    .cache_read_tokens
                    .unwrap_or_default()
                    .saturating_add(receipt.cache_read_tokens),
            );
            usage.cache_write_tokens = Some(
                usage
                    .cache_write_tokens
                    .unwrap_or_default()
                    .saturating_add(receipt.cache_write_tokens),
            );
            usage.calls.push(receipt);
        } else {
            usage.calls.push(xylo_core::pricing::UsageCall::default());
        }
    }
    Ok(turns)
}

fn add_usage(total: &mut UsageTotals, usage: &Usage) {
    total.turns = total.turns.saturating_add(1);
    total.input_tokens = total.input_tokens.saturating_add(usage.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(usage.output_tokens);
    total.cache_read_tokens = total
        .cache_read_tokens
        .saturating_add(usage.cache_read_tokens.unwrap_or_default());
    total.cache_write_tokens = total
        .cache_write_tokens
        .saturating_add(usage.cache_write_tokens.unwrap_or_default());
    if let Some(cost) = usage.estimated_cost {
        total.estimated_usd += cost.usd;
        total.unpriced_calls = total.unpriced_calls.saturating_add(cost.unpriced_calls);
    }
}

fn totals(
    connection: &Connection,
    predicate: &str,
    parameters: &[&dyn rusqlite::ToSql],
) -> Result<UsageTotals> {
    // Only static predicates from this module reach the query. Aggregates stay
    // in SQLite; no transcript or model context is loaded to draw a total.
    let sql = format!(
        "SELECT COUNT(*),
        COALESCE(SUM(json_extract(json,'$.usage.input_tokens')),0),
        COALESCE(SUM(json_extract(json,'$.usage.output_tokens')),0),
        COALESCE(SUM(json_extract(json,'$.usage.cache_read_tokens')),0),
        COALESCE(SUM(json_extract(json,'$.usage.cache_write_tokens')),0),
        COALESCE(SUM(json_extract(json,'$.usage.estimated_cost.usd')),0),
        COALESCE(SUM(COALESCE(json_extract(json,'$.usage.estimated_cost.unpriced_calls'),1)),0)
        FROM completed_turns WHERE {predicate}"
    );
    Ok(connection.query_row(&sql, parameters, |row| {
        Ok(UsageTotals {
            turns: row.get::<_, i64>(0)?.max(0) as u64,
            input_tokens: row.get::<_, i64>(1)?.max(0) as u64,
            output_tokens: row.get::<_, i64>(2)?.max(0) as u64,
            cache_read_tokens: row.get::<_, i64>(3)?.max(0) as u64,
            cache_write_tokens: row.get::<_, i64>(4)?.max(0) as u64,
            estimated_usd: row.get(5)?,
            unpriced_calls: row.get::<_, i64>(6)?.max(0) as u64,
        })
    })?)
}

#[cfg(test)]
mod tests;
