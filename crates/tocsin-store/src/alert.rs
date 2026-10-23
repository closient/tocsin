// SPDX-License-Identifier: AGPL-3.0-only

//! The alerting outbox: which notifier owes which message for which alert.
//!
//! The rows are written in the transaction that changes the outage, so the
//! decision the engine made (this change alerts, this recovery resolves) is
//! durable the moment the outage is: a delivery worker that is behind, a
//! lossy event channel or a restart can delay a message but never lose it.

use std::fmt;
use std::str::FromStr;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use tocsin_core::Health;

use crate::error::Result;
use crate::rollup::parse_column;
use crate::writer::apply_outage_change;
use crate::{NewOutage, OutageChange};

/// What an alert is about: the second half of its identity, after the node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SubjectKind {
    /// A probe check.
    Check,
    /// A heartbeat: a job that pings tocsin.
    Heartbeat,
}

impl SubjectKind {
    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SubjectKind::Check => "check",
            SubjectKind::Heartbeat => "heartbeat",
        }
    }
}

impl fmt::Display for SubjectKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A stored value that is not a known spelling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownValue(pub String);

impl fmt::Display for UnknownValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown value {:?}", self.0)
    }
}

impl std::error::Error for UnknownValue {}

impl FromStr for SubjectKind {
    type Err = UnknownValue;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "check" => Ok(SubjectKind::Check),
            "heartbeat" => Ok(SubjectKind::Heartbeat),
            other => Err(UnknownValue(other.to_owned())),
        }
    }
}

/// Which message a delivery carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeliveryKind {
    /// The alert started, or its health changed.
    Fire,
    /// The alert is still firing; sent on the route's `repeat` interval.
    Repeat,
    /// The alert ended.
    Resolve,
}

impl DeliveryKind {
    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            DeliveryKind::Fire => "fire",
            DeliveryKind::Repeat => "repeat",
            DeliveryKind::Resolve => "resolve",
        }
    }
}

impl FromStr for DeliveryKind {
    type Err = UnknownValue;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "fire" => Ok(DeliveryKind::Fire),
            "repeat" => Ok(DeliveryKind::Repeat),
            "resolve" => Ok(DeliveryKind::Resolve),
            other => Err(UnknownValue(other.to_owned())),
        }
    }
}

/// One notifier an alerting outage is sent to, as the routes decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertTarget {
    /// The notifier.
    pub notifier_id: String,
    /// The first route that matched.
    pub route_id: String,
    /// Whether the recovery sends a resolve.
    pub resolve: bool,
    /// How often to re-send while the alert fires (milliseconds), if ever.
    pub repeat_ms: Option<i64>,
}

/// Where an outage the engine decided alerts is sent, as handed to
/// [`Store::change_outage_alerting`](crate::Store::change_outage_alerting).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertRouting {
    /// Whether the outage's subject is a check or a heartbeat.
    pub subject_kind: SubjectKind,
    /// The notifiers that get a new alert.
    pub targets: Vec<AlertTarget>,
}

/// What [`Store::change_outage_alerting`](crate::Store::change_outage_alerting)
/// wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutageAlerts {
    /// The opened outage's id, if it opened one.
    pub opened: Option<i64>,
    /// Deliveries queued.
    pub queued: usize,
}

/// An alert as stored: one notifier's view of one identity from fire to
/// resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAlert {
    /// Its id.
    pub id: i64,
    /// The node that saw it: the first half of its identity.
    pub node_id: String,
    /// Check or heartbeat.
    pub subject_kind: SubjectKind,
    /// The check or heartbeat id: the second half of its identity.
    pub subject_id: String,
    /// The subject's component when it last fired.
    pub component_id: Option<String>,
    /// The notifier it is sent to.
    pub notifier_id: String,
    /// The route that sent it there.
    pub route_id: String,
    /// Its latest health.
    pub health: Health,
    /// When the first outage it fired for started (unix milliseconds).
    pub started_at_ms: i64,
    /// When it ended, or `None` while it fires.
    pub ended_at_ms: Option<i64>,
}

