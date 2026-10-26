// SPDX-License-Identifier: AGPL-3.0-only

//! The SQLite store: check results, outages, incidents and releases.
//!
//! The database holds history, never configuration. Results are
//! append-only and every row carries the `node_id` that produced it.
//!
//! One thread owns the only write connection and is fed through a channel;
//! reads go to a small pool of read connections. The database runs in WAL
//! mode, so reads never wait for writes, and a second thread checkpoints the
//! log so writes never wait for a checkpoint. Every timestamp is `i64` unix
//! milliseconds, UTC; see [`day`]. The schema is documented in
//! `docs/storage.md`.

pub mod alert;
mod checkpoint;
pub mod day;
mod error;
mod history;
mod incident;
mod outage;
mod readers;
mod release;
mod rollup;
mod schema;
mod silence;
mod slack;
mod stats;
mod subscriber;
#[cfg(test)]
mod tests;
mod writer;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, TransactionBehavior};
use tocsin_core::Health;
use tocsin_core::incident::{
    Actor, Incident, IncidentEdit, IncidentInternals, NewIncident, NewUpdate, Timestamp,
};
use tocsin_core::release::{DeployStamp, NewRelease, Release, Timeline};
use tocsin_core::silence::{NewSilence, Silence};
use tocsin_core::status::{CheckResult, OutageRecord};

pub use crate::alert::{
    AlertRouting, AlertTarget, DeliveryKind, DeliveryOutcome, DeliveryPrune, DeliveryRetention,
    FailedDelivery, OutageAlerts, Outbox, PendingDelivery, StoredAlert, SubjectKind,
};
use crate::checkpoint::Checkpointer;
pub use crate::day::Day;
pub use crate::error::{Error, Result};
pub use crate::history::{Checks, HistoryPage, Newest, ResultFilter, ResultSlice};
pub use crate::incident::{Drafts, OpenIncident, Opened};
pub use crate::outage::{LastUp, LatestResult, Outage};
use crate::readers::Readers;
pub use crate::release::{ComponentScope, Order, Projects, ReleaseQuery, TimelineQuery, Written};
pub use crate::rollup::DailyRollup;
pub use crate::silence::ActiveSilence;
pub use crate::slack::{
    ClaimedSlack, NewSlackConnection, SlackConnection, SlackCounts, SlackHook, SlackPurpose,
    WebhookUrl,
};
pub use crate::stats::{CheckpointEvent, CheckpointStats};
use crate::subscriber::Lane;
pub use crate::subscriber::{
    ClaimedMail, EVERY, MailOutcome, Mailbag, NewAnnouncement, NewSignup, Preferences, Scope,
    Signup, Subscriber, SubscriberCounts, SubscriberPrune,
};
use crate::writer::{Insert, Writer};

/// The database file's name inside the data folder.
pub const DB_FILE: &str = "tocsin.db";

/// Read connections in the pool.
const READERS: usize = 4;

/// How far after this node's clock a check result may start: room for clock
/// skew between nodes, not for a clock that is simply wrong.
pub const FUTURE_TOLERANCE: Duration = Duration::from_hours(1);

/// The tables `tocsin db info` counts, in schema order.
pub const TABLES: [&str; 20] = [
    "check_result",
    "outage",
    "daily_rollup",
    "incident",
    "incident_component",
    "incident_update",
    "incident_edit",
    "release",
    "retention",
    "alert",
    "alert_delivery",
    "subscriber",
    "subscription",
    "subscriber_event",
    "subscriber_event_target",
    "subscriber_delivery",
    "slack_connection",
    "slack_subscription",
    "slack_delivery",
    "silence",
];

/// One probe run, as handed to [`Store::append`].
#[derive(Debug, Clone, PartialEq)]
pub struct NewCheckResult {
    /// The node that ran the check.
    pub node_id: String,
    /// The check that ran.
    pub check_id: String,
    /// When the run started, unix milliseconds UTC.
    pub started_at_ms: i64,
    /// How long the run took.
    pub duration_ms: i64,
    /// The verdict.
    pub health: Health,
    /// How each assertion fared. Diagnostic only: never queried on.
    pub detail: Option<serde_json::Value>,
    /// Why the run failed, when it did.
    pub error: Option<String>,
    /// The protocol status code, when the check has one (HTTP).
    pub status_code: Option<u16>,
}

/// A machine-detected outage, as handed to [`Store::change_outage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOutage {
    /// The node that saw it.
    pub node_id: String,
    /// Whether `check_id` names a check or a heartbeat: the two have
    /// separate id spaces.
    pub subject_kind: SubjectKind,
    /// The failing check or heartbeat.
    pub check_id: String,
    /// The component the check belongs to, if any.
    pub component_id: Option<String>,
    /// `Degraded` or `Down`; an outage is never `Up`.
    pub health: Health,
    /// When it began, unix milliseconds UTC.
    pub started_at_ms: i64,
    /// Whether opening it alerted. Its end resolves only if it did.
    pub alerted: bool,
}

/// An outage that ends where another opens, written in one transaction by
/// [`Store::change_outage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutageChange {
    /// The open outage to end, if any.
    pub end: Option<OutageEnd>,
    /// The outage to open, if any.
    pub open: Option<NewOutage>,
}

/// Which outage ends, and when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutageEnd {
    /// The open outage's id.
    pub id: i64,
    /// When it ended, unix milliseconds UTC.
    pub ended_at_ms: i64,
}

/// How retention deletes old results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    /// Whole UTC days of raw results kept before today.
    pub days: u32,
    /// Most rows one DELETE removes, so queued writes never wait long.
    pub chunk_rows: u32,
    /// Pause between chunks, letting queued writes through.
    pub pause: Duration,
}

impl Retention {
    /// Keeps `days` days, deleting in chunks of 2000 rows 10 ms apart.
    #[must_use]
    pub const fn days(days: u32) -> Self {
        Retention {
            days,
            chunk_rows: 2000,
            pause: Duration::from_millis(10),
        }
    }
}

/// What a rollup recompute did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollupReport {
    /// The first day recomputed, or `None` if there was nothing to do. Later
    /// than the day asked for when retention has already finalized it.
    pub since: Option<Day>,
    /// Rollup rows written.
    pub rows: usize,
}

/// What one retention pass did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionReport {
    /// Results started before this instant are gone (unix milliseconds,
    /// always a UTC day boundary), or `None` if nothing is old enough yet.
    pub purged_before_ms: Option<i64>,
    /// Rollup rows finalized before deleting.
    pub rolled_up: usize,
    /// Results deleted.
    pub deleted: usize,
    /// DELETE statements run.
    pub chunks: u32,
}

