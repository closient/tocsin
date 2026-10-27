// SPDX-License-Identifier: AGPL-3.0-only

//! Applying a batch another node pushed: its results, outage changes and
//! rollups, each result and outage keyed by the store it came from and its
//! id there (`docs/design/federation.md`). Every row has one writer, its
//! origin, so a batch merges by set union plus one monotone step (an outage
//! ending), and applying one twice changes nothing.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use tocsin_core::incident::FieldError;
use tocsin_core::status::OutageEnding;

use crate::day::Day;
use crate::error::{Error, Result};
use crate::rollup::{self, DailyRollup};
use crate::{FUTURE_TOLERANCE, NewCheckResult, NewOutage, SubjectKind, node};

/// What one node pushed from one of its stores.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteBatch {
    /// The node that pushed it; every row in it must be this node's.
    pub node_id: String,
    /// The store the rows come from: 32 lowercase hex digits.
    pub store_id: String,
    /// Results, in ascending `origin_id`.
    pub results: Vec<RemoteResult>,
    /// Outage changes, in ascending seq: the order the store made them.
    pub outages: Vec<RemoteOutageChange>,
    /// The node's own rollups, by check, node and day.
    pub rollups: Vec<DailyRollup>,
}

/// A result as its store recorded it.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteResult {
    /// Its id in the store it came from.
    pub origin_id: i64,
    /// Whether it is a check's result or a heartbeat's.
    pub kind: SubjectKind,
    /// The result.
    pub result: NewCheckResult,
}

/// An outage opening or ending, as its store numbered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteOutageChange {
    /// The store opened an outage.
    Open {
        /// Its `opened_seq` there.
        seq: i64,
        /// The outage's id there.
        origin_id: i64,
        /// The outage.
        outage: NewOutage,
    },
    /// The store ended an outage it opened earlier.
    End {
        /// Its `ended_seq` there.
        seq: i64,
        /// The outage's id there.
        origin_id: i64,
        /// When it ended (unix milliseconds).
        ended_at_ms: i64,
        /// How it ended.
        ending: OutageEnding,
    },
}

impl RemoteOutageChange {
    fn seq(&self) -> i64 {
        match self {
            RemoteOutageChange::Open { seq, .. } | RemoteOutageChange::End { seq, .. } => *seq,
        }
    }
}

/// What applying a batch did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BatchReport {
    /// Results stored now: not ones already here from an earlier push.
    pub results_applied: usize,
    /// Results skipped because they started before this store's retention
    /// watermark: their day is final here, and the origin's rollup covers it.
    pub results_expired: usize,
    /// The last result `origin_id` the batch is done with (stored, already
    /// here, or expired): where the pusher's cursor moves to.
    pub results_through: Option<i64>,
    /// Outage changes made now.
    pub outage_changes_applied: usize,
    /// The last outage seq the batch is done with.
    pub outages_through: Option<i64>,
    /// Rollups written.
    pub rollups_applied: usize,
    /// When the first row more than [`FUTURE_TOLERANCE`] ahead of this
    /// node's clock says it happened (unix milliseconds), if any: that row
    /// and the rest of its kind wait, unapplied, until the clocks agree.
    /// Rows before it are applied.
    pub skewed_at_ms: Option<i64>,
}