/// The oldest pending delivery of one alert: the only one of its deliveries
/// that may be sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDelivery {
    /// Its id.
    pub id: i64,
    /// Fire, repeat or resolve.
    pub kind: DeliveryKind,
    /// The alert's health when it was queued.
    pub health: Health,
    /// Attempts so far.
    pub attempts: u32,
    /// When it may next be attempted (unix milliseconds).
    pub next_attempt_at_ms: i64,
    /// The alert it belongs to.
    pub alert: StoredAlert,
}

/// The outbox as the delivery worker sees it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Outbox {
    /// The head of each alert's pending deliveries, soonest first.
    pub pending: Vec<PendingDelivery>,
    /// The soonest repeat due on an alert with nothing pending, if any.
    pub next_repeat_at_ms: Option<i64>,
}

/// How an attempt to send a delivery ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// The notifier accepted it.
    Delivered,
    /// It failed and will be tried again at `at_ms`.
    Retry {
        /// When (unix milliseconds).
        at_ms: i64,
        /// Why it failed, with secrets redacted.
        error: String,
    },
    /// It failed for good.
    Failed {
        /// Why, with secrets redacted.
        error: String,
    },
}

/// A delivery that failed for good, for an operator to look into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedDelivery {
    /// Its id.
    pub id: i64,
    /// Fire, repeat or resolve.
    pub kind: DeliveryKind,
    /// Attempts made.
    pub attempts: u32,
    /// The last attempt's error.
    pub error: Option<String>,
    /// When it was queued (unix milliseconds).
    pub created_at_ms: i64,
    /// When it was given up on (unix milliseconds).
    pub failed_at_ms: i64,
    /// The alert it belongs to.
    pub alert: StoredAlert,
}

/// How long [`Store::prune_deliveries`](crate::Store::prune_deliveries)
/// keeps finished deliveries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryRetention {
    /// Delivered rows older than this go (milliseconds).
    pub delivered_ms: i64,
    /// Failed rows older than this go (milliseconds).
    pub failed_ms: i64,
}

/// Applies `change` and queues the deliveries it causes, in one
/// transaction.
pub(crate) fn change_outage(
    conn: &mut Connection,
    change: &OutageChange,
    routing: Option<&AlertRouting>,
    now_ms: i64,
) -> Result<OutageAlerts> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let identity = match (&change.open, &change.end) {
        (Some(open), _) => Some((open.node_id.clone(), open.check_id.clone())),
        (None, Some(end)) => transaction
            .prepare_cached("SELECT node_id, check_id FROM outage WHERE id = ?1")?
            .query_row(params![end.id], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?,
        (None, None) => None,
    };
    let id = apply_outage_change(&transaction, change)?;
    let queued = if let Some((node_id, subject_id)) = identity {
        let open_alerts = open_alerts(&transaction, &node_id, &subject_id)?;
        match &change.open {
            Some(open) if open.alerted => fire_alerts(
                &transaction,
                &FiringOutage {
                    id,
                    open,
                    node_id: &node_id,
                    subject_id: &subject_id,
                },
                &open_alerts,
                routing,
                now_ms,
            )?,
            _ => {
                let ended_at_ms = change.end.map_or(now_ms, |end| end.ended_at_ms);
                end_alerts(&transaction, &open_alerts, ended_at_ms, now_ms)?
            }
        }
    } else {
        0
    };
    transaction.commit()?;
    Ok(OutageAlerts {
        opened: change.open.as_ref().map(|_| id),
        queued,
    })
}

/// The outage an alerting change opened, and the identity it fires for.
struct FiringOutage<'a> {
    id: i64,
    open: &'a NewOutage,
    node_id: &'a str,
    subject_id: &'a str,
}

