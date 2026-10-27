// SPDX-License-Identifier: AGPL-3.0-only

//! Reading outages and recent results back, so the engine can resume a
//! check's state after a restart instead of opening a second outage for one
//! already open or showing an up check as unknown.

use rusqlite::{Connection, OptionalExtension, params};
use tocsin_core::Health;

use crate::alert::SubjectKind;
use crate::error::Result;
use crate::rollup::parse_column;

/// A stored outage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outage {
    /// Its id.
    pub id: i64,
    /// The node whose check was out.
    pub node_id: String,
    /// Whether `check_id` names a check or a heartbeat.
    pub subject_kind: SubjectKind,
    /// The check (or heartbeat) that was out.
    pub check_id: String,
    /// The check's component when the outage opened.
    pub component_id: Option<String>,
    /// `Degraded` or `Down`.
    pub health: Health,
    /// When it started (unix milliseconds).
    pub started_at_ms: i64,
    /// When it ended, or `None` while it is open.
    pub ended_at_ms: Option<i64>,
    /// Whether opening it alerted, so whether ending it resolves.
    pub alerted: bool,
}

/// When a check on a node was last up, for resuming its state after a
/// restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastUp {
    /// The check.
    pub check_id: String,
    /// When its latest `up` result started (unix milliseconds).
    pub last_up_ms: i64,
    /// When it became up: the end of its latest outage, or with none, its
    /// oldest stored result (unix milliseconds).
    pub since_ms: i64,
}

/// When a check (or heartbeat) on a node last recorded any result, for
/// resuming a heartbeat's deadline after a restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestResult {
    /// The check or heartbeat.
    pub check_id: String,
    /// When its latest result, of any health, started (unix milliseconds).
    pub started_at_ms: i64,
}

/// Every open outage recorded by `node_id`, ordered by check.
pub(crate) fn open(conn: &Connection, node_id: &str) -> Result<Vec<Outage>> {
    let mut statement = conn.prepare_cached(
        "SELECT id, node_id, check_id, component_id, health, started_at, ended_at, alerted,
             subject_kind
         FROM outage WHERE node_id = ?1 AND ended_at IS NULL ORDER BY subject_kind, check_id",
    )?;
    let rows = statement.query_map(params![node_id], outage_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// An outage from a row selecting, in order, `id, node_id, check_id,
/// component_id, health, started_at, ended_at, alerted, subject_kind`.
fn outage_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Outage> {
    Ok(Outage {
        id: row.get(0)?,
        node_id: row.get(1)?,
        subject_kind: parse_column(8, &row.get::<_, String>(8)?)?,
        check_id: row.get(2)?,
        component_id: row.get(3)?,
        health: parse_column(4, &row.get::<_, String>(4)?)?,
        started_at_ms: row.get(5)?,
        ended_at_ms: row.get(6)?,
        alerted: row.get(7)?,
    })
}

/// Every open outage recorded by a node other than `node_id`, ordered by
/// node, then check.
pub(crate) fn open_elsewhere(conn: &Connection, node_id: &str) -> Result<Vec<Outage>> {
    let mut statement = conn.prepare_cached(
        "SELECT id, node_id, check_id, component_id, health, started_at, ended_at, alerted,
             subject_kind
         FROM outage WHERE node_id <> ?1 AND ended_at IS NULL
         ORDER BY node_id, subject_kind, check_id",
    )?;
    let rows = statement.query_map(params![node_id], outage_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// For each of `check_ids` that `node_id` has an `up` result for, when it
/// was last up. Each check costs a few lookups on the per-check indexes.
pub(crate) fn last_up(
    conn: &Connection,
    node_id: &str,
    check_ids: &[String],
) -> Result<Vec<LastUp>> {
    let mut latest = conn.prepare_cached(
        "SELECT started_at FROM check_result
         WHERE check_id = ?1 AND node_id = ?2 AND health = 'up'
         ORDER BY started_at DESC LIMIT 1",
    )?;
    let mut ended = conn.prepare_cached(
        "SELECT max(ended_at) FROM outage
                         WHERE subject_kind = 'check' AND check_id = ?1 AND node_id = ?2",
    )?;
    let mut oldest = conn.prepare_cached(
        "SELECT min(started_at) FROM check_result WHERE check_id = ?1 AND node_id = ?2",
    )?;
    let mut found = Vec::new();
    for check_id in check_ids {
        let Some(last_up_ms) = latest
            .query_row(params![check_id, node_id], |row| row.get::<_, i64>(0))
            .optional()?
        else {
            continue;
        };
        let since_ms = match ended.query_row(params![check_id, node_id], |row| row.get(0))? {
            Some(ended_at) => ended_at,
            None => oldest.query_row(params![check_id, node_id], |row| row.get(0))?,
        };
        found.push(LastUp {
            check_id: check_id.clone(),
            last_up_ms,
            since_ms,
        });
    }
    Ok(found)
}

/// For each of `check_ids` that `node_id` has any result for, when the
/// latest one started. Each lookup walks the per-check index
/// (`check_id, started_at`) backwards and stops at the first row on
/// `node_id`, so on a single node it reads one row however long the history.
pub(crate) fn latest_results(
    conn: &Connection,
    node_id: &str,
    check_ids: &[String],
) -> Result<Vec<LatestResult>> {
    let mut latest = conn.prepare_cached(
        "SELECT started_at FROM check_result WHERE check_id = ?1 AND node_id = ?2
         ORDER BY started_at DESC LIMIT 1",
    )?;
    let mut found = Vec::new();
    for check_id in check_ids {
        let started_at: Option<i64> = latest
            .query_row(params![check_id, node_id], |row| row.get(0))
            .optional()?;
        if let Some(started_at_ms) = started_at {
            found.push(LatestResult {
                check_id: check_id.clone(),
                started_at_ms,
            });
        }
    }
    Ok(found)
}