/// Applies `batch` from another node in one IMMEDIATE transaction, given
/// this node's id and clock.
///
/// Results are stored unless already here; one before the retention
/// watermark is skipped and counted. Outage changes apply in seq order: an
/// open is stored unless already here, an end ends its outage unless it
/// already ended the same way. Rollups are upserted, days retention has
/// finalized included, since the origin's rollup for such a day is
/// complete. A row more than [`FUTURE_TOLERANCE`] ahead stops its kind at
/// that row; what came before it is kept, and the report says where.
///
/// The batch's `node_store` row is made on first sight; its state and last
/// push are the caller's to keep.
///
/// # Errors
///
/// Nothing is written when any of these fail. [`Error::Invalid`] if the
/// batch is from this node or this store, holds another node's row, is out
/// of order, opens an outage while the same check's last one is still open,
/// or ends one it never opened; [`Error::Conflict`] if it differs from what
/// the same store already sent for an outage, which means two machines are
/// writing as one node; otherwise the write fails.
pub(crate) fn apply_batch(
    conn: &mut Connection,
    batch: &RemoteBatch,
    local_node_id: &str,
    now_ms: i64,
) -> Result<BatchReport> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    validate(&transaction, batch, local_node_id)?;
    let store = remote_store(&transaction, batch, now_ms)?;
    let horizon_ms =
        now_ms.saturating_add(i64::try_from(FUTURE_TOLERANCE.as_millis()).unwrap_or(i64::MAX));
    let mut report = BatchReport::default();
    apply_results(&transaction, store, &batch.results, horizon_ms, &mut report)?;
    apply_outages(&transaction, store, &batch.outages, horizon_ms, &mut report)?;
    apply_rollups(&transaction, &batch.rollups, horizon_ms, &mut report)?;
    transaction.commit()?;
    Ok(report)
}

