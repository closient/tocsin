// SPDX-License-Identifier: AGPL-3.0-only

//! The alerting outbox: which notifier owes which message for which alert.
//!
//! The rows are written in the transaction that changes the outage, so the
//! decision the engine made (this change alerts, this recovery resolves) is
//! durable the moment the outage is: a delivery worker that is behind, a
//! lossy event channel or a restart can delay a message but never lose it.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use tocsin_core::Health;
use tocsin_core::status::OutageEnding;

use crate::error::{Error, Result};
use crate::rollup::parse_column;
use crate::writer::{apply_outage_change, insert_result};
use crate::{NewCheckResult, NewOutage, OutageChange};

/// What a result, an outage or an alert is about: the second half of an
/// alert's identity, after the node. Rollups weigh the two kinds' results
/// differently (see `docs/storage.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SubjectKind {
    /// A probe check: rolled up by counting results.
    Check,
    /// A heartbeat, a job that pings tocsin: rolled up by how long each
    /// state held.
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
    /// Stops a message that keeps ringing until acknowledged (a Pushover
    /// emergency): queued when the alert re-fires at a health that does not
    /// ring, and on the resolve. It is a lane of its own, so a cancel that
    /// keeps failing never holds up the alert's messages.
    Cancel,
}

impl DeliveryKind {
    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            DeliveryKind::Fire => "fire",
            DeliveryKind::Repeat => "repeat",
            DeliveryKind::Resolve => "resolve",
            DeliveryKind::Cancel => "cancel",
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
            "cancel" => Ok(DeliveryKind::Cancel),
            other => Err(UnknownValue(other.to_owned())),
        }
    }
}

/// Why an alert ended, which its resolve says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EndedBy {
    /// Its check or heartbeat recovered.
    Recovery,
    /// A reload or restart took its check, heartbeat or notifier out of the
    /// configuration, so nobody saw it recover. Its resolve says so rather
    /// than claim a recovery.
    Config,
}

impl EndedBy {
    /// The stored spelling, and the `ended_by` annotation's value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            EndedBy::Recovery => "recovery",
            EndedBy::Config => "config",
        }
    }
}

impl FromStr for EndedBy {
    type Err = UnknownValue;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "recovery" => Ok(EndedBy::Recovery),
            "config" => Ok(EndedBy::Config),
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
    /// How often to re-send while the alert fires (milliseconds), if ever.
    pub repeat_ms: Option<i64>,
}

/// Where an outage the engine decided alerts is sent, as handed to
/// [`Store::change_outage_alerting`](crate::Store::change_outage_alerting).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AlertRouting {
    /// The notifiers that get a new alert.
    pub targets: Vec<AlertTarget>,
    /// Every notifier whose message at the outage's health keeps ringing
    /// until acknowledged (a Pushover emergency), target or not. An alert
    /// that rang on a notifier not in this list gets a cancel.
    pub ringing: Vec<String>,
    /// The subject's configured labels, pinned on each alert this opens:
    /// an alert already firing keeps the set it opened with.
    pub labels: BTreeMap<String, String>,
    /// The subject's configured annotations, pinned the same way.
    pub annotations: BTreeMap<String, String>,
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
    /// Why it ended: `None` while it fires, and for an alert that ended
    /// before tocsin recorded why, which was always a recovery.
    pub ended_by: Option<EndedBy>,
    /// Whether its last message keeps ringing until acknowledged (a
    /// Pushover emergency) and has not been cancelled yet.
    pub ringing: bool,
    /// The outage it last fired for.
    pub outage_id: Option<i64>,
    /// Whether a fire or repeat has been delivered for its current fire,
    /// the one for its latest outage or health.
    pub notified: bool,
    /// Whether a send of a fire or repeat for it was ever started, whatever
    /// came of it. Until one was, the receiver cannot have heard of the
    /// alert: a silence holds back its resolve, and its cancel is dropped.
    pub attempted: bool,
    /// The subject's configured labels when it opened: every message of
    /// it carries these, whatever the configuration says now.
    pub labels: BTreeMap<String, String>,
    /// The subject's configured annotations when it opened.
    pub annotations: BTreeMap<String, String>,
}

