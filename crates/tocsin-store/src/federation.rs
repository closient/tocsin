// SPDX-License-Identifier: AGPL-3.0-only

//! Applying a batch another node pushed: its results, outage changes and
//! rollups, each result and outage keyed by the store it came from and its
//! id there (`docs/design/federation.md`). Every row has one writer, its
//! origin, so a batch merges by set union plus one monotone step (an outage
//! ending), and applying one twice changes nothing.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use tocsin_core::Health;
use tocsin_core::federation::wire;
use tocsin_core::incident::{FieldError, Timestamp};
use tocsin_core::status::OutageEnding;

use crate::alert::{self, AlertRouting, EndedBy, FiringOutage, Identity};
use crate::day::Day;
use crate::error::{Error, Result};
use crate::rollup::{self, DailyRollup};
use crate::{NewCheckResult, NewOutage, SubjectKind, future_horizon_ms, node};

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

/// How this node alerts for an outage a batch opens, decided by the
/// receiver before the batch is applied (`docs/design/federation.md`, "Who
/// pages"). An open outage with none fires nothing new.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteAlerting {
    /// Where it goes.
    pub routing: AlertRouting,
    /// Whether it may open alerts: false while a node ahead of this one in
    /// `alert_order` is up, when only the alerts it already fired re-fire
    /// until [`Store::take_over`](crate::Store::take_over) opens them.
    pub opens: bool,
    /// When this node heard of it, if that was late (unix milliseconds).
    pub arrived_late_at_ms: Option<i64>,
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
    /// Alert deliveries queued: fires, resolves and cancels.
    pub alerts_queued: usize,
    /// The earliest time a row more than [`FUTURE_TOLERANCE`](crate::FUTURE_TOLERANCE) ahead of
    /// this node's clock says it happened (unix milliseconds), if any. Such
    /// a result or outage change and the rest of its kind wait, unapplied,
    /// until the clocks agree, while the rows before it are applied; such a
    /// rollup waits alone.
    pub skewed_at_ms: Option<i64>,
    /// The stores of the same node this batch's store replaced, as a
    /// rebuilt node's fresh database does: each is retired and its open
    /// outages withdrawn. Empty unless the store pushed here for the first
    /// time while another of its node's was active.
    pub replaced: Vec<String>,
}

impl RemoteBatch {
    /// The batch a node sent over the wire, in the store's terms.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] naming each rollup whose `date` is not a
    /// `YYYY-MM-DD` day.
    pub fn from_wire(batch: wire::Batch) -> Result<Self> {
        let mut errors = Vec::new();
        let rollups = batch
            .rollups
            .into_iter()
            .enumerate()
            .filter_map(|(index, rollup)| {
                let Ok(date) = rollup.date.parse::<Day>() else {
                    errors.push(FieldError::new(
                        format!("rollups[{index}].date"),
                        "is not a YYYY-MM-DD day",
                    ));
                    return None;
                };
                Some(DailyRollup {
                    check_id: rollup.check_id,
                    node_id: rollup.node_id,
                    date,
                    kind: rollup.kind.into(),
                    total: rollup.total,
                    up: rollup.up,
                    degraded: rollup.degraded,
                    down: rollup.down,
                    worst: rollup.worst,
                })
            })
            .collect();
        if !errors.is_empty() {
            return Err(Error::Invalid(errors));
        }
        Ok(RemoteBatch {
            node_id: batch.node,
            store_id: batch.store,
            results: batch
                .results
                .into_iter()
                .map(RemoteResult::from_wire)
                .collect(),
            outages: batch
                .outages
                .into_iter()
                .map(RemoteOutageChange::from_wire)
                .collect(),
            rollups,
        })
    }
}

impl RemoteResult {
    fn from_wire(result: wire::BatchResult) -> Self {
        RemoteResult {
            origin_id: result.origin_id,
            kind: result.kind.into(),
            result: NewCheckResult {
                node_id: result.node_id,
                check_id: result.check_id,
                started_at_ms: result.started_at.as_millis(),
                duration_ms: result.duration_ms,
                health: result.health,
                detail: result.detail,
                error: result.error,
                status_code: result.status_code,
            },
        }
    }
}