/// Sizes, counts and the time span of stored results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbInfo {
    /// The database file.
    pub path: PathBuf,
    /// Bytes on disk: the database file plus its write-ahead log.
    pub size_bytes: u64,
    /// The schema version (number of migrations applied). It can be above
    /// [`latest_schema_version`] when a newer build wrote the database.
    pub schema_version: i64,
    /// Row count per table, in [`TABLES`] order. A table this database does
    /// not have (its schema is older or newer than this build's) is left out.
    pub rows: Vec<(&'static str, i64)>,
    /// When the oldest stored result started, unix milliseconds UTC.
    pub oldest_result_ms: Option<i64>,
    /// When the newest stored result started, unix milliseconds UTC.
    pub newest_result_ms: Option<i64>,
    /// The retention watermark; see [`RetentionReport::purged_before_ms`].
    pub purged_before_ms: Option<i64>,
}

/// What a vacuum reclaimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VacuumReport {
    /// Bytes on disk before.
    pub size_before: u64,
    /// Bytes on disk after.
    pub size_after: u64,
}

/// An open history database.
pub struct Store {
    path: PathBuf,
    writer: Writer,
    readers: Arc<Readers>,
    checkpointer: Checkpointer,
}

impl Store {
    /// Opens (creating it if missing) the database at `path`, switches it to
    /// WAL and runs pending migrations. The caller must be inside a tokio
    /// runtime to use the returned store.
    ///
    /// # Errors
    ///
    /// The file cannot be opened, is not a tocsin database, cannot use WAL,
    /// or has a schema newer than this build knows.
    pub fn open(path: &Path) -> Result<Store> {
        let conn = schema::open_writer(path)?;
        let readers = Readers::open(path, READERS)?;
        let (checkpointer, nudge) = Checkpointer::spawn(schema::open_checkpointer(path)?)?;
        let writer = Writer::spawn(conn, nudge)?;
        Ok(Store {
            path: path.to_owned(),
            writer,
            readers,
            checkpointer,
        })
    }

    /// The database file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The database's size on disk now, in bytes: the file and its
    /// write-ahead log. Reads file metadata only, never the database.
    ///
    /// # Errors
    ///
    /// Either file's metadata cannot be read; a missing log counts as
    /// empty.
    pub fn size_on_disk(&self) -> std::io::Result<u64> {
        size_on_disk(&self.path)
    }

    /// How many writes (inserts and write jobs) have been queued to the
    /// writer since the store opened, for tests and diagnostics: a caller
    /// that should not write can be checked for it.
    #[must_use]
    pub fn writes_queued(&self) -> u64 {
        self.writer.queued()
    }

    /// What the writer and the checkpointer have done since the store
    /// opened to keep the write-ahead log bounded, for the write-latency
    /// bench and diagnostics: a slow write can be traced to the path that
    /// made it slow.
    #[must_use]
    pub fn checkpoint_stats(&self) -> CheckpointStats {
        self.checkpointer.stats()
    }

    /// Appends a check result; resolves with its id once committed.
    ///
    /// # Errors
    ///
    /// The result starts more than [`FUTURE_TOLERANCE`] after now
    /// ([`Error::FutureResult`]) or on a day retention has already finalized,
    /// a value breaks a constraint (an empty id, a negative time), or the
    /// write fails.
    pub async fn append(&self, result: NewCheckResult) -> Result<i64> {
        not_future(&result)?;
        let insert = Insert::Record {
            result,
            kind: SubjectKind::Check,
            change: None,
        };
        self.writer.insert(insert).await
    }

    /// Stores a result of `kind` and the outage change it confirms, if any,
    /// in one transaction: both are kept or neither is. Resolves with the
    /// opened outage's id, if the change opened one.
    ///
    /// A check's result that changes nothing resolves once queued, before
    /// it commits: it is not durable yet, and a database error is logged
    /// and counted ([`CheckpointStats::rejected`]) instead of returned. A
    /// full queue still makes it wait. Every other record resolves once
    /// committed (DESIGN.md, "Durability").
    ///
    /// # Errors
    ///
    /// The result fails as [`Store::append`] would, the change as
    /// [`Store::change_outage`] would, or the write fails; nothing is
    /// written then. For a check's result with no change, only a result
    /// from the future and a closed store are reported.
    pub async fn record(
        &self,
        result: NewCheckResult,
        kind: SubjectKind,
        change: Option<OutageChange>,
    ) -> Result<Option<i64>> {
        not_future(&result)?;
        if kind == SubjectKind::Check && change.is_none() {
            self.writer
                .queue(Insert::Record {
                    result,
                    kind,
                    change,
                })
                .await?;
            return Ok(None);
        }
        let opens = change.as_ref().is_some_and(|change| change.open.is_some());
        let id = self
            .writer
            .insert(Insert::Record {
                result,
                kind,
                change,
            })
            .await?;
        Ok(opens.then_some(id))
    }

    /// Records the start of an outage; resolves with its id.
    ///
    /// # Errors
    ///
    /// The check already has an open outage on that node, the health is
    /// `Up`, or the write fails.
    pub async fn open_outage(&self, outage: NewOutage) -> Result<i64> {
        let change = OutageChange {
            end: None,
            open: Some(outage),
        };
        self.writer.insert(Insert::ChangeOutage(change)).await
    }

    /// Ends an open outage. This is the only change an outage ever receives.
    ///
    /// # Errors
    ///
    /// No outage has that id, it has already ended, it ends before it
    /// started, or the write fails.
    pub async fn end_outage(&self, id: i64, ended_at_ms: i64) -> Result<()> {
        let change = OutageChange {
            end: Some(OutageEnd { id, ended_at_ms }),
            open: None,
        };
        self.writer
            .insert(Insert::ChangeOutage(change))
            .await
            .map(drop)
    }

    /// Ends one outage and opens the next in one transaction, so a check's
    /// outage record never ends without its successor or opens beside the
    /// one it replaces. Resolves with the opened outage's id, if it opened
    /// one.
    ///
    /// # Errors
    ///
    /// Either half fails as [`Store::end_outage`] or [`Store::open_outage`]
    /// would, or the write fails; nothing is written then.
    pub async fn change_outage(&self, change: OutageChange) -> Result<Option<i64>> {
        let opens = change.open.is_some();
        let id = self.writer.insert(Insert::ChangeOutage(change)).await?;
        Ok(opens.then_some(id))
    }

    /// Changes an outage as [`Store::change_outage`] does and, in the same
    /// transaction, queues the alerting deliveries the change causes: a fire
    /// for every alert already firing for the identity and for every target
    /// in `routing` (given when the engine decided the change alerts), or on
    /// a recovery, a resolve for every firing alert; an alert that rang and
    /// stops ringing gets a cancel as well.
    ///
    /// # Errors
    ///
    /// The change fails as [`Store::change_outage`] would, or the write
    /// fails; nothing is written then.
    pub async fn change_outage_alerting(
        &self,
        change: OutageChange,
        routing: Option<AlertRouting>,
        now_ms: i64,
    ) -> Result<OutageAlerts> {
        self.writer
            .run(move |conn| alert::change_outage(conn, &change, routing.as_ref(), now_ms))
            .await
    }