/// A pending delivery that may be sent now: the oldest pending message of
/// its alert, or its oldest pending cancel once every message queued before
/// that cancel is finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDelivery {
    /// Its id.
    pub id: i64,
    /// Fire, repeat, resolve or cancel.
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
    /// The head of each alert's pending messages and of its pending
    /// cancels, soonest first.
    pub pending: Vec<PendingDelivery>,
    /// The soonest repeat due on an alert with no message pending, if any.
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
    /// Fire, repeat, resolve or cancel.
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
/// keeps finished deliveries, and how it deletes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryRetention {
    /// Delivered rows older than this go (milliseconds).
    pub delivered_ms: i64,
    /// Failed and silenced rows older than this go (milliseconds): both
    /// are kept for someone looking back at what was not sent.
    pub failed_ms: i64,
    /// Most rows one DELETE removes, so queued writes never wait long.
    pub chunk_rows: u32,
    /// Pause between chunks, letting queued writes through.
    pub pause: Duration,
}

/// What one [`Store::prune_deliveries`](crate::Store::prune_deliveries)
/// pass deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeliveryPrune {
    /// Finished deliveries deleted.
    pub deliveries: usize,
    /// Ended alerts deleted, once none of their deliveries was left.
    pub alerts: usize,
    /// DELETE statements that removed rows.
    pub chunks: usize,
}

/// An alert's identity: the node, and the check or heartbeat it saw.
struct Identity {
    node_id: String,
    subject_kind: SubjectKind,
    subject_id: String,
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
    let alerts = apply_alerting_change(&transaction, change, routing, now_ms)?;
    transaction.commit()?;
    Ok(alerts)
}

/// Stores `result` of `kind`, applies the `change` it confirms and queues
/// the deliveries that causes, in one transaction.
pub(crate) fn record(
    conn: &mut Connection,
    result: &NewCheckResult,
    kind: SubjectKind,
    change: &OutageChange,
    routing: Option<&AlertRouting>,
    now_ms: i64,
) -> Result<OutageAlerts> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    insert_result(&transaction, result, kind)?;
    let alerts = apply_alerting_change(&transaction, change, routing, now_ms)?;
    transaction.commit()?;
    Ok(alerts)
}

/// Applies `change` and queues the deliveries it causes, inside the
/// transaction `transaction` is in.
fn apply_alerting_change(
    transaction: &Connection,
    change: &OutageChange,
    routing: Option<&AlertRouting>,
    now_ms: i64,
) -> Result<OutageAlerts> {
    let identity = match (&change.open, &change.end) {
        (Some(open), _) => Some(Identity {
            node_id: open.node_id.clone(),
            subject_kind: open.subject_kind,
            subject_id: open.check_id.clone(),
        }),
        (None, Some(end)) => outage_identity(transaction, end.id, false)?,
        (None, None) => None,
    };
    let id = apply_outage_change(transaction, change)?;
    let queued = if let Some(identity) = identity {
        let open_alerts = open_alerts(transaction, &identity)?;
        match &change.open {
            Some(open) if open.alerted => fire_alerts(
                transaction,
                &FiringOutage {
                    id,
                    open,
                    identity: &identity,
                },
                &open_alerts,
                routing,
                now_ms,
            )?,
            _ => {
                let ended_at_ms = change.end.map_or(now_ms, |end| end.ended_at_ms);
                end_alerts(
                    transaction,
                    &open_alerts,
                    ended_at_ms,
                    now_ms,
                    EndedBy::Recovery,
                )?
            }
        }
    } else {
        0
    };
    Ok(OutageAlerts {
        opened: change.open.as_ref().map(|_| id),
        queued,
    })
}

/// The identity outage `id` alerts for; with `open_only`, only while the
/// outage is open.
fn outage_identity(conn: &Connection, id: i64, open_only: bool) -> Result<Option<Identity>> {
    Ok(conn
        .prepare_cached(
            "SELECT node_id, subject_kind, check_id FROM outage
             WHERE id = ?1 AND (?2 = 0 OR ended_at IS NULL)",
        )?
        .query_row(params![id, open_only], |row| {
            Ok(Identity {
                node_id: row.get(0)?,
                subject_kind: parse_column(1, &row.get::<_, String>(1)?)?,
                subject_id: row.get(2)?,
            })
        })
        .optional()?)
}