/// The engine decided this change alerts: every alert already firing
/// re-fires with the new health, and every target without one starts
/// firing. Returns the deliveries queued.
fn fire_alerts(
    transaction: &Connection,
    outage: &FiringOutage<'_>,
    open_alerts: &[OpenAlert],
    routing: Option<&AlertRouting>,
    now_ms: i64,
) -> Result<usize> {
    let mut queued = 0;
    for alert in open_alerts {
        transaction
            .prepare_cached(
                "UPDATE alert SET outage_id = ?2, health = ?3, component_id = ?4,
                     next_repeat_at = ?5 + repeat_ms
                 WHERE id = ?1",
            )?
            .execute(params![
                alert.id,
                outage.id,
                outage.open.health.as_str(),
                outage.open.component_id,
                now_ms
            ])?;
        drop_pending_repeats(transaction, alert.id)?;
        queue(
            transaction,
            alert.id,
            DeliveryKind::Fire,
            outage.open.health,
            now_ms,
        )?;
        queued += 1;
    }
    let targets = routing.map_or(&[][..], |routing| &routing.targets[..]);
    for target in targets {
        if open_alerts
            .iter()
            .any(|alert| alert.notifier_id == target.notifier_id)
        {
            continue;
        }
        let subject_kind = routing.map_or(SubjectKind::Check, |r| r.subject_kind);
        transaction
            .prepare_cached(
                "INSERT INTO alert (node_id, subject_kind, subject_id, component_id,
                     notifier_id, route_id, outage_id, health, started_at, resolve,
                     repeat_ms, next_repeat_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12 + ?11)",
            )?
            .execute(params![
                outage.node_id,
                subject_kind.as_str(),
                outage.subject_id,
                outage.open.component_id,
                target.notifier_id,
                target.route_id,
                outage.id,
                outage.open.health.as_str(),
                outage.open.started_at_ms,
                target.resolve,
                target.repeat_ms,
                now_ms,
            ])?;
        let alert_id = transaction.last_insert_rowid();
        queue(
            transaction,
            alert_id,
            DeliveryKind::Fire,
            outage.open.health,
            now_ms,
        )?;
        queued += 1;
    }
    Ok(queued)
}

/// A recovery: every alert firing for the identity ends, and
/// resolves where its route asks. An outage that opens without
/// alerting cannot follow one that alerted (the engine keeps an
/// alerted episode alerted until it recovers), but if it ever
/// did, its alerts end here too rather than fire forever.
/// Returns the deliveries queued.
fn end_alerts(
    transaction: &Connection,
    open_alerts: &[OpenAlert],
    ended_at_ms: i64,
    now_ms: i64,
) -> Result<usize> {
    let mut queued = 0;
    for alert in open_alerts {
        transaction
            .prepare_cached(
                "UPDATE alert SET ended_at = max(?2, started_at), next_repeat_at = NULL
                 WHERE id = ?1",
            )?
            .execute(params![alert.id, ended_at_ms])?;
        drop_pending_repeats(transaction, alert.id)?;
        if alert.resolve {
            queue(
                transaction,
                alert.id,
                DeliveryKind::Resolve,
                alert.health,
                now_ms,
            )?;
            queued += 1;
        }
    }
    Ok(queued)
}

struct OpenAlert {
    id: i64,
    notifier_id: String,
    health: Health,
    resolve: bool,
}