    /// Stores a result of `kind` and the outage change it confirms as
    /// [`Store::record`] does and, in the same transaction, queues the
    /// alerting deliveries the change causes as
    /// [`Store::change_outage_alerting`] does.
    ///
    /// # Errors
    ///
    /// The result fails as [`Store::append`] would, the change as
    /// [`Store::change_outage`] would, or the write fails; nothing is
    /// written then.
    pub async fn record_alerting(
        &self,
        result: NewCheckResult,
        kind: SubjectKind,
        change: OutageChange,
        routing: Option<AlertRouting>,
        now_ms: i64,
    ) -> Result<OutageAlerts> {
        not_future(&result)?;
        self.writer
            .run(move |conn| alert::record(conn, &result, kind, &change, routing.as_ref(), now_ms))
            .await
    }

    /// Withdraws outage `id`, whose check or heartbeat is no longer
    /// configured: ends it at `now_ms` (or its start, if later) and, in the
    /// same transaction, ends every alert firing for its identity without a
    /// resolve, since nobody saw it recover. An alert that rang gets a
    /// cancel, so it stops ringing. Withdrawing an outage that is not open
    /// does nothing. Resolves with the deliveries queued.
    ///
    /// # Errors
    ///
    /// The write fails; nothing is written then.
    pub async fn withdraw_outage(&self, id: i64, now_ms: i64) -> Result<usize> {
        self.writer
            .run(move |conn| alert::withdraw_outage(conn, id, now_ms))
            .await
    }

    /// Ends every open alert whose notifier is not among `notifiers` at
    /// `now_ms`, queueing nothing, since no notifier is left to send
    /// through. Deliveries already queued for those alerts are abandoned
    /// by the worker as they come due, so they still count in its metrics.
    /// Resolves with how many alerts it ended.
    ///
    /// # Errors
    ///
    /// The write fails; nothing is written then.
    pub async fn end_alerts_without_notifier(
        &self,
        notifiers: Vec<String>,
        now_ms: i64,
    ) -> Result<usize> {
        self.writer
            .run(move |conn| alert::end_alerts_without_notifier(conn, &notifiers, now_ms))
            .await
    }

