// SPDX-License-Identifier: AGPL-3.0-only

//! Daily rollups and the retention watermark, as plain SQL helpers. Every
//! aggregation runs on a reader and only the upsert on the writer, so a large
//! one never holds the write lock while it scans. Finalizing a day for
//! retention also checks, under the write lock, for results committed after
//! the reader's snapshot and recounts the day if there are any.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
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
    /// The worst health with at least one result, by [`Health`]'s ordering.
    fn worst_of(up: i64, degraded: i64, down: i64) -> Health {
        [
            (up, Health::Up),
            (degraded, Health::Degraded),
            (down, Health::Down),
        ]
        .into_iter()
        .filter(|&(count, _)| count > 0)
        .map(|(_, health)| health)
        .max()
        .unwrap_or(Health::Up)
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
            worst: DailyRollup::worst_of(up, degraded, down),
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
/// rolled up (it may have been partial), but never later than yesterday, so
/// yesterday and today are recomputed every pass even if some rollup is
/// dated in the future. With no rollups yet, the day of the oldest result.
pub(crate) fn resume_day(conn: &Connection, today: Day) -> Result<Option<Day>> {
    let latest: Option<String> =
        conn.query_row("SELECT max(date) FROM daily_rollup", [], |row| row.get(0))?;
    if let Some(latest) = latest {
        let latest: Day = parse_column(0, &latest)?;
        return Ok(Some(latest.min(today.plus(-1))));
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

/// The next span retention will finalize, counted on a reader.
pub(crate) struct Finalize {
    /// The watermark the span starts at.
    from_ms: i64,
    /// Where the watermark moves to: the end of the oldest day with results,
    /// or `cutoff_ms` if none remain before it.
    until_ms: i64,
    /// `max(id)` read before `rollups` was counted, so every result with an
    /// id up to it is in the count.
    seen_id: i64,
    rollups: Vec<DailyRollup>,
}

/// Plans the next day to finalize, or `None` once the watermark has reached
/// `cutoff_ms`. Runs on a reader, so the scan never holds the write lock.
pub(crate) fn plan_finalize(conn: &Connection, cutoff_ms: i64) -> Result<Option<Finalize>> {
    let from_ms = purged_before(conn)?.unwrap_or(0);
    if from_ms >= cutoff_ms {
        return Ok(None);
    }
    let seen_id: i64 =
        conn.query_row("SELECT coalesce(max(id), 0) FROM check_result", [], |row| {
            row.get(0)
        })?;
    let next: Option<i64> = conn.query_row(
        "SELECT min(started_at) FROM check_result WHERE started_at >= ?1 AND started_at < ?2",
        params![from_ms, cutoff_ms],
        |row| row.get(0),
    )?;
    let until_ms = next.map_or(cutoff_ms, |ms| {
        Day::containing(ms).plus(1).start_ms().min(cutoff_ms)
    });
    Ok(Some(Finalize {
        from_ms,
        until_ms,
        seen_id,
        rollups: aggregate(conn, from_ms, until_ms)?,
    }))
}

/// Writes a planned day's rollups and raises the watermark past it in one
/// IMMEDIATE transaction. A result for the span committed after the plan's
/// count (ids only grow, so it has a larger id) makes it recount the span
/// under the lock; that is the rare path, since results arriving now are
/// for today, not for a day `days` ago. Once the watermark moves, the
/// `check_result` trigger refuses any result before it, so none is deleted
/// uncounted. If another process moved the watermark first, nothing is
/// written and the caller plans again. Returns the rollup rows written.
pub(crate) fn finalize(conn: &mut Connection, plan: Finalize) -> Result<usize> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if purged_before(&transaction)?.unwrap_or(0) != plan.from_ms {
        return Ok(0);
    }
    let late: bool = transaction.query_row(
        "SELECT EXISTS (SELECT 1 FROM check_result
                        WHERE id > ?1 AND started_at >= ?2 AND started_at < ?3)",
        params![plan.seen_id, plan.from_ms, plan.until_ms],
        |row| row.get(0),
    )?;
    let rollups = if late {
        aggregate(&transaction, plan.from_ms, plan.until_ms)?
    } else {
        plan.rollups
    };
    let rows = upsert(&transaction, &rollups)?;
    raise_purged_before(&transaction, plan.until_ms)?;
    transaction.commit()?;
    Ok(rows)
}

/// Deletes up to `limit` of the oldest results started before `before_ms`.
/// Returns how many were deleted.
pub(crate) fn delete_chunk(conn: &mut Connection, before_ms: i64, limit: u32) -> Result<usize> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let deleted = transaction
        .prepare_cached(
            "DELETE FROM check_result WHERE id IN (
                 SELECT id FROM check_result WHERE started_at < ?1 ORDER BY started_at LIMIT ?2
             )",
        )?
        .execute(params![before_ms, limit])?;
    transaction.commit()?;
    Ok(deleted)
}

pub(crate) fn parse_column<T: std::str::FromStr>(index: usize, text: &str) -> rusqlite::Result<T>
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