/// Ends outage `id`, whose check or heartbeat is no longer configured, at
/// `now_ms` (or its start, if that is later) as
/// [`OutageEnding::Withdrawn`], and ends its alerts as [`EndedBy::Config`],
/// queueing each one's resolve, in one transaction. Returns the deliveries
/// queued, or zero if the outage was not open.
pub(crate) fn withdraw_outage(conn: &mut Connection, id: i64, now_ms: i64) -> Result<usize> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let Some(identity) = outage_identity(&transaction, id, true)? else {
        return Ok(0);
    };
    transaction
        .prepare_cached(
            "UPDATE outage SET ended_at = max(?2, started_at), ending = ?3 WHERE id = ?1",
        )?
        .execute(params![id, now_ms, OutageEnding::Withdrawn.as_str()])?;
    let open_alerts = open_alerts(&transaction, &identity)?;
    let queued = end_alerts(&transaction, &open_alerts, now_ms, now_ms, EndedBy::Config)?;
    transaction.commit()?;
    Ok(queued)
}

/// Ends every open alert whose notifier is not among `notifiers` at
/// `now_ms`, as [`EndedBy::Config`], queueing its resolve (and its cancel,
/// if it rings) like any other ending: the worker still holds a notifier
/// removed by a reload and sends them through it, and counts each one it
/// cannot send. Returns the alerts ended.
pub(crate) fn end_alerts_without_notifier(
    conn: &mut Connection,
    notifiers: &[String],
    now_ms: i64,
) -> Result<usize> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let orphans: Vec<OpenAlert> = {
        let mut statement = transaction.prepare_cached(
            "SELECT id, notifier_id, health, ringing FROM alert
             WHERE ended_at IS NULL ORDER BY id",
        )?;
        let rows = statement.query_map([], open_alert)?;
        let mut orphans = Vec::new();
        for row in rows {
            let alert = row?;
            if !notifiers.contains(&alert.notifier_id) {
                orphans.push(alert);
            }
        }
        orphans
    };
    end_alerts(&transaction, &orphans, now_ms, now_ms, EndedBy::Config)?;
    transaction.commit()?;
    Ok(orphans.len())
}

/// The outage an alerting change opened, and the identity it fires for.
struct FiringOutage<'a> {
    id: i64,
    open: &'a NewOutage,
    identity: &'a Identity,
}