    /// The head of every alert's pending deliveries, and the next repeat
    /// due.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn outbox(&self) -> Result<Outbox> {
        self.read(alert::outbox).await
    }

    /// Queues a repeat for every firing alert that is due one and has
    /// nothing pending. Resolves with how many it queued.
    ///
    /// # Errors
    ///
    /// The write fails.
    pub async fn queue_repeats(&self, now_ms: i64) -> Result<usize> {
        self.writer
            .run(move |conn| alert::queue_repeats(conn, now_ms))
            .await
    }

    /// Records how an attempt to send a delivery ended.
    ///
    /// # Errors
    ///
    /// The write fails.
    pub async fn finish_delivery(
        &self,
        delivery_id: i64,
        outcome: DeliveryOutcome,
        now_ms: i64,
    ) -> Result<()> {
        self.writer
            .run(move |conn| alert::finish(conn, delivery_id, &outcome, now_ms))
            .await
    }

    /// Records a silence, made by `actor` at `at` to last until
    /// `expires_at`. The caller has validated `new`, resolved its expiry,
    /// and scoped it to its episode, if it has one.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] when the episode's outage has ended or does not
    /// exist; a value breaks a constraint, or the write fails.
    pub async fn create_silence(
        &self,
        new: NewSilence,
        expires_at: Timestamp,
        actor: Actor,
        at: Timestamp,
    ) -> Result<Silence> {
        self.writer
            .run(move |conn| silence::insert(conn, &new, expires_at, &actor, at))
            .await
    }

    /// Silence `id` as of `now`, with the messages it held back.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn silence(&self, id: i64, now: Timestamp) -> Result<Option<Silence>> {
        self.read(move |conn| silence::load(conn, id, now)).await
    }

    /// Up to `limit` silences as of `now`, newest first; with `before`,
    /// only those with a lower id.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn silences(
        &self,
        before: Option<i64>,
        limit: u32,
        now: Timestamp,
    ) -> Result<Vec<Silence>> {
        self.read(move |conn| silence::list(conn, before, limit, now))
            .await
    }

    /// Ends silence `id` early, by `actor` at `at`, and reads it back. A
    /// silence already over is left as it is. `None` if there is no such
    /// silence.
    ///
    /// # Errors
    ///
    /// The write fails.
    pub async fn end_silence(
        &self,
        id: i64,
        actor: Actor,
        at: Timestamp,
    ) -> Result<Option<Silence>> {
        self.writer
            .run(move |conn| silence::end(conn, id, &actor, at))
            .await
    }

    /// Every silence that applies at `now`, by id.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn active_silences(&self, now: Timestamp) -> Result<Vec<ActiveSilence>> {
        self.read(move |conn| silence::active(conn, now)).await
    }

    /// Holds back pending fire or repeat `delivery_id` for silence
    /// `silence_id` instead of sending it. `false` when it no longer may
    /// be: the delivery is no longer pending or is not a fire or repeat,
    /// the silence has stopped applying, or the silence is scoped to an
    /// outage other than the one the alert fired for.
    ///
    /// # Errors
    ///
    /// The write fails.
    pub async fn hold_delivery(
        &self,
        delivery_id: i64,
        silence_id: i64,
        now: Timestamp,
    ) -> Result<bool> {
        self.writer
            .run(move |conn| silence::hold(conn, delivery_id, silence_id, now))
            .await
    }

    /// Holds back pending resolve `delivery_id` if the receiver cannot
    /// have heard of its alert: no send of a fire or repeat was ever
    /// attempted and the latest was held back by a silence. The silence it
    /// was held under, or `None` when it must be sent.
    ///
    /// # Errors
    ///
    /// The write fails.
    pub async fn hold_resolve(&self, delivery_id: i64, now: Timestamp) -> Result<Option<i64>> {
        self.writer
            .run(move |conn| silence::hold_resolve(conn, delivery_id, now))
            .await
    }

    /// Marks alert `alert_id` attempted before a send of its fire or
    /// repeat starts: from then on the receiver may have heard of it.
    ///
    /// # Errors
    ///
    /// The write fails.
    pub async fn attempt_alert(&self, alert_id: i64) -> Result<()> {
        self.writer
            .run(move |conn| alert::attempt(conn, alert_id))
            .await
    }

    /// Drops pending cancel `delivery_id` if no send of its alert's fire
    /// or repeats was ever attempted: nothing rang. Whether it was dropped.
    ///
    /// # Errors
    ///
    /// The write fails.
    pub async fn drop_cancel(&self, delivery_id: i64) -> Result<bool> {
        self.writer
            .run(move |conn| alert::drop_cancel(conn, delivery_id))
            .await
    }

    /// Every open alert a silence held back that nobody has heard of and
    /// that has no message pending. One no silence applies to any more is
    /// fired afresh with [`Store::refire`].
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn stranded_alerts(&self) -> Result<Vec<StoredAlert>> {
        self.read(alert::stranded).await
    }

    /// Queues a fresh fire for alert `alert_id` at `now_ms` if it is still
    /// stranded. Whether it queued one.
    ///
    /// # Errors
    ///
    /// The write fails.
    pub async fn refire(&self, alert_id: i64, now_ms: i64) -> Result<bool> {
        self.writer
            .run(move |conn| alert::refire(conn, alert_id, now_ms))
            .await
    }

    /// Outage `id`, if there is one.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn outage(&self, id: i64) -> Result<Option<OutageRecord>> {
        self.read(move |conn| history::outage(conn, id)).await
    }

    /// Deletes finished deliveries older than `retention` allows, then
    /// ended alerts left with none. Both deletes run in chunks of
    /// `retention.chunk_rows` through the writer, `retention.pause` apart,
    /// so writes queued meanwhile go through between chunks.
    ///
    /// # Errors
    ///
    /// A write fails. A pass that fails part-way is finished by the next
    /// one.
    pub async fn prune_deliveries(
        &self,
        retention: DeliveryRetention,
        now_ms: i64,
    ) -> Result<DeliveryPrune> {
        let limit = retention.chunk_rows.max(1);
        let full = usize::try_from(limit).unwrap_or(usize::MAX);
        let mut report = DeliveryPrune::default();
        loop {
            let removed = self
                .writer
                .run(move |conn| alert::prune_deliveries_chunk(conn, retention, now_ms, limit))
                .await?;
            if removed > 0 {
                report.deliveries += removed;
                report.chunks += 1;
            }
            if removed < full {
                break;
            }
            tokio::time::sleep(retention.pause).await;
        }
        loop {
            let removed = self
                .writer
                .run(move |conn| alert::prune_alerts_chunk(conn, limit))
                .await?;
            if removed > 0 {
                report.alerts += removed;
                report.chunks += 1;
            }
            if removed < full {
                break;
            }
            tokio::time::sleep(retention.pause).await;
        }
        Ok(report)
    }

    /// Deliveries that failed for good, newest first.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn failed_deliveries(&self, limit: u32) -> Result<Vec<FailedDelivery>> {
        self.read(move |conn| alert::failed(conn, limit)).await
    }

    /// Every stored alert, oldest first.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn alerts(&self) -> Result<Vec<StoredAlert>> {
        self.read(alert::alerts).await
    }

    /// Opens an incident (or announces maintenance) with its first update,
    /// written by `actor` at `at`: a draft when `new.draft` is `Some(true)`,
    /// otherwise published by `actor` at once. The caller has validated
    /// `new`, including that its components are configured, and that
    /// `actor` may publish.
    ///
    /// # Errors
    ///
    /// A value breaks a constraint, or the write fails.
    pub async fn create_incident(
        &self,
        new: NewIncident,
        actor: Actor,
        at: Timestamp,
    ) -> Result<Incident> {
        Ok(self.open_incident(new, actor, at).await?.0)
    }

    /// [`Store::create_incident`], except that when an open incident
    /// already holds `new.external_key` nothing is written and that
    /// incident is answered, with [`Opened::Existing`]. A resolved
    /// incident has freed its key, so the same key then opens a new one:
    /// the trouble came back as a new episode. The lookup and the insert share
    /// one IMMEDIATE transaction, so two creates with one key cannot both
    /// insert.
    ///
    /// # Errors
    ///
    /// A value breaks a constraint, or the write fails.
    pub async fn open_incident(
        &self,
        new: NewIncident,
        actor: Actor,
        at: Timestamp,
    ) -> Result<(Incident, Opened)> {
        self.writer
            .run(move |conn| {
                let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let existing = match &new.external_key {
                    Some(key) => incident::keyed(&transaction, key)?,
                    None => None,
                };
                let (id, opened) = match existing {
                    Some(id) => (id, Opened::Existing),
                    None => (
                        incident::insert(&transaction, &new, &actor, at)?,
                        Opened::Created,
                    ),
                };
                let incident = incident::load(&transaction, id)?;
                transaction.commit()?;
                // The row was found or inserted by this transaction, so it
                // is there.
                incident
                    .map(|incident| (incident, opened))
                    .ok_or(Error::Sqlite(rusqlite::Error::QueryReturnedNoRows))
            })
            .await
    }

    /// Appends an update to incident `id` and moves it to the update's
    /// status. `None` if no incident has that id. With `drafts_only`, only
    /// a draft takes it.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] if the status does not suit the incident's kind,
    /// or it reopens a resolved incident whose external key another open
    /// incident now holds; [`Error::Published`] if `drafts_only` and it is
    /// published; otherwise the write fails.
    pub async fn add_incident_update(
        &self,
        id: i64,
        update: NewUpdate,
        drafts_only: bool,
        actor: Actor,
        at: Timestamp,
    ) -> Result<Option<Incident>> {
        self.writer
            .run(move |conn| {
                let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                if incident::add_update(&transaction, id, &update, drafts_only, &actor, at)?
                    .is_none()
                {
                    return Ok(None);
                }
                let incident = incident::load(&transaction, id)?;
                transaction.commit()?;
                Ok(incident)
            })
            .await
    }

    /// Changes incident `id`'s title, impact, components or schedule, and
    /// logs the edit with its author. `None` if no incident has that id.
    /// With `drafts_only`, only a draft takes it.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] if the result would break a rule (a schedule on
    /// an incident, a window ending before it starts);
    /// [`Error::Published`] if `drafts_only` and it is published;
    /// otherwise the write fails.
    pub async fn edit_incident(
        &self,
        id: i64,
        edit: IncidentEdit,
        drafts_only: bool,
        actor: Actor,
        at: Timestamp,
    ) -> Result<Option<Incident>> {
        self.writer
            .run(move |conn| {
                let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                if incident::edit(&transaction, id, &edit, drafts_only, &actor, at)?.is_none() {
                    return Ok(None);
                }
                let incident = incident::load(&transaction, id)?;
                transaction.commit()?;
                Ok(incident)
            })
            .await
    }

    /// Publishes draft incident `id`: `actor` at `at`. `None` if no
    /// incident has that id.
    ///
    /// # Errors
    ///
    /// [`Error::Published`] if it already is; otherwise the write fails.
    pub async fn publish_incident(
        &self,
        id: i64,
        actor: Actor,
        at: Timestamp,
    ) -> Result<Option<Incident>> {
        self.writer
            .run(move |conn| {
                let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                if incident::publish(&transaction, id, &actor, at)?.is_none() {
                    return Ok(None);
                }
                let incident = incident::load(&transaction, id)?;
                transaction.commit()?;
                Ok(incident)
            })
            .await
    }

    /// Deletes draft incident `id` with its updates, and returns it as it
    /// was. `None` if no incident has that id.
    ///
    /// # Errors
    ///
    /// [`Error::Published`] if it is published, which is never deleted;
    /// otherwise the write fails.
    pub async fn discard_incident(&self, id: i64) -> Result<Option<Incident>> {
        self.writer
            .run(move |conn| {
                let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let incident = incident::discard(&transaction, id)?;
                transaction.commit()?;
                Ok(incident)
            })
            .await
    }

    /// Publishes a release, or updates the one with the same project,
    /// version and environment, written by `actor` at `at`. The caller has
    /// validated `new`, including that its project is configured.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] if `new.released_at` is before the recorded one,
    /// or either time is more than [`FUTURE_TOLERANCE`] past `at`;
    /// otherwise a value breaks a constraint, or the write fails.
    pub async fn publish_release(
        &self,
        new: NewRelease,
        actor: Actor,
        at: Timestamp,
    ) -> Result<(Release, Written)> {
        self.writer
            .run(move |conn| {
                let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let (id, written) = release::publish(&transaction, &new, &actor, at)?;
                let release = release::load(&transaction, id)?;
                transaction.commit()?;
                // The row was written by this transaction, so it is there.
                let release = release.ok_or(Error::Sqlite(rusqlite::Error::QueryReturnedNoRows))?;
                Ok((release, written))
            })
            .await
    }

    /// Records that `version` of `project` was deployed, creating a stub
    /// release for a later publish to fill in if there is none. The caller
    /// has validated the stamp, the project and the version.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] if the stamp is more than [`FUTURE_TOLERANCE`]
    /// past `at`; otherwise a value breaks a constraint, or the write
    /// fails.
    pub async fn stamp_deploy(
        &self,
        project: String,
        version: String,
        stamp: DeployStamp,
        actor: Actor,
        at: Timestamp,
    ) -> Result<(Release, Written)> {
        self.writer
            .run(move |conn| {
                let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let (id, written) =
                    release::stamp(&transaction, &project, &version, &stamp, &actor, at)?;
                let release = release::load(&transaction, id)?;
                transaction.commit()?;
                let release = release.ok_or(Error::Sqlite(rusqlite::Error::QueryReturnedNoRows))?;
                Ok((release, written))
            })
            .await
    }

    /// Published releases matching `query`, newest first, and whether more
    /// matched than its limit.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn releases(&self, query: ReleaseQuery) -> Result<(Vec<Release>, bool)> {
        self.read(move |conn| release::list(conn, &query)).await
    }

    /// One release per project and version matching `query`, newest
    /// first, and whether more matched than its limit. The production
    /// release stands for the version if it was published there, else the
    /// first published; its `released_at` is the version's first publish
    /// in any environment.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn release_versions(&self, query: ReleaseQuery) -> Result<(Vec<Release>, bool)> {
        self.read(move |conn| release::versions(conn, &query)).await
    }

    /// Every environment's row for `project` `version` that `visible`
    /// allows, deploy stamps included; production first.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn release_environments(
        &self,
        visible: Projects,
        project: String,
        version: String,
    ) -> Result<Vec<Release>> {
        self.read(move |conn| release::environments(conn, &visible, &project, &version))
            .await
    }

    /// Outages, incidents, deploys and releases matching `query`, in its
    /// order, read from one snapshot.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn timeline(&self, query: TimelineQuery) -> Result<Timeline> {
        self.read(move |conn| release::timeline(conn, &query)).await
    }

    /// The incident with that id, with its components and updates.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn incident(&self, id: i64) -> Result<Option<Incident>> {
        self.read(move |conn| incident::load(conn, id)).await
    }

    /// Up to `limit` incidents given external key `key`, newest first,
    /// drafts and resolved ones included: at most one is open, and each
    /// resolved one is an earlier episode. With `before`, only those with
    /// a lower id, as [`Store::incidents`] pages.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn incidents_keyed(
        &self,
        key: String,
        before: Option<i64>,
        limit: u32,
    ) -> Result<Vec<Incident>> {
        self.read(move |conn| incident::all_keyed(conn, &key, before, limit))
            .await
    }

    /// The internal note and external key of each of `ids` that exists.
    /// Every other incident read leaves them out; only a caller trusted
    /// with them asks.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn incident_internals(
        &self,
        ids: Vec<i64>,
    ) -> Result<std::collections::HashMap<i64, IncidentInternals>> {
        self.read(move |conn| incident::internals(conn, &ids)).await
    }

    /// Up to `limit` incidents, newest first; with `before`, only those
    /// with a lower id (the cursor for the next page).
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn incidents(
        &self,
        drafts: Drafts,
        before: Option<i64>,
        limit: u32,
    ) -> Result<Vec<Incident>> {
        self.read(move |conn| incident::list(conn, drafts, before, limit))
            .await
    }

    /// Every draft incident, resolved or not, newest first.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn draft_incidents(&self) -> Result<Vec<Incident>> {
        self.read(incident::drafts).await
    }

    /// Up to `limit` incidents and maintenance naming `[[component]]`
    /// `component`, newest first.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn incidents_naming(
        &self,
        drafts: Drafts,
        component: &str,
        limit: u32,
    ) -> Result<Vec<Incident>> {
        let component = component.to_owned();
        self.read(move |conn| incident::naming(conn, drafts, &component, limit))
            .await
    }

    /// Every published incident not yet resolved and every published
    /// maintenance not yet completed, oldest first: what the engine needs
    /// to derive component status. A draft changes no status. The schema keeps `resolved_at` set exactly when the status is
    /// `resolved` or `completed`.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn open_incidents(&self) -> Result<Vec<OpenIncident>> {
        self.read(incident::open).await
    }

    /// Every incident not yet resolved and every maintenance not yet
    /// completed, scheduled ones included, newest first, with components
    /// and updates: what a status page lists.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn unresolved_incidents(&self, drafts: Drafts) -> Result<Vec<Incident>> {
        self.read(move |conn| incident::unresolved(conn, drafts))
            .await
    }

    /// One page of `check_id`'s raw results, newest first.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] when `page.before` names no result of
    /// `check_id`, or the read fails.
    pub async fn check_results(
        &self,
        check_id: &str,
        page: HistoryPage,
    ) -> Result<Vec<CheckResult>> {
        let check_id = check_id.to_owned();
        self.read(move |conn| history::results(conn, &check_id, page))
            .await
    }

    /// For each of `queries`, in order, the newest result it asks for.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn newest_results(&self, queries: Vec<Newest>) -> Result<Vec<Option<CheckResult>>> {
        self.read(move |conn| history::newest(conn, &queries)).await
    }

    /// The results since `since` of the check or heartbeat `check`, or of
    /// every one when `None`, in slices `width_ms` wide, ordered by check
    /// and slice.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn result_slices(
        &self,
        since: Timestamp,
        width_ms: i64,
        check: Option<String>,
    ) -> Result<Vec<ResultSlice>> {
        self.read(move |conn| history::slices(conn, since, width_ms, check.as_deref()))
            .await
    }

    /// One page of the outages open at any moment since `page.since`,
    /// newest start first.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] when `page.before` names no outage of `shown`,
    /// or the read fails.
    pub async fn outages(&self, page: HistoryPage, shown: Checks) -> Result<Vec<OutageRecord>> {
        self.read(move |conn| history::outages(conn, page, &shown))
            .await
    }

    /// For each of `check_ids` that `node_id` has an `up` result for, when
    /// it was last up and since when.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn last_up(&self, node_id: &str, check_ids: Vec<String>) -> Result<Vec<LastUp>> {
        let node_id = node_id.to_owned();
        self.read(move |conn| outage::last_up(conn, &node_id, &check_ids))
            .await
    }

    /// For each of `check_ids` that `node_id` has any result for, when its
    /// latest result started. Heartbeats resume their deadline from it.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn latest_results(
        &self,
        node_id: &str,
        check_ids: Vec<String>,
    ) -> Result<Vec<LatestResult>> {
        let node_id = node_id.to_owned();
        self.read(move |conn| outage::latest_results(conn, &node_id, &check_ids))
            .await
    }

    /// Every outage `node_id` has open, ordered by check.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn open_outages(&self, node_id: &str) -> Result<Vec<Outage>> {
        let node_id = node_id.to_owned();
        self.read(move |conn| outage::open(conn, &node_id)).await
    }

    /// Signs `signup.email` up, or refreshes the confirmation of an address
    /// that signed up but never confirmed. An address already confirmed is
    /// left as it is.
    ///
    /// # Errors
    ///
    /// The write fails.
    pub async fn sign_up(&self, signup: NewSignup) -> Result<Signup> {
        self.writer
            .run(move |conn| subscriber::sign_up(conn, &signup))
            .await
    }

    /// The unconfirmed subscriber whose confirmation token hashes to
    /// `hash`, if the token has not expired.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn subscriber_to_confirm(
        &self,
        hash: String,
        now_ms: i64,
    ) -> Result<Option<Subscriber>> {
        self.read(move |conn| subscriber::by_confirmation(conn, &hash, now_ms))
            .await
    }

    /// Confirms the subscriber whose unexpired confirmation token hashes to
    /// `hash`. Resolves with them, or `None` if no token matches.
    ///
    /// # Errors
    ///
    /// The write fails.
    pub async fn confirm_subscriber(
        &self,
        hash: String,
        now_ms: i64,
    ) -> Result<Option<Subscriber>> {
        self.writer
            .run(move |conn| subscriber::confirm(conn, &hash, now_ms))
            .await
    }

    /// The subscriber with this id.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn subscriber(&self, id: i64) -> Result<Option<Subscriber>> {
        self.read(move |conn| subscriber::by_id(conn, id)).await
    }

    /// Replaces what a subscriber hears about. Resolves with whether they
    /// exist.
    ///
    /// # Errors
    ///
    /// The write fails.
    pub async fn set_subscriber_preferences(
        &self,
        id: i64,
        preferences: Preferences,
    ) -> Result<bool> {
        self.writer
            .run(move |conn| subscriber::set_preferences(conn, id, &preferences))
            .await
    }

    /// Deletes a subscriber, with their subscriptions and every delivery
    /// still queued for them. Resolves with whether they existed.
    ///
    /// # Errors
    ///
    /// The write fails.
    pub async fn remove_subscriber(&self, id: i64) -> Result<bool> {
        self.writer
            .run(move |conn| subscriber::remove(conn, id))
            .await
    }

    /// Up to `limit` subscribers with ids after `after`, in id order.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn subscribers(&self, after: Option<i64>, limit: u32) -> Result<Vec<Subscriber>> {
        self.read(move |conn| subscriber::list(conn, after, limit))
            .await
    }

    /// How many subscribers are confirmed and how many wait.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn subscriber_counts(&self) -> Result<SubscriberCounts> {
        self.read(subscriber::counts).await
    }

    /// Queues an announcement for subscribers, unless one with the same key
    /// already was. Resolves with whether it was queued.
    ///
    /// # Errors
    ///
    /// The write fails.
    pub async fn queue_announcement(&self, announcement: NewAnnouncement) -> Result<bool> {
        self.writer
            .run(move |conn| subscriber::queue_announcement(conn, &announcement))
            .await
    }

    /// Queues a delivery of each new announcement to every confirmed
    /// subscriber it matches. Resolves with how many it queued.
    ///
    /// # Errors
    ///
    /// The write fails. An announcement whose transaction committed stays
    /// expanded.
    pub async fn expand_announcements(&self, now_ms: i64) -> Result<usize> {
        self.writer
            .run(move |conn| subscriber::expand(conn, now_ms))
            .await
    }

    /// Up to `limit` deliveries due at `now_ms`, and when the next is.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn mailbag(&self, now_ms: i64, limit: u32) -> Result<Mailbag> {
        self.read(move |conn| subscriber::mailbag(conn, Lane::Mail, now_ms, limit))
            .await
    }

    /// Marks a due delivery `sending`, committed before it returns, and
    /// resolves with what to send. `None` when it is no longer pending or
    /// its subscriber is gone or unconfirmed.
    ///
    /// # Errors
    ///
    /// The write fails.
    pub async fn claim_mail(&self, id: i64) -> Result<Option<ClaimedMail>> {
        self.writer
            .run(move |conn| subscriber::claim(conn, id))
            .await
    }

    /// Records how sending a claimed delivery ended.
    ///
    /// # Errors
    ///
    /// The write fails.
    pub async fn finish_mail(&self, id: i64, outcome: MailOutcome, now_ms: i64) -> Result<()> {
        self.writer
            .run(move |conn| subscriber::finish(conn, Lane::Mail, id, &outcome, now_ms))
            .await
    }

    /// Fails every delivery, email or Slack, a stopped process left
    /// `sending`, so none is sent twice. Resolves with how many.
    ///
    /// # Errors
    ///
    /// The write fails.
    pub async fn interrupt_mail(&self, now_ms: i64) -> Result<usize> {
        self.writer
            .run(move |conn| subscriber::interrupt(conn, now_ms))
            .await
    }

    /// Forgets unconfirmed subscribers whose link expired, and finished
    /// deliveries from before `finished_before_ms`.
    ///
    /// # Errors
    ///
    /// The write fails.
    pub async fn prune_subscribers(
        &self,
        now_ms: i64,
        finished_before_ms: i64,
    ) -> Result<SubscriberPrune> {
        self.writer
            .run(move |conn| subscriber::prune(conn, now_ms, finished_before_ms))
            .await
    }

    /// Stores a connection Slack approved; see [`NewSlackConnection`]. An
    /// operator connection replaces the one of the same name, and a
    /// subscriber connection any other to the same channel.
    ///
    /// # Errors
    ///
    /// The write fails.
    pub async fn connect_slack(&self, connection: NewSlackConnection) -> Result<SlackConnection> {
        self.writer
            .run(move |conn| slack::connect(conn, &connection))
            .await
    }

    /// The Slack connection with this id.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn slack_connection(&self, id: i64) -> Result<Option<SlackConnection>> {
        self.read(move |conn| slack::by_id(conn, id)).await
    }

    /// Every Slack connection for `purpose`, without webhook URLs.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn slack_connections(&self, purpose: SlackPurpose) -> Result<Vec<SlackConnection>> {
        self.read(move |conn| slack::list(conn, purpose)).await
    }

    /// The operator connection a notifier names.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn slack_hook(&self, name: String) -> Result<SlackHook> {
        self.read(move |conn| slack::hook(conn, &name)).await
    }

    /// How many Slack connections there are, live and dead, and each
    /// operator connection's name and whether it is live.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn slack_census(&self) -> Result<(SlackCounts, Vec<(String, bool)>)> {
        self.read(|conn| Ok((slack::counts(conn)?, slack::operator_states(conn)?)))
            .await
    }

    /// Marks a Slack connection dead with Slack's reason, and fails its
    /// waiting deliveries. Resolves with whether it was live.
    ///
    /// # Errors
    ///
    /// The write fails.
    pub async fn kill_slack(&self, id: i64, reason: String, now_ms: i64) -> Result<bool> {
        self.writer
            .run(move |conn| slack::kill(conn, id, &reason, now_ms))
            .await
    }

    /// Deletes a Slack connection, with its subscriptions and deliveries.
    /// Resolves with whether it existed.
    ///
    /// # Errors
    ///
    /// The write fails.
    pub async fn remove_slack(&self, id: i64) -> Result<bool> {
        self.writer.run(move |conn| slack::remove(conn, id)).await
    }

    /// Replaces what a subscriber Slack connection hears about. Resolves
    /// with whether it exists.
    ///
    /// # Errors
    ///
    /// The write fails.
    pub async fn set_slack_preferences(&self, id: i64, preferences: Preferences) -> Result<bool> {
        self.writer
            .run(move |conn| slack::set_preferences(conn, id, &preferences))
            .await
    }

    /// Up to `limit` Slack deliveries due at `now_ms`, and when the next is.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn slack_mailbag(&self, now_ms: i64, limit: u32) -> Result<Mailbag> {
        self.read(move |conn| subscriber::mailbag(conn, Lane::Slack, now_ms, limit))
            .await
    }

    /// Marks a due Slack delivery `sending`, committed before it returns,
    /// and resolves with what to post. `None` when it is no longer pending
    /// or its connection is gone or dead.
    ///
    /// # Errors
    ///
    /// The write fails.
    pub async fn claim_slack(&self, id: i64) -> Result<Option<ClaimedSlack>> {
        self.writer.run(move |conn| slack::claim(conn, id)).await
    }

    /// Records how posting a claimed Slack delivery ended.
    ///
    /// # Errors
    ///
    /// The write fails.
    pub async fn finish_slack(&self, id: i64, outcome: MailOutcome, now_ms: i64) -> Result<()> {
        self.writer
            .run(move |conn| subscriber::finish(conn, Lane::Slack, id, &outcome, now_ms))
            .await
    }

    /// Runs `query` on a pooled read connection. Read connections refuse
    /// writes.
    ///
    /// # Errors
    ///
    /// Whatever `query` returns, or [`Error::ReaderPanicked`].
    pub async fn read<T, F>(&self, query: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        self.readers.read(query).await
    }

    /// Stored rollups from `since` on, ordered by check, node and day.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn daily_rollups(&self, since: Day) -> Result<Vec<DailyRollup>> {
        self.read(move |conn| rollup::since(conn, since)).await
    }

    /// Recomputes daily rollups from raw results for every day from `since`
    /// on. Idempotent. Days retention has finalized are left as they are.
    ///
    /// # Errors
    ///
    /// A read or write fails.
    pub async fn rollup_since(&self, since: Day) -> Result<RollupReport> {
        self.recompute(Some(since)).await
    }

    /// Recomputes rollups from the latest day already rolled up (which may
    /// have been partial) to now. Cheap: normally one or two days.
    ///
    /// # Errors
    ///
    /// A read or write fails.
    pub async fn rollup_recent(&self) -> Result<RollupReport> {
        self.recompute(None).await
    }

    async fn recompute(&self, since: Option<Day>) -> Result<RollupReport> {
        let (since, rollups) = self
            .read(move |conn| {
                let now_ms = now_ms();
                let today = Day::containing(now_ms);
                let Some(asked) =
                    since.map_or_else(|| rollup::resume_day(conn, today), |day| Ok(Some(day)))?
                else {
                    return Ok((None, Vec::new()));
                };
                let floor = Day::containing(rollup::purged_before(conn)?.unwrap_or(0));
                let since = asked.max(floor);
                Ok((
                    Some(since),
                    rollup::aggregate(conn, since.start_ms(), i64::MAX, now_ms)?,
                ))
            })
            .await?;
        let rows = self
            .writer
            .run(move |conn| {
                // IMMEDIATE: the upsert reads the watermark before writing,
                // and a DEFERRED read-then-write fails with SQLITE_BUSY,
                // unretried, if another process (`tocsin db rollup` beside
                // `serve`) commits in between.
                let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let rows = rollup::upsert(&transaction, &rollups)?;
                transaction.commit()?;
                Ok(rows)
            })
            .await?;
        Ok(RollupReport { since, rows })
    }

    /// Deletes results older than `policy.days` whole UTC days before
    /// `now_ms`. Each day being dropped is counted on a reader, then its
    /// rollups are written in the write transaction that moves the watermark
    /// past it, recounting there if a result for it was committed in
    /// between, so the daily bar never loses a day or a result. The delete
    /// then runs in chunks through the writer, so writes queued meanwhile go
    /// through between chunks and readers never wait. Before each of its
    /// writes it lets the checkpointer copy a log past the soft limit, so
    /// the purge never carries the log to the hard limit, where a queued
    /// write would wait for the copy (TOC-62).
    ///
    /// # Errors
    ///
    /// A read or write fails. A pass that fails part-way is finished by the
    /// next one.
    pub async fn apply_retention(&self, policy: Retention, now_ms: i64) -> Result<RetentionReport> {
        let cutoff = Day::containing(now_ms)
            .plus(-i64::from(policy.days))
            .start_ms();
        let mut rolled_up = 0;
        // A cutoff before 1970 finalizes nothing: the watermark starts at 0.
        while let Some(plan) = self
            .read(move |conn| rollup::plan_finalize(conn, cutoff, now_ms))
            .await?
        {
            self.checkpointer.make_room().await;
            rolled_up += self
                .writer
                .run(move |conn| rollup::finalize(conn, plan))
                .await?;
        }
        // The watermark, which may be from an earlier pass that stopped
        // part-way through its delete; this one finishes it.
        let Some(target) = self.read(rollup::purged_before).await? else {
            return Ok(RetentionReport {
                purged_before_ms: None,
                rolled_up,
                deleted: 0,
                chunks: 0,
            });
        };
        let limit = policy.chunk_rows.max(1);
        let (mut deleted, mut chunks) = (0, 0);
        loop {
            self.checkpointer.make_room().await;
            let removed = self
                .writer
                .run(move |conn| rollup::delete_chunk(conn, target, limit))
                .await?;
            if removed == 0 {
                break;
            }
            deleted += removed;
            chunks += 1;
            if removed < usize::try_from(limit).unwrap_or(usize::MAX) {
                break;
            }
            tokio::time::sleep(policy.pause).await;
        }
        Ok(RetentionReport {
            purged_before_ms: Some(target),
            rolled_up,
            deleted,
            chunks,
        })
    }

    /// Reports file size, schema version, row counts and the span of stored
    /// results.
    ///
    /// # Errors
    ///
    /// A read fails or the file cannot be inspected.
    pub async fn info(&self) -> Result<DbInfo> {
        let path = self.path.clone();
        self.read(move |conn| describe(conn, path)).await
    }

    /// Rebuilds the database file to reclaim space freed by retention, then
    /// truncates the write-ahead log.
    ///
    /// # Errors
    ///
    /// SQLite cannot vacuum (for example, the disk is full).
    pub async fn vacuum(&self) -> Result<VacuumReport> {
        let path = self.path.clone();
        self.writer
            .run(move |conn| {
                let size_before = size_on_disk(&path)?;
                conn.execute_batch("VACUUM")?;
                conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
                Ok(VacuumReport {
                    size_before,
                    size_after: size_on_disk(&path)?,
                })
            })
            .await
    }

    /// Lets queued writes finish, then closes every connection.
    ///
    /// # Errors
    ///
    /// The writer thread ([`Error::WriterThreadPanicked`]) or the
    /// checkpointer thread ([`Error::CheckpointerPanicked`]) panicked.
    pub async fn close(self) -> Result<()> {
        let Store {
            writer,
            readers,
            checkpointer,
            ..
        } = self;
        drop(readers);
        let thread = writer.close();
        // The writer thread's end stops the checkpointer, whose connection
        // closes last and so checkpoints and removes the log.
        tokio::task::spawn_blocking(move || join(thread, checkpointer))
            .await
            .map_err(|_| Error::Closed)?
    }
}

