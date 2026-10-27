// SPDX-License-Identifier: AGPL-3.0-only

//! Pushing this store's rows to the nodes upstream of it: how far each has
//! acknowledged them (`push_cursor`), and the next batch past that
//! (`docs/design/federation.md`). Only this node's own rows are pushed:
//! those this store recorded (`origin_store IS NULL`) as `node_id`. A row
//! another node pushed here belongs to that node, which pushes it itself.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, params};
use tocsin_core::federation::wire;
use tocsin_core::incident::Timestamp;

use crate::day::Day;
use crate::error::Result;
use crate::history::subject_kind;
use crate::incident::text;
use crate::rollup::parse_column;

/// How far one upstream has acknowledged this store's rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushCursor {
    /// The last result id it committed; 0 before the first.
    pub results_through: i64,
    /// The last outage seq it committed; 0 before the first.
    pub outages_through: i64,
    /// The first day whose rollups it may not have, or `None` when it has
    /// every day's latest.
    pub rollups_from: Option<Day>,
    /// How many times rewritten rollups have moved `rollups_from` back. A
    /// cursor advanced with a count since passed keeps the rewound day.
    pub rollups_rewound: i64,
    /// When it last acknowledged a push (unix milliseconds).
    pub acked_at_ms: Option<i64>,
}

impl PushCursor {
    /// The cursor as a push states it: `null` before the first row.
    #[must_use]
    pub fn positions(&self) -> wire::Positions {
        wire::Positions {
            results: (self.results_through > 0).then_some(self.results_through),
            outages: (self.outages_through > 0).then_some(self.outages_through),
        }
    }
}

/// The rows past a cursor, at most a batch's worth, and where this node's
/// own rows end.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Unpushed {
    /// Outage changes, in seq order.
    pub outages: Vec<wire::BatchOutageChange>,
    /// Results, in id order.
    pub results: Vec<wire::BatchResult>,
    /// Whole days of rollups, by day, then check.
    pub rollups: Vec<wire::BatchRollup>,
    /// The first day of rollups left out for want of room, if any.
    pub rollups_next: Option<Day>,
    /// The last result id and outage seq this node has.
    pub head: wire::Positions,
}

impl Unpushed {
    /// Rows in the batch.
    #[must_use]
    pub fn len(&self) -> usize {
        self.outages.len() + self.results.len() + self.rollups.len()
    }

    /// Rows in the batch, as a batch size counts them.
    fn rows(&self) -> u32 {
        u32::try_from(self.len()).unwrap_or(u32::MAX)
    }