/// The engine decided this change alerts: every alert already firing
/// re-fires with the new health, and every target without one starts
/// firing. An alert that rang and no longer rings at the new health gets a
/// cancel first. Returns the deliveries queued.
fn fire_alerts(
    transaction: &Connection,
    outage: &FiringOutage<'_>,
    open_alerts: &[OpenAlert],
    routing: Option<&AlertRouting>,
    now_ms: i64,
) -> Result<usize> {
    let rings = |notifier_id: &str| {
        routing.is_some_and(|routing| routing.ringing.iter().any(|id| id == notifier_id))
    };
    let mut queued = 0;
    for alert in open_alerts {
        let ringing = rings(&alert.notifier_id);
        if alert.ringing && !ringing {
            queue(
                transaction,
                alert.id,
                DeliveryKind::Cancel,
                alert.health,
                now_ms,
            )?;
            queued += 1;
        }
        transaction
            .prepare_cached(
                // The new fire is unheard until it is delivered, so a silence
                // that holds it strands the alert; `attempted` stays, as the
                // earlier fire's resolve is still owed.
                "UPDATE alert SET outage_id = ?2, health = ?3, component_id = ?4,
                     next_repeat_at = ?5 + repeat_ms, ringing = ?6, notified = 0
                 WHERE id = ?1",
            )?
            .execute(params![
                alert.id,
                outage.id,
                outage.open.health.as_str(),
                outage.open.component_id,
                now_ms,
                ringing,
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
    let Some(routing) = routing else {
        return Ok(queued);
    };
    let labels = serde_json::to_string(&routing.labels).map_err(Error::Encode)?;
    let annotations = serde_json::to_string(&routing.annotations).map_err(Error::Encode)?;
    for target in &routing.targets {
        if open_alerts
            .iter()
            .any(|alert| alert.notifier_id == target.notifier_id)
        {
            continue;
        }
        transaction
            .prepare_cached(
                "INSERT INTO alert (node_id, subject_kind, subject_id, component_id,
                     notifier_id, route_id, outage_id, health, started_at, ringing,
                     repeat_ms, next_repeat_at, notified, attempted, labels, annotations)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12 + ?11, 0, 0, ?13, ?14)",
            )?
            .execute(params![
                outage.identity.node_id,
                outage.identity.subject_kind.as_str(),
                outage.identity.subject_id,
                outage.open.component_id,
                target.notifier_id,
                target.route_id,
                outage.id,
                outage.open.health.as_str(),
                outage.open.started_at_ms,
                rings(&target.notifier_id),
                target.repeat_ms,
                now_ms,
                labels,
                annotations,
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

/// Every alert in `open_alerts` ends, `ended_by` says why, and each one
/// resolves: every alert that fires is answered, whether its subject
/// recovered or the configuration stopped watching it. One that rang gets a
/// cancel beside its resolve. An outage that opens without alerting cannot
/// follow one that alerted (the engine keeps an alerted episode alerted
/// until it recovers), but if it ever did, its alerts end here too rather
/// than fire forever. Returns the deliveries queued.
fn end_alerts(
    transaction: &Connection,
    open_alerts: &[OpenAlert],
    ended_at_ms: i64,
    now_ms: i64,
    ended_by: EndedBy,
) -> Result<usize> {
    let mut queued = 0;
    for alert in open_alerts {
        transaction
            .prepare_cached(
                "UPDATE alert SET ended_at = max(?2, started_at), ended_by = ?3,
                     next_repeat_at = NULL, ringing = 0
                 WHERE id = ?1",
            )?
            .execute(params![alert.id, ended_at_ms, ended_by.as_str()])?;
        drop_pending_repeats(transaction, alert.id)?;
        if alert.ringing {
            queue(
                transaction,
                alert.id,
                DeliveryKind::Cancel,
                alert.health,
                now_ms,
            )?;
            queued += 1;
        }
        queue(
            transaction,
            alert.id,
            DeliveryKind::Resolve,
            alert.health,
            now_ms,
        )?;
        queued += 1;
    }
    Ok(queued)
}

struct OpenAlert {
    id: i64,
    notifier_id: String,
    health: Health,
    ringing: bool,
}

fn open_alerts(conn: &Connection, identity: &Identity) -> Result<Vec<OpenAlert>> {
    let mut statement = conn.prepare_cached(
        "SELECT id, notifier_id, health, ringing FROM alert
         WHERE node_id = ?1 AND subject_kind = ?2 AND subject_id = ?3 AND ended_at IS NULL
         ORDER BY id",
    )?;
    let rows = statement.query_map(
        params![
            identity.node_id,
            identity.subject_kind.as_str(),
            identity.subject_id
        ],
        open_alert,
    )?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// An [`OpenAlert`] from `id, notifier_id, health, ringing`.
fn open_alert(row: &rusqlite::Row<'_>) -> rusqlite::Result<OpenAlert> {
    Ok(OpenAlert {
        id: row.get(0)?,
        notifier_id: row.get(1)?,
        health: parse_column(2, &row.get::<_, String>(2)?)?,
        ringing: row.get(3)?,
    })
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

/// Which of `notifiers` still has an open alert or a pending delivery.
pub(crate) fn notifiers_with_unfinished_alerts(
    conn: &Connection,
    notifiers: &[String],
) -> Result<Vec<String>> {
    let mut statement = conn.prepare_cached(
        "SELECT 1 FROM alert a WHERE a.notifier_id = ?1
           AND (a.ended_at IS NULL
                OR EXISTS (SELECT 1 FROM alert_delivery d
                           WHERE d.alert_id = a.id AND d.status = 'pending'))
         LIMIT 1",
    )?;
    let mut unfinished = Vec::new();
    for notifier in notifiers {
        if statement.exists(params![notifier])? {
            unfinished.push(notifier.clone());
        }
    }
    Ok(unfinished)
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
    a.notifier_id, a.route_id, a.health, a.started_at, a.ended_at, a.ringing, a.outage_id,
    a.notified, a.attempted, a.labels, a.annotations, a.ended_by";

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
        ringing: row.get(first + 10)?,
        outage_id: row.get(first + 11)?,
        notified: row.get(first + 12)?,
        attempted: row.get(first + 13)?,
        labels: json_map(row, first + 14)?,
        annotations: json_map(row, first + 15)?,
        ended_by: row
            .get::<_, Option<String>>(first + 16)?
            .map(|ended_by| parse_column(first + 16, &ended_by))
            .transpose()?,
    })
}

/// A column holding a JSON object of strings.
fn json_map(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<BTreeMap<String, String>> {
    serde_json::from_str(&row.get::<_, String>(index)?).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

/// Every pending delivery that may be sent, and the next repeat due. A
/// message may go once every message queued before it is finished, so a
/// resolve never overtakes its fire; a cancel may go once every message
/// queued before it is finished, and no message waits for a cancel.
pub(crate) fn outbox(conn: &Connection) -> Result<Outbox> {
    let mut statement = conn.prepare_cached(&format!(
        "SELECT d.id, d.kind, d.health, d.attempts, d.next_attempt_at, {ALERT_COLUMNS}
         FROM alert_delivery d JOIN alert a ON a.id = d.alert_id
         WHERE d.status = 'pending'
           AND d.id = (SELECT min(p.id) FROM alert_delivery p
                       WHERE p.alert_id = d.alert_id AND p.status = 'pending'
                         AND (p.kind = 'cancel') = (d.kind = 'cancel'))
           AND (d.kind <> 'cancel'
                OR NOT EXISTS (SELECT 1 FROM alert_delivery p
                               WHERE p.alert_id = d.alert_id AND p.status = 'pending'
                                 AND p.kind <> 'cancel' AND p.id < d.id))
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
                               WHERE p.alert_id = a.id AND p.status = 'pending'
                                 AND p.kind <> 'cancel')",
        )?
        .query_row([], |row| row.get(0))?;
    Ok(Outbox {
        pending,
        next_repeat_at_ms,
    })
}

/// Queues a repeat for every firing alert that is due one and has no
/// message pending, and schedules its next. Returns how many it queued.
pub(crate) fn queue_repeats(conn: &mut Connection, now_ms: i64) -> Result<usize> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let due: Vec<(i64, Health)> = transaction
        .prepare_cached(
            "SELECT id, health FROM alert a
             WHERE ended_at IS NULL AND next_repeat_at <= ?1
               AND NOT EXISTS (SELECT 1 FROM alert_delivery p
                               WHERE p.alert_id = a.id AND p.status = 'pending'
                                 AND p.kind <> 'cancel')",
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
/// is. A fire or repeat delivered marks its alert notified (and attempted,
/// which [`attempt`] marked before the send).
pub(crate) fn finish(
    conn: &Connection,
    delivery_id: i64,
    outcome: &DeliveryOutcome,
    now_ms: i64,
) -> Result<()> {
    match outcome {
        DeliveryOutcome::Delivered => {
            // The alert first: a crash between the two leaves it notified
            // with the message still pending, which only means its resolve
            // is sent, never that a resolve is held back wrongly.
            conn.prepare_cached(
                "UPDATE alert SET notified = 1, attempted = 1
                 WHERE id = (SELECT alert_id FROM alert_delivery
                             WHERE id = ?1 AND status = 'pending'
                               AND kind IN ('fire', 'repeat'))",
            )?
            .execute(params![delivery_id])?;
            conn.prepare_cached(
                "UPDATE alert_delivery
                 SET status = 'delivered', attempts = attempts + 1, finished_at = ?2,
                     last_error = NULL
                 WHERE id = ?1 AND status = 'pending'",
            )?
            .execute(params![delivery_id, now_ms])?
        }
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

/// Marks alert `alert_id` attempted before a send of its fire or repeat
/// starts, so a send whose answer is lost (a timeout, a crash) still
/// counts as one the receiver may have seen.
pub(crate) fn attempt(conn: &Connection, alert_id: i64) -> Result<()> {
    conn.prepare_cached("UPDATE alert SET attempted = 1 WHERE id = ?1 AND attempted = 0")?
        .execute(params![alert_id])?;
    Ok(())
}

/// Drops pending cancel `delivery_id` if no send of its alert's fire or
/// repeats was ever attempted: nothing rang, so there is nothing to stop.
/// Whether it was dropped.
pub(crate) fn drop_cancel(conn: &Connection, delivery_id: i64) -> Result<bool> {
    Ok(conn
        .prepare_cached(
            "DELETE FROM alert_delivery
             WHERE id = ?1 AND status = 'pending' AND kind = 'cancel'
               AND NOT (SELECT attempted FROM alert a WHERE a.id = alert_delivery.alert_id)",
        )?
        .execute(params![delivery_id])?
        == 1)
}

/// What makes an open alert stranded by a silence: nothing reached the
/// receiver, its latest fire or repeat was held back, and no message is
/// pending. Over `alert a`.
const STRANDED: &str = "a.ended_at IS NULL AND NOT a.notified
    AND (SELECT d.status FROM alert_delivery d
         WHERE d.alert_id = a.id AND d.kind IN ('fire', 'repeat')
         ORDER BY d.id DESC LIMIT 1) = 'silenced'
    AND NOT EXISTS (SELECT 1 FROM alert_delivery p
                    WHERE p.alert_id = a.id AND p.status = 'pending' AND p.kind <> 'cancel')";

/// Every open alert a silence held back that nobody has heard of and that
/// has no message pending: if no silence applies to it any more, it fires
/// afresh with [`refire`].
pub(crate) fn stranded(conn: &Connection) -> Result<Vec<StoredAlert>> {
    let mut statement = conn.prepare_cached(&format!(
        "SELECT {ALERT_COLUMNS} FROM alert a WHERE {STRANDED} ORDER BY a.id"
    ))?;
    let rows = statement.query_map([], |row| stored_alert(row, 0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Queues a fresh fire for alert `alert_id` at its current health, and
/// schedules its next repeat from `now_ms`, if it is still stranded.
/// Whether it queued one.
pub(crate) fn refire(conn: &mut Connection, alert_id: i64, now_ms: i64) -> Result<bool> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let health = transaction
        .prepare_cached(&format!(
            "SELECT a.health FROM alert a WHERE a.id = ?1 AND {STRANDED}"
        ))?
        .query_row(params![alert_id], |row| {
            parse_column::<Health>(0, &row.get::<_, String>(0)?)
        })
        .optional()?;
    let Some(health) = health else {
        return Ok(false);
    };
    transaction
        .prepare_cached("UPDATE alert SET next_repeat_at = ?2 + repeat_ms WHERE id = ?1")?
        .execute(params![alert_id, now_ms])?;
    queue(&transaction, alert_id, DeliveryKind::Fire, health, now_ms)?;
    transaction.commit()?;
    Ok(true)
}

/// Deletes up to `limit` finished deliveries older than `retention` allows.
/// Returns how many it deleted.
pub(crate) fn prune_deliveries_chunk(
    conn: &Connection,
    retention: DeliveryRetention,
    now_ms: i64,
    limit: u32,
) -> Result<usize> {
    Ok(conn
        .prepare_cached(
            "DELETE FROM alert_delivery WHERE id IN (
                 SELECT id FROM alert_delivery
                 WHERE (status = 'delivered' AND finished_at < ?1)
                    OR (status IN ('failed', 'silenced') AND finished_at < ?2)
                 LIMIT ?3
             )",
        )?
        .execute(params![
            now_ms.saturating_sub(retention.delivered_ms),
            now_ms.saturating_sub(retention.failed_ms),
            limit,
        ])?)
}

/// Deletes up to `limit` ended alerts left with no deliveries. Returns how
/// many it deleted.
pub(crate) fn prune_alerts_chunk(conn: &Connection, limit: u32) -> Result<usize> {
    Ok(conn
        .prepare_cached(
            "DELETE FROM alert WHERE id IN (
                 SELECT id FROM alert WHERE ended_at IS NOT NULL
                   AND NOT EXISTS (SELECT 1 FROM alert_delivery d WHERE d.alert_id = alert.id)
                 LIMIT ?1
             )",
        )?
        .execute(params![limit])?)
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