impl RemoteOutageChange {
    fn from_wire(change: wire::BatchOutageChange) -> Self {
        match change {
            wire::BatchOutageChange::Open {
                seq,
                origin_id,
                node_id,
                kind,
                check_id,
                component_id,
                health,
                started_at,
                alerted,
            } => RemoteOutageChange::Open {
                seq,
                origin_id,
                outage: NewOutage {
                    node_id,
                    subject_kind: kind.into(),
                    check_id,
                    component_id,
                    health,
                    started_at_ms: started_at.as_millis(),
                    alerted,
                },
            },
            wire::BatchOutageChange::End {
                seq,
                origin_id,
                ended_at,
                ending,
            } => RemoteOutageChange::End {
                seq,
                origin_id,
                ended_at_ms: ended_at.as_millis(),
                ending,
            },
        }
    }
}

impl BatchReport {
    /// The answer to the batch this report is for, whose cursor was
    /// `cursor`: the cursor moves past every row the batch is done with.
    #[must_use]
    pub fn receipt(&self, cursor: wire::Positions) -> wire::BatchReceipt {
        wire::BatchReceipt {
            results_applied: self.results_applied,
            results_expired: self.results_expired,
            outage_changes_applied: self.outage_changes_applied,
            rollups_applied: self.rollups_applied,
            cursor: wire::Positions {
                results: self.results_through.or(cursor.results),
                outages: self.outages_through.or(cursor.outages),
            },
            skewed_at: self.skewed_at_ms.map(Timestamp::from_millis),
        }
    }
}

/// Applies `batch` from another node in one IMMEDIATE transaction, given
/// this node's id and clock.
///
/// Results are stored unless already here; one before the retention
/// watermark is skipped and counted. Outage changes apply in seq order: an
/// open is stored unless already here, an end ends its outage unless it
/// already ended the same way. Rollups are upserted, days retention has
/// finalized included, since the origin's rollup for such a day is
/// complete. A result or outage change more than [`FUTURE_TOLERANCE`](crate::FUTURE_TOLERANCE)
/// ahead stops its kind at that row, keeping what came before it; a rollup
/// for a day that has not begun by then is left out. The report says where.
///
/// The batch's `node_store` row is made on first sight, and its last push
/// set to `now_ms`, in the same transaction, which first refuses the batch
/// if its node or store may no longer push. A node has one active store: a
/// store pushing for the first time while another is active replaces it,
/// as a rebuilt node's fresh database does, and the old one is retired and
/// its open outages withdrawn before the batch's own changes apply, so the
/// new store can open what the old one had open.
///
/// # Errors
///
/// Nothing is written when any of these fail, but for marking the node
/// conflicted. [`Error::Invalid`] if the batch is from this node or this
/// store, holds another node's row or a value the schema refuses, is out
/// of order, opens an outage while the same check's last one is still
/// open, or ends one it never opened or before it started;
/// [`Error::Conflict`] if any of the node's stores is conflicted, or the
/// batch comes from a store a newer one replaced, or it differs from what
/// the same store already sent for an outage: each of the last two means
/// two machines are writing as one node, and marks every store of the node
/// conflicted, so all of them are refused from then on; [`Error::Retired`]
/// if its store was retired with its node; otherwise the write fails.
pub(crate) fn apply_batch(
    conn: &mut Connection,
    batch: &RemoteBatch,
    local_node_id: &str,
    alerting: &BTreeMap<i64, RemoteAlerting>,
    now_ms: i64,
) -> Result<BatchReport> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    match apply_admitted(&transaction, batch, local_node_id, alerting, now_ms) {
        Ok(report) => {
            transaction.commit()?;
            Ok(report)
        }
        Err(conflict @ Error::Conflict(_)) => {
            transaction.rollback()?;
            mark_conflicted(conn, &batch.node_id)?;
            Err(conflict)
        }
        Err(error) => Err(error),
    }
}