/// Refuses a batch from this node or store, one holding another node's
/// row, and one out of order.
fn validate(conn: &Connection, batch: &RemoteBatch, local_node_id: &str) -> Result<()> {
    let mut errors = Vec::new();
    if batch.node_id.is_empty() {
        errors.push(FieldError::new("node_id", "is empty"));
    } else if batch.node_id == local_node_id {
        errors.push(FieldError::new("node_id", "is this node's own"));
    }
    if batch.store_id == node::local_store_id(conn)? {
        errors.push(FieldError::new("store_id", "is this database's own store"));
    }
    let mut previous = 0;
    for (index, remote) in batch.results.iter().enumerate() {
        if remote.result.node_id != batch.node_id {
            errors.push(not_own(format!("results[{index}].node_id"), batch));
        }
        if remote.origin_id <= previous {
            errors.push(FieldError::new(
                format!("results[{index}].origin_id"),
                format!("{} does not follow {previous}", remote.origin_id),
            ));
        }
        previous = remote.origin_id;
    }
    let mut previous = 0;
    for (index, change) in batch.outages.iter().enumerate() {
        if let RemoteOutageChange::Open { outage, .. } = change
            && outage.node_id != batch.node_id
        {
            errors.push(not_own(format!("outages[{index}].node_id"), batch));
        }
        if change.seq() <= previous {
            errors.push(FieldError::new(
                format!("outages[{index}].seq"),
                format!("{} does not follow {previous}", change.seq()),
            ));
        }
        previous = change.seq();
    }
    for (index, rollup) in batch.rollups.iter().enumerate() {
        if rollup.node_id != batch.node_id {
            errors.push(not_own(format!("rollups[{index}].node_id"), batch));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(Error::Invalid(errors))
    }
}

fn not_own(path: String, batch: &RemoteBatch) -> FieldError {
    FieldError::new(
        path,
        format!("is not {}, the node that sent it", batch.node_id),
    )
}

/// The `node_store` row for the batch's store, made on first sight.
fn remote_store(conn: &Connection, batch: &RemoteBatch, now_ms: i64) -> Result<i64> {
    conn.prepare_cached(
        "INSERT INTO node_store (node_id, store_id, local, first_seen_at)
         VALUES (?1, ?2, 0, ?3)
         ON CONFLICT (node_id, store_id) DO NOTHING",
    )?
    .execute(params![batch.node_id, batch.store_id, now_ms])?;
    Ok(conn.query_row(
        "SELECT id FROM node_store WHERE node_id = ?1 AND store_id = ?2",
        params![batch.node_id, batch.store_id],
        |row| row.get(0),
    )?)
}

fn apply_results(
    conn: &Connection,
    store: i64,
    results: &[RemoteResult],
    horizon_ms: i64,
    report: &mut BatchReport,
) -> Result<()> {
    let floor = rollup::purged_before(conn)?;
    let mut insert = conn.prepare_cached(
        "INSERT INTO check_result (node_id, check_id, started_at, duration_ms, health, detail,
             error, status_code, kind, origin_store, origin_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT (origin_store, origin_id) WHERE origin_store IS NOT NULL DO NOTHING",
    )?;
    for remote in results {
        let result = &remote.result;
        if result.started_at_ms > horizon_ms {
            report.skewed_at_ms = Some(result.started_at_ms);
            break;
        }
        if floor.is_some_and(|floor| result.started_at_ms < floor) {
            report.results_expired += 1;
        } else {
            report.results_applied += insert.execute(params![
                result.node_id,
                result.check_id,
                result.started_at_ms,
                result.duration_ms,
                result.health.as_str(),
                result.detail.as_ref().map(ToString::to_string),
                result.error,
                result.status_code,
                remote.kind.as_str(),
                store,
                remote.origin_id,
            ])?;
        }
        report.results_through = Some(remote.origin_id);
    }
    Ok(())
}

/// An outage as a store sent it, read back to compare with a resend.
struct StoredOutage {
    id: i64,
    node_id: String,
    subject_kind: String,
    check_id: String,
    component_id: Option<String>,
    health: String,
    started_at: i64,
    alerted: bool,
    opened_seq: Option<i64>,
    ended_at: Option<i64>,
    ending: Option<String>,
    ended_seq: Option<i64>,
}

fn stored_outage(conn: &Connection, store: i64, origin_id: i64) -> Result<Option<StoredOutage>> {
    Ok(conn
        .prepare_cached(
            "SELECT id, node_id, subject_kind, check_id, component_id, health, started_at,
                    alerted, opened_seq, ended_at, ending, ended_seq
             FROM outage WHERE origin_store = ?1 AND origin_id = ?2",
        )?
        .query_row(params![store, origin_id], |row| {
            Ok(StoredOutage {
                id: row.get(0)?,
                node_id: row.get(1)?,
                subject_kind: row.get(2)?,
                check_id: row.get(3)?,
                component_id: row.get(4)?,
                health: row.get(5)?,
                started_at: row.get(6)?,
                alerted: row.get(7)?,
                opened_seq: row.get(8)?,
                ended_at: row.get(9)?,
                ending: row.get(10)?,
                ended_seq: row.get(11)?,
            })
        })
        .optional()?)
}

fn apply_outages(
    conn: &Connection,
    store: i64,
    changes: &[RemoteOutageChange],
    horizon_ms: i64,
    report: &mut BatchReport,
) -> Result<()> {
    // The highest seq of this store's already here: a new change below it
    // arrived out of order.
    let mut high_seq: i64 = conn.query_row(
        "SELECT max(coalesce(max(opened_seq), 0), coalesce(max(ended_seq), 0))
         FROM outage WHERE origin_store = ?1",
        params![store],
        |row| row.get(0),
    )?;
    for (index, change) in changes.iter().enumerate() {
        let at_ms = match change {
            RemoteOutageChange::Open { outage, .. } => outage.started_at_ms,
            RemoteOutageChange::End { ended_at_ms, .. } => *ended_at_ms,
        };
        if at_ms > horizon_ms {
            report.skewed_at_ms.get_or_insert(at_ms);
            break;
        }
        let applied = match change {
            RemoteOutageChange::Open {
                seq,
                origin_id,
                outage,
            } => open_outage(conn, store, index, *seq, *origin_id, outage, high_seq)?,
            RemoteOutageChange::End {
                seq,
                origin_id,
                ended_at_ms,
                ending,
            } => {
                let end = (*seq, *ended_at_ms, *ending);
                end_outage(conn, store, index, *origin_id, end, high_seq)?
            }
        };
        if applied {
            report.outage_changes_applied += 1;
            high_seq = change.seq();
        }
        report.outages_through = Some(change.seq());
    }
    Ok(())
}

fn out_of_order(index: usize, seq: i64, high_seq: i64) -> Error {
    Error::Invalid(vec![FieldError::new(
        format!("outages[{index}].seq"),
        format!("{seq} arrived after this store's change {high_seq}"),
    )])
}

/// Stores an opened outage unless the store already sent it. Returns
/// whether it was stored.
fn open_outage(
    conn: &Connection,
    store: i64,
    index: usize,
    seq: i64,
    origin_id: i64,
    outage: &NewOutage,
    high_seq: i64,
) -> Result<bool> {
    if let Some(stored) = stored_outage(conn, store, origin_id)? {
        let same = stored.node_id == outage.node_id
            && stored.subject_kind == outage.subject_kind.as_str()
            && stored.check_id == outage.check_id
            && stored.component_id == outage.component_id
            && stored.health == outage.health.as_str()
            && stored.started_at == outage.started_at_ms
            && stored.alerted == outage.alerted
            && stored.opened_seq == Some(seq);
        return if same {
            Ok(false)
        } else {
            Err(Error::Conflict(format!(
                "outage {origin_id} from store {store} was opened differently before"
            )))
        };
    }
    if seq <= high_seq {
        return Err(out_of_order(index, seq, high_seq));
    }
    let open: Option<i64> = conn
        .prepare_cached(
            "SELECT id FROM outage
             WHERE subject_kind = ?1 AND check_id = ?2 AND node_id = ?3 AND ended_at IS NULL",
        )?
        .query_row(
            params![
                outage.subject_kind.as_str(),
                outage.check_id,
                outage.node_id
            ],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(open) = open {
        return Err(Error::Invalid(vec![FieldError::new(
            format!("outages[{index}]"),
            format!("opens an outage while outage {open} for the same check is still open"),
        )]));
    }
    conn.prepare_cached(
        "INSERT INTO outage (node_id, subject_kind, check_id, component_id, health, started_at,
             alerted, opened_seq, origin_store, origin_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )?
    .execute(params![
        outage.node_id,
        outage.subject_kind.as_str(),
        outage.check_id,
        outage.component_id,
        outage.health.as_str(),
        outage.started_at_ms,
        outage.alerted,
        seq,
        store,
        origin_id,
    ])?;
    Ok(true)
}

/// Ends an outage the store opened, unless it already ended the same way.
/// Returns whether it was ended now.
fn end_outage(
    conn: &Connection,
    store: i64,
    index: usize,
    origin_id: i64,
    (seq, ended_at_ms, ending): (i64, i64, OutageEnding),
    high_seq: i64,
) -> Result<bool> {
    let Some(stored) = stored_outage(conn, store, origin_id)? else {
        return Err(Error::Invalid(vec![FieldError::new(
            format!("outages[{index}].origin_id"),
            format!("ends outage {origin_id}, which this store never opened"),
        )]));
    };
    if stored.ended_at.is_some() {
        let same = stored.ended_at == Some(ended_at_ms)
            && stored.ending.as_deref() == Some(ending.as_str())
            && stored.ended_seq == Some(seq);
        return if same {
            Ok(false)
        } else {
            Err(Error::Conflict(format!(
                "outage {origin_id} from store {store} already ended differently"
            )))
        };
    }
    if seq <= high_seq {
        return Err(out_of_order(index, seq, high_seq));
    }
    conn.prepare_cached(
        "UPDATE outage SET ended_at = ?2, ending = ?3, ended_seq = ?4 WHERE id = ?1",
    )?
    .execute(params![stored.id, ended_at_ms, ending.as_str(), seq])?;
    Ok(true)
}

/// Upserts the origin's rollups, days before the watermark included. A
/// rollup for a day that has not begun by `horizon_ms` stops the rest.
fn apply_rollups(
    conn: &Connection,
    rollups: &[DailyRollup],
    horizon_ms: i64,
    report: &mut BatchReport,
) -> Result<()> {
    let last_day = Day::containing(horizon_ms);
    let fresh = rollups
        .iter()
        .position(|rollup| rollup.date > last_day)
        .unwrap_or(rollups.len());
    if let Some(future) = rollups.get(fresh) {
        report.skewed_at_ms.get_or_insert(future.date.start_ms());
    }
    report.rollups_applied = rollup::upsert_all(conn, &rollups[..fresh])?;
    Ok(())
}