    /// Whether the batch carries no rows: a heartbeat push.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// `upstream`'s cursor, made at the start if it has none yet: every result
/// and outage change unpushed, and every day of `node_id`'s rollups.
pub(crate) fn ensure_cursor(
    conn: &Connection,
    upstream: &str,
    node_id: &str,
) -> Result<PushCursor> {
    conn.prepare_cached(
        "INSERT INTO push_cursor (upstream_id, rollups_from)
         VALUES (?1, (SELECT min(date) FROM daily_rollup WHERE node_id = ?2))
         ON CONFLICT (upstream_id) DO NOTHING",
    )?
    .execute(params![upstream, node_id])?;
    Ok(cursor(conn, upstream)?.expect("the row was just made"))
}

/// `upstream`'s cursor, if it has one.
pub(crate) fn cursor(conn: &Connection, upstream: &str) -> Result<Option<PushCursor>> {
    let row = conn
        .prepare_cached(
            "SELECT results_through, outages_through, rollups_from, acked_at, rollups_rewound
             FROM push_cursor WHERE upstream_id = ?1",
        )?
        .query_row(params![upstream], |row| {
            let rollups_from: Option<String> = row.get(2)?;
            Ok(PushCursor {
                results_through: row.get(0)?,
                outages_through: row.get(1)?,
                rollups_from: rollups_from.map(|day| parse_column(2, &day)).transpose()?,
                acked_at_ms: row.get(3)?,
                rollups_rewound: row.get(4)?,
            })
        })
        .optional()?;
    Ok(row)
}

/// Moves `upstream`'s cursor to `cursor`, read as `cursor.rollups_rewound`
/// counted. If rollups were rewritten since, `rollups_from` moves only
/// back, to the earlier of the rewound day and `cursor`'s, since the push
/// may have sent the day before it was rewritten. Days are `YYYY-MM-DD`,
/// so text order is day order.
pub(crate) fn advance(conn: &Connection, upstream: &str, cursor: &PushCursor) -> Result<()> {
    conn.prepare_cached(
        "UPDATE push_cursor
         SET results_through = ?2, outages_through = ?3, acked_at = ?5,
             rollups_from = CASE
                 WHEN rollups_rewound = ?6 OR rollups_from IS NULL THEN ?4
                 WHEN ?4 IS NOT NULL AND ?4 < rollups_from THEN ?4
                 ELSE rollups_from
             END
         WHERE upstream_id = ?1",
    )?
    .execute(params![
        upstream,
        cursor.results_through,
        cursor.outages_through,
        cursor.rollups_from.map(|day| day.to_string()),
        cursor.acked_at_ms,
        cursor.rollups_rewound,
    ])?;
    Ok(())
}

/// Moves every cursor's `rollups_from` back to `from` where it is later or
/// unset, and counts the rewind on each, so a push in flight does not move
/// past `from` even when its cursor was already there.
pub(crate) fn repush_rollups_from(conn: &Connection, from: Day) -> Result<()> {
    conn.prepare_cached(
        "UPDATE push_cursor
         SET rollups_from = CASE
                 WHEN rollups_from IS NULL OR rollups_from > ?1 THEN ?1
                 ELSE rollups_from
             END,
             rollups_rewound = rollups_rewound + 1",
    )?
    .execute(params![from.to_string()])?;
    Ok(())
}

/// `node_id`'s rows past `cursor`, at most `limit` of them (rollups may
/// pass it, below). Outage changes come first, since a remote outage is
/// what the receiver alerts on; then results; then rollups, in whole days,
/// from `cursor.rollups_from`. Once results and outages are caught up
/// (they leave room), the first day of rollups is sent whole even if it
/// does not fit, so a day larger than the room left beside a trickle of
/// new results is not put off for ever.
pub(crate) fn unpushed(
    conn: &Connection,
    node_id: &str,
    cursor: &PushCursor,
    limit: u32,
) -> Result<Unpushed> {
    let mut batch = Unpushed {
        outages: outage_changes(conn, node_id, cursor.outages_through, limit)?,
        ..Unpushed::default()
    };
    batch.results = results(conn, node_id, cursor.results_through, limit - batch.rows())?;
    if let Some(from) = cursor.rollups_from {
        // Results and outages caught up when they left room.
        let room = limit - batch.rows();
        let (rollups, next) = rollups(conn, node_id, from, room, room > 0)?;
        batch.rollups = rollups;
        batch.rollups_next = next;
    }
    batch.head = head(conn, node_id)?;
    Ok(batch)
}

fn outage_changes(
    conn: &Connection,
    node_id: &str,
    through: i64,
    limit: u32,
) -> Result<Vec<wire::BatchOutageChange>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut statement = conn.prepare_cached(
        "SELECT opened_seq AS seq, 0, id, subject_kind, check_id, component_id, health,
                started_at, alerted, NULL, NULL
         FROM outage
         WHERE origin_store IS NULL AND node_id = ?1 AND opened_seq > ?2
         UNION ALL
         SELECT ended_seq AS seq, 1, id, subject_kind, check_id, component_id, health,
                started_at, alerted, ended_at, ending
         FROM outage
         WHERE origin_store IS NULL AND node_id = ?1 AND ended_seq > ?2
         ORDER BY seq LIMIT ?3",
    )?;
    let rows = statement.query_map(params![node_id, through, limit], |row| {
        let seq = row.get(0)?;
        let origin_id = row.get(2)?;
        if row.get::<_, i64>(1)? == 1 {
            return Ok(wire::BatchOutageChange::End {
                seq,
                origin_id,
                ended_at: Timestamp::from_millis(row.get(9)?),
                ending: text(row, 10)?,
            });
        }
        Ok(wire::BatchOutageChange::Open {
            seq,
            origin_id,
            node_id: node_id.to_owned(),
            kind: subject_kind(3, row)?,
            check_id: row.get(4)?,
            component_id: row.get(5)?,
            health: parse_column(6, &row.get::<_, String>(6)?)?,
            started_at: Timestamp::from_millis(row.get(7)?),
            alerted: row.get(8)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn results(
    conn: &Connection,
    node_id: &str,
    through: i64,
    limit: u32,
) -> Result<Vec<wire::BatchResult>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut statement = conn.prepare_cached(
        "SELECT id, kind, check_id, started_at, duration_ms, health, detail, error, status_code
         FROM check_result
         WHERE id > ?2 AND origin_store IS NULL AND node_id = ?1
         ORDER BY id LIMIT ?3",
    )?;
    let rows = statement.query_map(params![node_id, through, limit], |row| {
        let detail = row
            .get::<_, Option<String>>(6)?
            .map(|text| {
                serde_json::from_str(&text).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        6,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })
            })
            .transpose()?;
        Ok(wire::BatchResult {
            origin_id: row.get(0)?,
            node_id: node_id.to_owned(),
            kind: subject_kind(1, row)?,
            check_id: row.get(2)?,
            started_at: Timestamp::from_millis(row.get(3)?),
            duration_ms: row.get(4)?,
            health: parse_column(5, &row.get::<_, String>(5)?)?,
            detail,
            error: row.get(7)?,
            status_code: row.get(8)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// `node_id`'s rollups from `from` on, in as many whole days as fit in
/// `room` (the first even if it does not, when `forced`), and the first
/// day left out.
fn rollups(
    conn: &Connection,
    node_id: &str,
    from: Day,
    room: u32,
    forced: bool,
) -> Result<(Vec<wire::BatchRollup>, Option<Day>)> {
    let mut days = conn.prepare_cached(
        "SELECT date, count(*) FROM daily_rollup
         WHERE node_id = ?1 AND date >= ?2 GROUP BY date ORDER BY date",
    )?;
    let days = days
        .query_map(params![node_id, from.to_string()], |row| {
            Ok((
                parse_column::<Day>(0, &row.get::<_, String>(0)?)?,
                row.get::<_, u32>(1)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut taken = 0_u32;
    let mut through = None;
    let mut next = None;
    for (day, rows) in days {
        if taken.saturating_add(rows) > room && !(forced && through.is_none()) {
            next = Some(day);
            break;
        }
        taken += rows;
        through = Some(day);
    }
    let Some(through) = through else {
        return Ok((Vec::new(), next));
    };
    let mut statement = conn.prepare_cached(
        "SELECT check_id, date, kind, total, up, degraded, down, worst FROM daily_rollup
         WHERE node_id = ?1 AND date >= ?2 AND date <= ?3 ORDER BY date, check_id, kind",
    )?;
    let rows = statement.query_map(
        params![node_id, from.to_string(), through.to_string()],
        |row| {
            Ok(wire::BatchRollup {
                check_id: row.get(0)?,
                node_id: node_id.to_owned(),
                date: row.get(1)?,
                kind: subject_kind(2, row)?,
                total: row.get(3)?,
                up: row.get(4)?,
                degraded: row.get(5)?,
                down: row.get(6)?,
                worst: parse_column(7, &row.get::<_, String>(7)?)?,
            })
        },
    )?;
    Ok((rows.collect::<rusqlite::Result<_>>()?, next))
}

/// `node_id`'s last result id and outage seq in this store.
fn head(conn: &Connection, node_id: &str) -> Result<wire::Positions> {
    let results = conn
        .prepare_cached(
            "SELECT id FROM check_result WHERE origin_store IS NULL AND node_id = ?1
             ORDER BY id DESC LIMIT 1",
        )?
        .query_row(params![node_id], |row| row.get(0))
        .optional()?;
    let outages = conn
        .prepare_cached(
            "SELECT max(max(opened_seq), coalesce(max(ended_seq), 0)) FROM outage
             WHERE origin_store IS NULL AND node_id = ?1",
        )?
        .query_row(params![node_id], |row| row.get(0))?;
    Ok(wire::Positions { results, outages })
}

/// For each upstream, how many of this store's own results among those
/// `chunk` selects (as a subquery of `check_result` ids, bound to `?1` and
/// `?2`) it has not acknowledged.
pub(crate) fn unacknowledged_in(
    conn: &Connection,
    chunk: &str,
    before_ms: i64,
    limit: u32,
) -> Result<BTreeMap<String, u64>> {
    let mut statement = conn.prepare_cached(&format!(
        "SELECT cursor.upstream_id, count(*)
         FROM push_cursor AS cursor
         JOIN check_result AS result
             ON result.origin_store IS NULL AND result.id > cursor.results_through
         WHERE result.id IN ({chunk})
         GROUP BY cursor.upstream_id"
    ))?;
    let rows = statement.query_map(params![before_ms, limit], |row| {
        Ok((row.get(0)?, row.get::<_, u32>(1)?.into()))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}