/// [`apply_batch`] inside its transaction, which commits only on `Ok`.
fn apply_admitted(
    conn: &Connection,
    batch: &RemoteBatch,
    local_node_id: &str,
    alerting: &BTreeMap<i64, RemoteAlerting>,
    now_ms: i64,
) -> Result<BatchReport> {
    validate(conn, batch, local_node_id)?;
    let replaces = admit(conn, batch)?;
    let origin = Origin {
        row: remote_store(conn, batch, now_ms)?,
        batch,
    };
    let horizon_ms = future_horizon_ms(now_ms);
    let mut report = BatchReport::default();
    for old in replaces {
        report.alerts_queued += replace_store(conn, old.row, now_ms)?;
        report.replaced.push(old.store_id);
    }
    apply_results(conn, &origin, horizon_ms, &mut report)?;
    let touched = apply_outages(conn, &origin, horizon_ms, &mut report)?;
    report.alerts_queued += alert_touched(conn, &touched, alerting, now_ms)?;
    apply_rollups(conn, &batch.rollups, horizon_ms, &mut report)?;
    Ok(report)
}

/// Refuses a batch from this node or store, one holding another node's
/// row, one out of order, and one holding a value the schema would refuse,
/// so the sender learns which field rather than seeing a database error.
fn validate(conn: &Connection, batch: &RemoteBatch, local_node_id: &str) -> Result<()> {
    let mut errors = Vec::new();
    if batch.node_id.is_empty() {
        errors.push(FieldError::new("node", "is empty"));
    } else if batch.node_id == local_node_id {
        errors.push(FieldError::new("node", "is this node's own"));
    }
    if batch.store_id.len() != 32
        || !batch
            .store_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        errors.push(FieldError::new("store", "is not 32 lowercase hex digits"));
    } else if batch.store_id == node::local_store_id(conn)? {
        errors.push(FieldError::new("store", "is this database's own store"));
    }
    validate_results(batch, &mut errors);
    validate_outages(batch, &mut errors);
    validate_rollups(batch, &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(Error::Invalid(errors))
    }
}

fn validate_results(batch: &RemoteBatch, errors: &mut Vec<FieldError>) {
    let mut previous = 0;
    for (index, remote) in batch.results.iter().enumerate() {
        let path = format!("results[{index}]");
        let result = &remote.result;
        if result.node_id != batch.node_id {
            errors.push(not_own(format!("{path}.node_id"), batch));
        }
        if remote.origin_id <= previous {
            errors.push(FieldError::new(
                format!("{path}.origin_id"),
                format!("{} does not follow {previous}", remote.origin_id),
            ));
        }
        previous = remote.origin_id;
        if result.check_id.is_empty() {
            errors.push(FieldError::new(format!("{path}.check_id"), "is empty"));
        }
        if result.started_at_ms < 0 {
            errors.push(FieldError::new(format!("{path}.started_at"), "is negative"));
        }
        if result.duration_ms < 0 {
            errors.push(FieldError::new(
                format!("{path}.duration_ms"),
                "is negative",
            ));
        }
        if result
            .status_code
            .is_some_and(|code| !(0..=999).contains(&code))
        {
            errors.push(FieldError::new(
                format!("{path}.status_code"),
                "is not between 0 and 999",
            ));
        }
    }
}

fn validate_outages(batch: &RemoteBatch, errors: &mut Vec<FieldError>) {
    let mut previous = 0;
    for (index, change) in batch.outages.iter().enumerate() {
        let path = format!("outages[{index}]");
        if change.seq() <= previous {
            errors.push(FieldError::new(
                format!("{path}.seq"),
                format!("{} does not follow {previous}", change.seq()),
            ));
        }
        previous = change.seq();
        let RemoteOutageChange::Open { outage, .. } = change else {
            continue;
        };
        if outage.node_id != batch.node_id {
            errors.push(not_own(format!("{path}.node_id"), batch));
        }
        if outage.check_id.is_empty() {
            errors.push(FieldError::new(format!("{path}.check_id"), "is empty"));
        }
        if outage.health == Health::Up {
            errors.push(FieldError::new(
                format!("{path}.health"),
                "is up; an outage is degraded or down",
            ));
        }
        if outage.started_at_ms < 0 {
            errors.push(FieldError::new(format!("{path}.started_at"), "is negative"));
        }
    }
}