fn open_alerts(conn: &Connection, node_id: &str, subject_id: &str) -> Result<Vec<OpenAlert>> {
    let mut statement = conn.prepare_cached(
        "SELECT id, notifier_id, health, resolve FROM alert
         WHERE node_id = ?1 AND subject_id = ?2 AND ended_at IS NULL ORDER BY id",
    )?;
    let rows = statement.query_map(params![node_id, subject_id], |row| {
        Ok(OpenAlert {
            id: row.get(0)?,
            notifier_id: row.get(1)?,
            health: parse_column(2, &row.get::<_, String>(2)?)?,
            resolve: row.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn queue(
    conn: &Connection,
    alert_id: i64,
    kind: DeliveryKind,
    health: Health,
    now_ms: i64,
) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO alert_delivery (alert_id, kind, health, created_at, next_attempt_at)
         VALUES (?1, ?2, ?3, ?4, ?4)",
    )?
    .execute(params![alert_id, kind.as_str(), health.as_str(), now_ms])?;
    Ok(())
}

/// A repeat still waiting when the alert re-fires or resolves says nothing
/// the newer message does not.
fn drop_pending_repeats(conn: &Connection, alert_id: i64) -> Result<()> {
    conn.prepare_cached(
        "DELETE FROM alert_delivery WHERE alert_id = ?1 AND status = 'pending' AND kind = 'repeat'",
    )?
    .execute(params![alert_id])?;
    Ok(())
}

const ALERT_COLUMNS: &str = "a.id, a.node_id, a.subject_kind, a.subject_id, a.component_id,
    a.notifier_id, a.route_id, a.health, a.started_at, a.ended_at";

fn stored_alert(row: &rusqlite::Row<'_>, first: usize) -> rusqlite::Result<StoredAlert> {
    Ok(StoredAlert {
        id: row.get(first)?,
        node_id: row.get(first + 1)?,
        subject_kind: parse_column(first + 2, &row.get::<_, String>(first + 2)?)?,
        subject_id: row.get(first + 3)?,
        component_id: row.get(first + 4)?,
        notifier_id: row.get(first + 5)?,
        route_id: row.get(first + 6)?,
        health: parse_column(first + 7, &row.get::<_, String>(first + 7)?)?,
        started_at_ms: row.get(first + 8)?,
        ended_at_ms: row.get(first + 9)?,
    })
}

/// The head of every alert's pending deliveries, and the next repeat due.
pub(crate) fn outbox(conn: &Connection) -> Result<Outbox> {
    let mut statement = conn.prepare_cached(&format!(
        "SELECT d.id, d.kind, d.health, d.attempts, d.next_attempt_at, {ALERT_COLUMNS}
         FROM alert_delivery d JOIN alert a ON a.id = d.alert_id
         WHERE d.status = 'pending'
           AND d.id = (SELECT min(p.id) FROM alert_delivery p
                       WHERE p.alert_id = d.alert_id AND p.status = 'pending')
         ORDER BY d.next_attempt_at, d.id"
    ))?;
    let pending = statement
        .query_map([], |row| {
            Ok(PendingDelivery {
                id: row.get(0)?,
                kind: parse_column(1, &row.get::<_, String>(1)?)?,
                health: parse_column(2, &row.get::<_, String>(2)?)?,
                attempts: row.get(3)?,
                next_attempt_at_ms: row.get(4)?,
                alert: stored_alert(row, 5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let next_repeat_at_ms = conn
        .prepare_cached(
            "SELECT min(next_repeat_at) FROM alert a
             WHERE ended_at IS NULL AND next_repeat_at IS NOT NULL
               AND NOT EXISTS (SELECT 1 FROM alert_delivery p
                               WHERE p.alert_id = a.id AND p.status = 'pending')",
        )?
        .query_row([], |row| row.get(0))?;
    Ok(Outbox {
        pending,
        next_repeat_at_ms,
    })
}

/// Queues a repeat for every firing alert that is due one and has nothing
/// pending, and schedules its next. Returns how many it queued.
pub(crate) fn queue_repeats(conn: &mut Connection, now_ms: i64) -> Result<usize> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let due: Vec<(i64, Health)> = transaction
        .prepare_cached(
            "SELECT id, health FROM alert a
             WHERE ended_at IS NULL AND next_repeat_at <= ?1
               AND NOT EXISTS (SELECT 1 FROM alert_delivery p
                               WHERE p.alert_id = a.id AND p.status = 'pending')",
        )?
        .query_map(params![now_ms], |row| {
            Ok((row.get(0)?, parse_column(1, &row.get::<_, String>(1)?)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    for (alert_id, health) in &due {
        queue(
            &transaction,
            *alert_id,
            DeliveryKind::Repeat,
            *health,
            now_ms,
        )?;
        transaction
            .prepare_cached("UPDATE alert SET next_repeat_at = ?2 + repeat_ms WHERE id = ?1")?
            .execute(params![alert_id, now_ms])?;
    }
    transaction.commit()?;
    Ok(due.len())
}

/// Records how an attempt ended. A delivery already finished is left as it
/// is.
pub(crate) fn finish(
    conn: &Connection,
    delivery_id: i64,
    outcome: &DeliveryOutcome,
    now_ms: i64,
) -> Result<()> {
    match outcome {
        DeliveryOutcome::Delivered => conn
            .prepare_cached(
                "UPDATE alert_delivery
                 SET status = 'delivered', attempts = attempts + 1, finished_at = ?2,
                     last_error = NULL
                 WHERE id = ?1 AND status = 'pending'",
            )?
            .execute(params![delivery_id, now_ms])?,
        DeliveryOutcome::Retry { at_ms, error } => conn
            .prepare_cached(
                "UPDATE alert_delivery
                 SET attempts = attempts + 1, next_attempt_at = ?2, last_error = ?3
                 WHERE id = ?1 AND status = 'pending'",
            )?
            .execute(params![delivery_id, at_ms, error])?,
        DeliveryOutcome::Failed { error } => conn
            .prepare_cached(
                "UPDATE alert_delivery
                 SET status = 'failed', attempts = attempts + 1, finished_at = ?2,
                     last_error = ?3
                 WHERE id = ?1 AND status = 'pending'",
            )?
            .execute(params![delivery_id, now_ms, error])?,
    };
    Ok(())
}

/// Deletes finished deliveries older than `retention` allows, then ended
/// alerts left with none. Returns how many deliveries it deleted.
pub(crate) fn prune(
    conn: &mut Connection,
    retention: DeliveryRetention,
    now_ms: i64,
) -> Result<usize> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let deleted = transaction
        .prepare_cached(
            "DELETE FROM alert_delivery
             WHERE (status = 'delivered' AND finished_at < ?1)
                OR (status = 'failed' AND finished_at < ?2)",
        )?
        .execute(params![
            now_ms.saturating_sub(retention.delivered_ms),
            now_ms.saturating_sub(retention.failed_ms)
        ])?;
    transaction
        .prepare_cached(
            "DELETE FROM alert WHERE ended_at IS NOT NULL
               AND NOT EXISTS (SELECT 1 FROM alert_delivery d WHERE d.alert_id = alert.id)",
        )?
        .execute([])?;
    transaction.commit()?;
    Ok(deleted)
}

/// Deliveries that failed for good, newest first.
pub(crate) fn failed(conn: &Connection, limit: u32) -> Result<Vec<FailedDelivery>> {
    let mut statement = conn.prepare_cached(&format!(
        "SELECT d.id, d.kind, d.attempts, d.last_error, d.created_at, d.finished_at, {ALERT_COLUMNS}
         FROM alert_delivery d JOIN alert a ON a.id = d.alert_id
         WHERE d.status = 'failed'
         ORDER BY d.finished_at DESC, d.id DESC
         LIMIT ?1"
    ))?;
    let rows = statement.query_map(params![limit], |row| {
        Ok(FailedDelivery {
            id: row.get(0)?,
            kind: parse_column(1, &row.get::<_, String>(1)?)?,
            attempts: row.get(2)?,
            error: row.get(3)?,
            created_at_ms: row.get(4)?,
            failed_at_ms: row.get(5)?,
            alert: stored_alert(row, 6)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Every alert, oldest first: for tests and diagnostics.
pub(crate) fn alerts(conn: &Connection) -> Result<Vec<StoredAlert>> {
    let mut statement = conn.prepare_cached(&format!(
        "SELECT {ALERT_COLUMNS} FROM alert a ORDER BY a.id"
    ))?;
    let rows = statement.query_map([], |row| stored_alert(row, 0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}