/// Waits for the writer thread, then the checkpointer, and names the first
/// that panicked.
fn join(writer: std::thread::JoinHandle<()>, checkpointer: Checkpointer) -> Result<()> {
    let writer = writer.join();
    let checkpointer = checkpointer.join();
    writer.map_err(|_| Error::WriterThreadPanicked)?;
    checkpointer.map_err(|_| Error::CheckpointerPanicked)
}

/// Refuses a result that starts more than [`FUTURE_TOLERANCE`] after now.
fn not_future(result: &NewCheckResult) -> Result<()> {
    let now_ms = now_ms();
    let tolerance_ms = i64::try_from(FUTURE_TOLERANCE.as_millis()).unwrap_or(i64::MAX);
    if result.started_at_ms > now_ms.saturating_add(tolerance_ms) {
        return Err(Error::FutureResult {
            started_at_ms: result.started_at_ms,
            now_ms,
        });
    }
    Ok(())
}

/// The current time as unix milliseconds, UTC.
#[must_use]
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

/// The schema version this build migrates to.
#[must_use]
pub fn latest_schema_version() -> usize {
    schema::latest_version()
}

/// Describes the database at `path` without changing it: it is opened read
/// only, nothing is migrated, and no writer starts. A schema newer than this
/// build's is reported, not refused, which is what `tocsin db info` needs to
/// say "upgrade the binary" rather than fail.
///
/// # Errors
///
/// The file is not a SQLite database, or cannot be read.
pub fn inspect(path: &Path) -> Result<DbInfo> {
    let conn = schema::open_inspector(path)?;
    describe(&conn, path.to_path_buf())
}

