// SPDX-License-Identifier: AGPL-3.0-only

//! Daily rollups and the retention watermark, as plain SQL helpers. The
//! store runs the aggregation on a reader and the upsert on the writer, so a
//! large recompute never holds the write lock while it scans.

use rusqlite::{Connection, OptionalExtension, params};
use tocsin_core::Health;

use crate::day::{Day, MS_PER_DAY};
use crate::error::Result;

/// One check's results on one node for one UTC day.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DailyRollup {
    /// The check the results belong to.
    pub check_id: String,
    /// The node that ran the check.
    pub node_id: String,
    /// The UTC day.
    pub date: Day,
    /// Results that day: `up + degraded + down`.
    pub total: i64,
    /// Results that were up.
    pub up: i64,
    /// Results that were degraded.
    pub degraded: i64,
    /// Results that were down.
    pub down: i64,
    /// The worst health seen that day.
    pub worst: Health,
}

impl DailyRollup {
    fn worst_of(degraded: i64, down: i64) -> Health {
        if down > 0 {
            Health::Down
        } else if degraded > 0 {
            Health::Degraded
        } else {
            Health::Up
        }
    }
}

/// Counts results with `from_ms <= started_at < until_ms` per check, node and
/// UTC day. `started_at` is never negative, so integer division is the floor.
pub(crate) fn aggregate(
    conn: &Connection,
    from_ms: i64,
    until_ms: i64,
) -> Result<Vec<DailyRollup>> {
    let mut statement = conn.prepare_cached(&format!(
        "SELECT check_id, node_id, started_at / {MS_PER_DAY} AS day, count(*),
                sum(health = 'up'), sum(health = 'degraded'), sum(health = 'down')
         FROM check_result
         WHERE started_at >= ?1 AND started_at < ?2
         GROUP BY check_id, node_id, day"
    ))?;
    let rows = statement.query_map(params![from_ms, until_ms], |row| {
        let (up, degraded, down) = (row.get(4)?, row.get(5)?, row.get(6)?);
        Ok(DailyRollup {
            check_id: row.get(0)?,
            node_id: row.get(1)?,
            date: Day::from_epoch_days(row.get(2)?),
            total: row.get(3)?,
            up,
            degraded,
            down,
            worst: DailyRollup::worst_of(degraded, down),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Inserts or replaces each rollup, skipping days before the retention
/// watermark: those are final and their raw results may already be gone.
/// Returns how many rows were written.
pub(crate) fn upsert(conn: &Connection, rollups: &[DailyRollup]) -> Result<usize> {
    let floor = purged_before(conn)?.unwrap_or(0);
    let mut statement = conn.prepare_cached(
        "INSERT INTO daily_rollup (check_id, node_id, date, total, up, degraded, down, worst)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT (check_id, node_id, date) DO UPDATE SET
             total = excluded.total, up = excluded.up, degraded = excluded.degraded,
             down = excluded.down, worst = excluded.worst",
    )?;
    let mut written = 0;
    for rollup in rollups.iter().filter(|r| r.date.start_ms() >= floor) {
        written += statement.execute(params![
            rollup.check_id,
            rollup.node_id,
            rollup.date.to_string(),
            rollup.total,
            rollup.up,
            rollup.degraded,
            rollup.down,
            rollup.worst.as_str(),
        ])?;
    }
    Ok(written)
}

/// Every stored rollup from `since` on, ordered by check, node and day.
pub(crate) fn since(conn: &Connection, since: Day) -> Result<Vec<DailyRollup>> {
    let mut statement = conn.prepare_cached(
        "SELECT check_id, node_id, date, total, up, degraded, down, worst
         FROM daily_rollup WHERE date >= ?1 ORDER BY check_id, node_id, date",
    )?;
    let rows = statement.query_map(params![since.to_string()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
            row.get(6)?,
            row.get::<_, String>(7)?,
        ))
    })?;
    let mut rollups = Vec::new();
    for row in rows {
        let (check_id, node_id, date, total, up, degraded, down, worst) = row?;
        rollups.push(DailyRollup {
            check_id,
            node_id,
            date: parse_column(2, &date)?,
            total,
            up,
            degraded,
            down,
            worst: parse_column(7, &worst)?,
        });
    }
    Ok(rollups)
}

/// The day the hourly rollup should start from: the latest day already
/// rolled up (it may have been partial), else the day of the oldest result.
pub(crate) fn resume_day(conn: &Connection) -> Result<Option<Day>> {
    let latest: Option<String> =
        conn.query_row("SELECT max(date) FROM daily_rollup", [], |row| row.get(0))?;
    if let Some(latest) = latest {
        return Ok(Some(parse_column(0, &latest)?));
    }
    let oldest: Option<i64> =
        conn.query_row("SELECT min(started_at) FROM check_result", [], |row| {
            row.get(0)
        })?;
    Ok(oldest.map(Day::containing))
}

/// The retention watermark, if retention has ever run.
pub(crate) fn purged_before(conn: &Connection) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT purged_before FROM retention WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .optional()?)
}

/// Raises the watermark to `ms`; never lowers it.
pub(crate) fn raise_purged_before(conn: &Connection, ms: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO retention (id, purged_before) VALUES (1, ?1)
         ON CONFLICT (id) DO UPDATE SET purged_before = max(purged_before, excluded.purged_before)",
        params![ms],
    )?;
    Ok(())
}

/// Deletes up to `limit` of the oldest results started before `before_ms`.
/// Returns how many were deleted.
pub(crate) fn delete_chunk(conn: &Connection, before_ms: i64, limit: u32) -> Result<usize> {
    Ok(conn
        .prepare_cached(
            "DELETE FROM check_result WHERE id IN (
                 SELECT id FROM check_result WHERE started_at < ?1 ORDER BY started_at LIMIT ?2
             )",
        )?
        .execute(params![before_ms, limit])?)
}

fn parse_column<T: std::str::FromStr>(index: usize, text: &str) -> rusqlite::Result<T>
where
    T::Err: std::error::Error + Send + Sync + 'static,
{
    text.parse().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}
