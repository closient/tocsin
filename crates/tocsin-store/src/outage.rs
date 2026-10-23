// SPDX-License-Identifier: AGPL-3.0-only

//! Reading outages back, so the engine can resume a check's state after a
//! restart instead of opening a second outage for one already open.

use rusqlite::{Connection, params};
use tocsin_core::Health;

use crate::error::Result;
use crate::rollup::parse_column;

/// A stored outage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outage {
    /// Its id.
    pub id: i64,
    /// The node whose check was out.
    pub node_id: String,
    /// The check that was out.
    pub check_id: String,
    /// The check's component when the outage opened.
    pub component_id: Option<String>,
    /// `Degraded` or `Down`.
    pub health: Health,
    /// When it started (unix milliseconds).
    pub started_at_ms: i64,
    /// When it ended, or `None` while it is open.
    pub ended_at_ms: Option<i64>,
}

/// Every open outage recorded by `node_id`, ordered by check.
pub(crate) fn open(conn: &Connection, node_id: &str) -> Result<Vec<Outage>> {
    let mut statement = conn.prepare_cached(
        "SELECT id, node_id, check_id, component_id, health, started_at, ended_at
         FROM outage WHERE node_id = ?1 AND ended_at IS NULL ORDER BY check_id",
    )?;
    let rows = statement.query_map(params![node_id], |row| {
        Ok(Outage {
            id: row.get(0)?,
            node_id: row.get(1)?,
            check_id: row.get(2)?,
            component_id: row.get(3)?,
            health: parse_column(4, &row.get::<_, String>(4)?)?,
            started_at_ms: row.get(5)?,
            ended_at_ms: row.get(6)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}