/// Every operator Slack connection's name and whether it is live, read
/// from the database at `path` without writing or migrating it, for
/// `tocsin config check`. `None` when the database predates Slack
/// connections.
///
/// # Errors
///
/// The file is not a SQLite database, or cannot be read.
pub fn inspect_slack(path: &Path) -> Result<Option<Vec<(String, bool)>>> {
    let conn = schema::open_inspector(path)?;
    if !has_slack(&conn)? {
        return Ok(None);
    }
    slack::operator_states(&conn).map(Some)
}

/// The operator Slack connection `name`, read from the database at `path`
/// without writing or migrating it, for `tocsin notify test`.
///
/// # Errors
///
/// The file is not a SQLite database, or cannot be read.
pub fn inspect_slack_hook(path: &Path, name: &str) -> Result<SlackHook> {
    let conn = schema::open_inspector(path)?;
    if !has_slack(&conn)? {
        return Ok(SlackHook::Missing);
    }
    slack::hook(&conn, name)
}

fn has_slack(conn: &Connection) -> Result<bool> {
    Ok(conn
        .prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'slack_connection'")?
        .exists([])?)
}

/// The shared body of [`Store::info`] and [`inspect`]. Only tables present
/// in `sqlite_master` are read, so a schema from another version works too.
fn describe(conn: &Connection, path: PathBuf) -> Result<DbInfo> {
    let schema_version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let mut present =
        conn.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1")?;
    let mut rows = Vec::new();
    for table in TABLES {
        if present.exists([table])? {
            let count = conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })?;
            rows.push((table, count));
        }
    }
    let has = |table: &str| rows.iter().any(|(name, _)| *name == table);
    let (oldest_result_ms, newest_result_ms) = if has("check_result") {
        conn.query_row(
            "SELECT min(started_at), max(started_at) FROM check_result",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?
    } else {
        (None, None)
    };
    let purged_before_ms = if has("retention") {
        rollup::purged_before(conn)?
    } else {
        None
    };
    Ok(DbInfo {
        size_bytes: size_on_disk(&path)?,
        path,
        schema_version,
        rows,
        oldest_result_ms,
        newest_result_ms,
        purged_before_ms,
    })
}

fn size_on_disk(path: &Path) -> std::io::Result<u64> {
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    let wal = match std::fs::metadata(wal) {
        Ok(meta) => meta.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => return Err(error),
    };
    Ok(std::fs::metadata(path)?.len() + wal)
}
