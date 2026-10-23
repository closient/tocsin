// SPDX-License-Identifier: AGPL-3.0-only

//! Daily rollups and the retention watermark, as plain SQL helpers. Every
//! aggregation runs on a reader and only the upsert on the writer, so a large
//! one never holds the write lock while it scans. Finalizing a day for
//! retention also checks, under the write lock, for results committed after
//! the reader's snapshot and recounts the day if there are any.

use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use tocsin_core::Health;

use crate::SubjectKind;
use crate::day::{Day, MS_PER_DAY};
use crate::error::Result;

/// One check's or heartbeat's results on one node for one UTC day.
///
/// A check's figures count results. A heartbeat's are milliseconds: each of
/// its results holds its health until the next result or the end of the
/// day, whichever comes first (until now, today), and time before the day's
/// first result is not counted. A heartbeat reports only when its job runs
/// or a deadline passes, so counting would weigh an hour of pings every
/// minute against a day of hourly misses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DailyRollup {
    /// The check or heartbeat the results belong to.
    pub check_id: String,
    /// The node that recorded them.
    pub node_id: String,
    /// The UTC day.
    pub date: Day,
    /// Whether the figures are result counts or milliseconds.
    pub kind: SubjectKind,
    /// `up + degraded + down`.
    pub total: i64,
    /// Results, or milliseconds, up.
    pub up: i64,
    /// Results, or milliseconds, degraded.
    pub degraded: i64,
    /// Results, or milliseconds, down.
    pub down: i64,
    /// The worst health of any result that day.
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

/// Rolls up results with `from_ms <= started_at < until_ms` per check, node
/// and UTC day: counted for checks, weighed by time for heartbeats, whose
/// last state today holds until `now_ms`. `from_ms` is a day boundary.
pub(crate) fn aggregate(
    conn: &Connection,
    from_ms: i64,
    until_ms: i64,
    now_ms: i64,
) -> Result<Vec<DailyRollup>> {
    let heartbeats = weigh_heartbeats(&heartbeat_results(conn, from_ms, until_ms)?, now_ms);
    let mut rollups = count_checks(conn, from_ms, until_ms, &heartbeats)?;
    rollups.extend(heartbeats);
    Ok(rollups)
}

/// One heartbeat result, as its rollup reads it.
struct HeartbeatResult {
    check_id: String,
    node_id: String,
    started_at: i64,
    health: Health,
}