fn validate_rollups(batch: &RemoteBatch, errors: &mut Vec<FieldError>) {
    for (index, rollup) in batch.rollups.iter().enumerate() {
        let path = format!("rollups[{index}]");
        if rollup.node_id != batch.node_id {
            errors.push(not_own(format!("{path}.node_id"), batch));
        }
        if rollup.check_id.is_empty() {
            errors.push(FieldError::new(format!("{path}.check_id"), "is empty"));
        }
        let parts = [rollup.up, rollup.degraded, rollup.down];
        if parts.iter().any(|part| *part < 0)
            || parts
                .iter()
                .try_fold(0_i64, |sum, part| sum.checked_add(*part))
                != Some(rollup.total)
        {
            errors.push(FieldError::new(
                format!("{path}.total"),
                "is not up + degraded + down, each at least 0",
            ));
        }
    }
}

fn not_own(path: String, batch: &RemoteBatch) -> FieldError {
    FieldError::new(
        path,
        format!("is not {}, the node that sent it", batch.node_id),
    )
}

/// One of a node's stores, as [`admit`] reads it.
struct KnownStore {
    row: i64,
    store_id: String,
    state: Option<wire::StoreState>,
    superseded: bool,
}

/// Refuses a batch whose node has a conflicted store, as two machines
/// pushing as one leave it, whose own store a newer one replaced (so two
/// machines are pushing as one), or whose own store was retired with its
/// node: once the node is retired, every store of it is just retired. Otherwise returns the active stores the batch's store replaces:
/// none unless it is pushing here for the first time. Read under the write
/// lock, so a store marked between the read and the write cannot slip a
/// batch through.
fn admit(conn: &Connection, batch: &RemoteBatch) -> Result<Vec<KnownStore>> {
    let mut statement = conn.prepare_cached(
        "SELECT id, store_id, state, superseded_at IS NOT NULL FROM node_store
         WHERE node_id = ?1 AND local = 0 ORDER BY id",
    )?;
    let stores = statement
        .query_map(params![batch.node_id], |row| {
            Ok(KnownStore {
                row: row.get(0)?,
                store_id: row.get(1)?,
                state: row.get::<_, String>(2)?.parse().ok(),
                superseded: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if let Some(conflicted) = stores
        .iter()
        .find(|store| store.state == Some(wire::StoreState::Conflicted))
    {
        return Err(Error::Conflict(format!(
            "node {} is conflicted: store {} pushed as it too; retire one machine",
            batch.node_id, conflicted.store_id
        )));
    }
    // Only retiring the node retires a store without superseding it.
    let node_retired = stores
        .iter()
        .any(|store| store.state == Some(wire::StoreState::Retired) && !store.superseded);
    match stores.iter().find(|store| store.store_id == batch.store_id) {
        Some(own) if own.superseded && !node_retired => Err(Error::Conflict(format!(
            "store {} of node {} was replaced by a newer store and pushed again: two machines \
             are pushing as node {}; retire one",
            batch.store_id, batch.node_id, batch.node_id
        ))),
        Some(own) if own.state == Some(wire::StoreState::Retired) => Err(Error::Retired(format!(
            "store {} of node {} is retired: its pushes are refused",
            batch.store_id, batch.node_id
        ))),
        Some(_) => Ok(Vec::new()),
        None => Ok(stores
            .into_iter()
            .filter(|store| store.state == Some(wire::StoreState::Active))
            .collect()),
    }
}

/// Retires store `row`, which a newer store of its node replaced, and
/// withdraws its open outages. Returns the alert deliveries queued.
fn replace_store(conn: &Connection, row: i64, now_ms: i64) -> Result<usize> {
    conn.prepare_cached("UPDATE node_store SET state = ?2, superseded_at = ?3 WHERE id = ?1")?
        .execute(params![row, wire::StoreState::Retired.as_str(), now_ms])?;
    withdraw_store_outages(conn, row, now_ms)
}

/// Withdraws every outage store `row` has open here at `now_ms` (or its
/// start, if that is later), as for a check removed from the
/// configuration: `ending = withdrawn`, and its alerts end as
/// [`EndedBy::Config`], queueing their resolves. Each end takes the seq
/// straight after its open: the store's own numbering, never this node's,
/// and no later change from it is taken. Returns the deliveries queued.
fn withdraw_store_outages(conn: &Connection, row: i64, now_ms: i64) -> Result<usize> {
    let open = conn
        .prepare_cached("SELECT id FROM outage WHERE origin_store = ?1 AND ended_at IS NULL")?
        .query_map(params![row], |row| row.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut queued = 0;
    for id in open {
        let identity = alert::outage_identity(conn, id, true)?
            .ok_or(Error::Sqlite(rusqlite::Error::QueryReturnedNoRows))?;
        conn.prepare_cached(
            "UPDATE outage SET ended_at = max(?2, started_at), ending = ?3,
                 ended_seq = opened_seq + 1
             WHERE id = ?1",
        )?
        .execute(params![id, now_ms, OutageEnding::Withdrawn.as_str()])?;
        let open_alerts = alert::open_alerts(conn, &identity)?;
        queued += alert::end_alerts(conn, &open_alerts, now_ms, now_ms, EndedBy::Config)?;
    }
    Ok(queued)
}

/// Marks every store of `node_id` conflicted, in a transaction of its own
/// since the batch that found the conflict writes nothing.
fn mark_conflicted(conn: &mut Connection, node_id: &str) -> Result<()> {
    conn.prepare_cached("UPDATE node_store SET state = ?2 WHERE node_id = ?1 AND local = 0")?
        .execute(params![node_id, wire::StoreState::Conflicted.as_str()])?;
    Ok(())
}

/// Retires every store of `node_ids`, nodes the configuration marks
/// retired, and withdraws their open outages, in one transaction. Nothing
/// changes for a store already retired with nothing open, so running it at
/// every start and reload is cheap. Returns the alert deliveries queued.
pub(crate) fn retire_nodes(
    conn: &mut Connection,
    node_ids: &[String],
    now_ms: i64,
) -> Result<usize> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut queued = 0;
    for node_id in node_ids {
        transaction
            .prepare_cached(
                "UPDATE node_store SET state = ?2 WHERE node_id = ?1 AND local = 0 AND state <> ?2",
            )?
            .execute(params![node_id, wire::StoreState::Retired.as_str()])?;
        let rows = transaction
            .prepare_cached("SELECT id FROM node_store WHERE node_id = ?1 AND local = 0")?
            .query_map(params![node_id], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for row in rows {
            queued += withdraw_store_outages(&transaction, row, now_ms)?;
        }
    }
    transaction.commit()?;
    Ok(queued)
}

/// The `node_store` row for the batch's store, made on first sight, its
/// last push moved to `now_ms`.
fn remote_store(conn: &Connection, batch: &RemoteBatch, now_ms: i64) -> Result<i64> {
    conn.prepare_cached(
        "INSERT INTO node_store (node_id, store_id, local, first_seen_at, last_push_at)
         VALUES (?1, ?2, 0, ?3, ?3)
         ON CONFLICT (node_id, store_id) DO UPDATE SET last_push_at = excluded.last_push_at",
    )?
    .execute(params![batch.node_id, batch.store_id, now_ms])?;
    Ok(conn.query_row(
        "SELECT id FROM node_store WHERE node_id = ?1 AND store_id = ?2",
        params![batch.node_id, batch.store_id],
        |row| row.get(0),
    )?)
}

/// The store a batch came from: its `node_store` row here, and the batch.
struct Origin<'a> {
    row: i64,
    batch: &'a RemoteBatch,
}

impl Origin<'_> {
    /// The conflict error for outage `origin_id`, naming the node and store
    /// an operator would recognise.
    fn conflict(&self, origin_id: i64, what: &str) -> Error {
        Error::Conflict(format!(
            "outage {origin_id} from node {} store {} {what}",
            self.batch.node_id, self.batch.store_id
        ))
    }
}

/// Records a row too far ahead, keeping the earliest across kinds.
fn skewed(report: &mut BatchReport, at_ms: i64) {
    report.skewed_at_ms = Some(report.skewed_at_ms.map_or(at_ms, |seen| seen.min(at_ms)));
}

fn apply_results(
    conn: &Connection,
    origin: &Origin<'_>,
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
    for remote in &origin.batch.results {
        let result = &remote.result;
        if result.started_at_ms > horizon_ms {
            skewed(report, result.started_at_ms);
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
                origin.row,
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

/// An alert identity a batch's outage changes touched.
struct Touched<'a> {
    identity: Identity,
    /// The last outage the batch opened for it: its row here, its
    /// `origin_id`, and the outage.
    opened: Option<(i64, i64, &'a NewOutage)>,
    /// When the last outage the batch ended for it ended.
    ended_at_ms: Option<i64>,
}

/// Applies the batch's outage changes in order, and returns the alert
/// identities they touched.
fn apply_outages<'a>(
    conn: &Connection,
    origin: &Origin<'a>,
    horizon_ms: i64,
    report: &mut BatchReport,
) -> Result<Vec<Touched<'a>>> {
    let mut touched: BTreeMap<Identity, Touched<'a>> = BTreeMap::new();
    // The highest seq of this store's already here: a new change below it
    // arrived out of order.
    let mut high_seq: i64 = conn.query_row(
        "SELECT max(coalesce(max(opened_seq), 0), coalesce(max(ended_seq), 0))
         FROM outage WHERE origin_store = ?1",
        params![origin.row],
        |row| row.get(0),
    )?;
    for (index, change) in origin.batch.outages.iter().enumerate() {
        let at_ms = match change {
            RemoteOutageChange::Open { outage, .. } => outage.started_at_ms,
            RemoteOutageChange::End { ended_at_ms, .. } => *ended_at_ms,
        };
        if at_ms > horizon_ms {
            skewed(report, at_ms);
            break;
        }
        let applied = match change {
            RemoteOutageChange::Open {
                seq,
                origin_id,
                outage,
            } => open_outage(conn, origin, index, *seq, *origin_id, outage, high_seq)?.map(|id| {
                let identity = Identity {
                    node_id: outage.node_id.clone(),
                    subject_kind: outage.subject_kind,
                    subject_id: outage.check_id.clone(),
                };
                touch(&mut touched, identity).opened = Some((id, *origin_id, outage));
            }),
            RemoteOutageChange::End {
                seq,
                origin_id,
                ended_at_ms,
                ending,
            } => {
                let end = (*seq, *ended_at_ms, *ending);
                match end_outage(conn, origin, index, *origin_id, end, high_seq)? {
                    Some(id) => {
                        let identity = alert::outage_identity(conn, id, false)?
                            .ok_or(Error::Sqlite(rusqlite::Error::QueryReturnedNoRows))?;
                        touch(&mut touched, identity).ended_at_ms = Some(*ended_at_ms);
                        Some(())
                    }
                    None => None,
                }
            }
        };
        if applied.is_some() {
            report.outage_changes_applied += 1;
            high_seq = change.seq();
        }
        report.outages_through = Some(change.seq());
    }
    Ok(touched.into_values().collect())
}

/// The entry for `identity` in `touched`, made if it is not there yet.
fn touch<'m, 'a>(
    touched: &'m mut BTreeMap<Identity, Touched<'a>>,
    identity: Identity,
) -> &'m mut Touched<'a> {
    touched.entry(identity.clone()).or_insert_with(|| Touched {
        identity,
        opened: None,
        ended_at_ms: None,
    })
}

/// Fires or resolves the alerts of every identity the batch's outage
/// changes touched, now that all of them are applied: an identity whose
/// open outage the batch opened, and which alerts, fires as `alerting`
/// routes it; one with nothing open, or nothing that alerts, resolves. So
/// an outage that opened and ended in the same batch, one that arrived
/// already ended, never fires, and an alert that fired for one ended in a
/// later batch resolves with the same identity. Returns the deliveries
/// queued.
fn alert_touched(
    conn: &Connection,
    touched: &[Touched<'_>],
    alerting: &BTreeMap<i64, RemoteAlerting>,
    now_ms: i64,
) -> Result<usize> {
    let mut queued = 0;
    for entry in touched {
        let open_alerts = alert::open_alerts(conn, &entry.identity)?;
        let open: Option<(i64, bool)> = conn
            .prepare_cached(
                "SELECT id, alerted FROM outage
                 WHERE subject_kind = ?1 AND check_id = ?2 AND node_id = ?3 AND ended_at IS NULL",
            )?
            .query_row(
                params![
                    entry.identity.subject_kind.as_str(),
                    entry.identity.subject_id,
                    entry.identity.node_id
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match (open, entry.opened) {
            (Some((open_id, true)), Some((id, origin_id, outage))) if open_id == id => {
                let remote = alerting.get(&origin_id);
                // Behind a node that is up, only the alerts this node
                // already fired re-fire: `fire_alerts` re-fires every open
                // alert whatever the targets, and with none opens nothing.
                let routing = remote.map(|remote| {
                    let mut routing = remote.routing.clone();
                    if !remote.opens {
                        routing.targets.clear();
                    }
                    routing
                });
                queued += alert::fire_alerts(
                    conn,
                    &FiringOutage {
                        id,
                        open: outage,
                        identity: &entry.identity,
                        arrived_late_at_ms: remote.and_then(|remote| remote.arrived_late_at_ms),
                    },
                    &open_alerts,
                    routing.as_ref(),
                    now_ms,
                )?;
            }
            (Some((_, true)), _) => {}
            _ => {
                let ended_at_ms = entry.ended_at_ms.unwrap_or(now_ms);
                queued +=
                    alert::end_alerts(conn, &open_alerts, ended_at_ms, now_ms, EndedBy::Recovery)?;
            }
        }
    }
    Ok(queued)
}

/// Opens alerts for the other nodes' outages this node holds open and
/// alerted with no alert open yet, wherever `decide` says it now opens
/// them: a node in `alert_order` taking over when the nodes ahead of it go
/// down after the push arrived. A tocsin-kept heartbeat's outage is never
/// taken over, as it never pages from a push. Returns the deliveries
/// queued.
pub(crate) fn take_over(
    conn: &mut Connection,
    local_node_id: &str,
    decide: &dyn Fn(&NewOutage) -> RemoteAlerting,
    now_ms: i64,
) -> Result<usize> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let held: Vec<(i64, NewOutage)> = transaction
        .prepare_cached(
            "SELECT id, node_id, subject_kind, check_id, component_id, health, started_at
             FROM outage
             WHERE origin_store IS NOT NULL AND alerted AND ended_at IS NULL
               AND node_id != ?1 AND instr(check_id, ':') = 0
             ORDER BY id",
        )?
        .query_map([local_node_id], |row| {
            Ok((
                row.get(0)?,
                NewOutage {
                    node_id: row.get(1)?,
                    subject_kind: rollup::parse_column(2, &row.get::<_, String>(2)?)?,
                    check_id: row.get(3)?,
                    component_id: row.get(4)?,
                    health: rollup::parse_column(5, &row.get::<_, String>(5)?)?,
                    started_at_ms: row.get(6)?,
                    alerted: true,
                },
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut queued = 0;
    for (id, outage) in &held {
        let identity = Identity {
            node_id: outage.node_id.clone(),
            subject_kind: outage.subject_kind,
            subject_id: outage.check_id.clone(),
        };
        if !alert::open_alerts(&transaction, &identity)?.is_empty() {
            continue;
        }
        let remote = decide(outage);
        if !remote.opens {
            continue;
        }
        queued += alert::fire_alerts(
            &transaction,
            &FiringOutage {
                id: *id,
                open: outage,
                identity: &identity,
                arrived_late_at_ms: remote.arrived_late_at_ms,
            },
            &[],
            Some(&remote.routing),
            now_ms,
        )?;
    }
    transaction.commit()?;
    Ok(queued)
}

fn out_of_order(index: usize, seq: i64, high_seq: i64) -> Error {
    Error::Invalid(vec![FieldError::new(
        format!("outages[{index}].seq"),
        format!("{seq} arrived after this store's change {high_seq}"),
    )])
}

/// Stores an opened outage unless the store already sent it. Returns its
/// row if it was stored now.
fn open_outage(
    conn: &Connection,
    origin: &Origin<'_>,
    index: usize,
    seq: i64,
    origin_id: i64,
    outage: &NewOutage,
    high_seq: i64,
) -> Result<Option<i64>> {
    if let Some(stored) = stored_outage(conn, origin.row, origin_id)? {
        let same = stored.node_id == outage.node_id
            && stored.subject_kind == outage.subject_kind.as_str()
            && stored.check_id == outage.check_id
            && stored.component_id == outage.component_id
            && stored.health == outage.health.as_str()
            && stored.started_at == outage.started_at_ms
            && stored.alerted == outage.alerted
            && stored.opened_seq == Some(seq);
        return if same {
            Ok(None)
        } else {
            Err(origin.conflict(origin_id, "was opened differently before"))
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
        origin.row,
        origin_id,
    ])?;
    Ok(Some(conn.last_insert_rowid()))
}

/// Ends an outage the store opened, unless it already ended the same way.
/// Returns its row if it was ended now.
fn end_outage(
    conn: &Connection,
    origin: &Origin<'_>,
    index: usize,
    origin_id: i64,
    (seq, ended_at_ms, ending): (i64, i64, OutageEnding),
    high_seq: i64,
) -> Result<Option<i64>> {
    let Some(stored) = stored_outage(conn, origin.row, origin_id)? else {
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
            Ok(None)
        } else {
            Err(origin.conflict(origin_id, "already ended differently"))
        };
    }
    if seq <= high_seq {
        return Err(out_of_order(index, seq, high_seq));
    }
    if ended_at_ms < stored.started_at {
        return Err(Error::Invalid(vec![FieldError::new(
            format!("outages[{index}].ended_at"),
            format!(
                "{ended_at_ms} is before outage {origin_id} started, {}",
                stored.started_at
            ),
        )]));
    }
    conn.prepare_cached(
        "UPDATE outage SET ended_at = ?2, ending = ?3, ended_seq = ?4 WHERE id = ?1",
    )?
    .execute(params![stored.id, ended_at_ms, ending.as_str(), seq])?;
    Ok(Some(stored.id))
}

/// Upserts the origin's rollups, days before the watermark included. A
/// rollup for a day that has not begun by `horizon_ms` is left out; the
/// others still apply, since each day's row stands alone.
fn apply_rollups(
    conn: &Connection,
    rollups: &[DailyRollup],
    horizon_ms: i64,
    report: &mut BatchReport,
) -> Result<()> {
    let last_day = Day::containing(horizon_ms);
    if let Some(future) = rollups
        .iter()
        .filter(|rollup| rollup.date > last_day)
        .map(|rollup| rollup.date)
        .min()
    {
        skewed(report, future.start_ms());
    }
    report.rollups_applied = rollup::upsert_all(
        conn,
        rollups.iter().filter(|rollup| rollup.date <= last_day),
    )?;
    Ok(())
}