/// Heartbeat results, ordered by heartbeat, node and time, from an index
/// that holds only heartbeats' rows: on a store of checks alone it is empty.
fn heartbeat_results(
    conn: &Connection,
    from_ms: i64,
    until_ms: i64,
) -> Result<Vec<HeartbeatResult>> {
    let mut statement = conn.prepare_cached(
        "SELECT check_id, node_id, started_at, health FROM check_result
         WHERE started_at >= ?1 AND started_at < ?2 AND kind = 'heartbeat'
         ORDER BY check_id, node_id, started_at, id",
    )?;
    let rows = statement.query_map(params![from_ms, until_ms], |row| {
        Ok(HeartbeatResult {
            check_id: row.get(0)?,
            node_id: row.get(1)?,
            started_at: row.get(2)?,
            health: parse_column(3, &row.get::<_, String>(3)?)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Counts check results, skipping each day `heartbeats` has a rollup for:
/// any heartbeat result makes a day a heartbeat's (the two share one id
/// space, so both kinds land on one day only when an id changes kind).
/// Counting every result scans the time index, which has an entry for each;
/// filtering by kind there would widen every entry, and every write and
/// every purge would pay for it. `started_at` is never negative, so integer
/// division is the floor.
fn count_checks(
    conn: &Connection,
    from_ms: i64,
    until_ms: i64,
    heartbeats: &[DailyRollup],
) -> Result<Vec<DailyRollup>> {
    let weighed: HashSet<(&str, &str, Day)> = heartbeats
        .iter()
        .map(|rollup| {
            (
                rollup.check_id.as_str(),
                rollup.node_id.as_str(),
                rollup.date,
            )
        })
        .collect();
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
            kind: SubjectKind::Check,
            total: row.get(3)?,
            up,
            degraded,
            down,
            worst: DailyRollup::worst_of(up, degraded, down),
        })
    })?;
    let mut rollups = Vec::new();
    for rollup in rows {
        let rollup = rollup?;
        if !weighed.contains(&(
            rollup.check_id.as_str(),
            rollup.node_id.as_str(),
            rollup.date,
        )) {
            rollups.push(rollup);
        }
    }
    Ok(rollups)
}

/// Weighs heartbeat results by how long each held: until the next result
/// that day, or the end of the day, or `now_ms`, whichever comes first.
fn weigh_heartbeats(heartbeats: &[HeartbeatResult], now_ms: i64) -> Vec<DailyRollup> {
    let mut rollups: Vec<DailyRollup> = Vec::new();
    for (index, heartbeat) in heartbeats.iter().enumerate() {
        let date = Day::containing(heartbeat.started_at);
        let next = heartbeats
            .get(index + 1)
            .filter(|next| {
                next.check_id == heartbeat.check_id
                    && next.node_id == heartbeat.node_id
                    && Day::containing(next.started_at) == date
            })
            .map(|next| next.started_at);
        let end = next.unwrap_or_else(|| date.plus(1).start_ms().min(now_ms));
        let held = (end - heartbeat.started_at).max(0);
        let continues = rollups.last().is_some_and(|rollup| {
            rollup.check_id == heartbeat.check_id
                && rollup.node_id == heartbeat.node_id
                && rollup.date == date
        });
        if !continues {
            rollups.push(DailyRollup {
                check_id: heartbeat.check_id.clone(),
                node_id: heartbeat.node_id.clone(),
                date,
                kind: SubjectKind::Heartbeat,
                total: 0,
                up: 0,
                degraded: 0,
                down: 0,
                worst: heartbeat.health,
            });
        }
        let rollup = rollups.last_mut().expect("continued or just pushed");
        rollup.total += held;
        match heartbeat.health {
            Health::Up => rollup.up += held,
            Health::Degraded => rollup.degraded += held,
            Health::Down => rollup.down += held,
        }
        rollup.worst = rollup.worst.max(heartbeat.health);
    }
    rollups
}

/// Inserts or replaces each rollup, skipping days before the retention
/// watermark: those are final and their raw results may already be gone.
/// Returns how many rows were written.
pub(crate) fn upsert(conn: &Connection, rollups: &[DailyRollup]) -> Result<usize> {
    let floor = purged_before(conn)?.unwrap_or(0);
    let mut statement = conn.prepare_cached(
        "INSERT INTO daily_rollup (check_id, node_id, date, total, up, degraded, down, worst, kind)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT (check_id, node_id, date) DO UPDATE SET
             total = excluded.total, up = excluded.up, degraded = excluded.degraded,
             down = excluded.down, worst = excluded.worst, kind = excluded.kind",
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
            rollup.kind.as_str(),
        ])?;
    }
    Ok(written)
}

/// Every stored rollup from `since` on, ordered by check, node and day.
pub(crate) fn since(conn: &Connection, since: Day) -> Result<Vec<DailyRollup>> {
    let mut statement = conn.prepare_cached(
        "SELECT check_id, node_id, date, total, up, degraded, down, worst, kind
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
            row.get::<_, String>(8)?,
        ))
    })?;
    let mut rollups = Vec::new();
    for row in rows {
        let (check_id, node_id, date, total, up, degraded, down, worst, kind) = row?;
        rollups.push(DailyRollup {
            check_id,
            node_id,
            date: parse_column(2, &date)?,
            kind: parse_column(8, &kind)?,
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
    /// The time a heartbeat's last state holds until, if its day is today.
    now_ms: i64,
    rollups: Vec<DailyRollup>,
}

/// Plans the next day to finalize, or `None` once the watermark has reached
/// `cutoff_ms`. Runs on a reader, so the scan never holds the write lock.
pub(crate) fn plan_finalize(
    conn: &Connection,
    cutoff_ms: i64,
    now_ms: i64,
) -> Result<Option<Finalize>> {
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
        now_ms,
        rollups: aggregate(conn, from_ms, until_ms, now_ms)?,
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
        aggregate(&transaction, plan.from_ms, plan.until_ms, plan.now_ms)?
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
