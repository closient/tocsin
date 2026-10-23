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
mod incident;
mod outage;
mod readers;
mod release;
mod rollup;
mod schema;
#[cfg(test)]
mod tests;
mod writer;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, TransactionBehavior};
use tocsin_core::Health;
use tocsin_core::incident::{Actor, Incident, IncidentEdit, NewIncident, NewUpdate, Timestamp};
use tocsin_core::release::{DeployStamp, NewRelease, Release, Timeline};

pub use crate::alert::{
    AlertRouting, AlertTarget, DeliveryKind, DeliveryOutcome, DeliveryPrune, DeliveryRetention,
    FailedDelivery, OutageAlerts, Outbox, PendingDelivery, StoredAlert, SubjectKind,
};
use crate::checkpoint::Checkpointer;
pub use crate::day::Day;
pub use crate::error::{Error, Result};
pub use crate::incident::OpenIncident;
pub use crate::outage::{LastUp, Outage};
use crate::readers::Readers;
pub use crate::release::{ComponentScope, Order, Projects, ReleaseQuery, TimelineQuery, Written};
pub use crate::rollup::DailyRollup;
use crate::writer::{Insert, Writer};

/// The database file's name inside the data folder.
pub const DB_FILE: &str = "tocsin.db";

/// Read connections in the pool.
const READERS: usize = 4;

/// How far after this node's clock a check result may start: room for clock
/// skew between nodes, not for a clock that is simply wrong.
pub const FUTURE_TOLERANCE: Duration = Duration::from_hours(1);

/// The tables `tocsin db info` counts, in schema order.
pub const TABLES: [&str; 12] = [
    "check_result",
    "outage",
    "daily_rollup",
    "heartbeat_ping",
    "incident",
    "incident_component",
    "incident_update",
    "incident_edit",
    "release",
    "retention",
    "alert",
    "alert_delivery",
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

    /// How many writes (inserts and write jobs) have been queued to the
    /// writer since the store opened, for tests and diagnostics: a caller
    /// that should not write can be checked for it.
    #[must_use]
    pub fn writes_queued(&self) -> u64 {
        self.writer.queued()
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
        let now_ms = now_ms();
        let tolerance_ms = i64::try_from(FUTURE_TOLERANCE.as_millis()).unwrap_or(i64::MAX);
        if result.started_at_ms > now_ms.saturating_add(tolerance_ms) {
            return Err(Error::FutureResult {
                started_at_ms: result.started_at_ms,
                now_ms,
            });
        }
        self.writer.insert(Insert::CheckResult(result)).await
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
    /// written by `actor` at `at`. The caller has validated `new`, including
    /// that its components are configured.
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
        self.writer
            .run(move |conn| {
                let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let id = incident::insert(&transaction, &new, &actor, at)?;
                let incident = incident::load(&transaction, id)?;
                transaction.commit()?;
                // The row was inserted by this transaction, so it is there.
                incident.ok_or(Error::Sqlite(rusqlite::Error::QueryReturnedNoRows))
            })
            .await
    }

    /// Appends an update to incident `id` and moves it to the update's
    /// status. `None` if no incident has that id.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] if the status does not suit the incident's kind;
    /// otherwise the write fails.
    pub async fn add_incident_update(
        &self,
        id: i64,
        update: NewUpdate,
        actor: Actor,
        at: Timestamp,
    ) -> Result<Option<Incident>> {
        self.writer
            .run(move |conn| {
                let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                if incident::add_update(&transaction, id, &update, &actor, at)?.is_none() {
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
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] if the result would break a rule (a schedule on
    /// an incident, a window ending before it starts); otherwise the write
    /// fails.
    pub async fn edit_incident(
        &self,
        id: i64,
        edit: IncidentEdit,
        actor: Actor,
        at: Timestamp,
    ) -> Result<Option<Incident>> {
        self.writer
            .run(move |conn| {
                let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                if incident::edit(&transaction, id, &edit, &actor, at)?.is_none() {
                    return Ok(None);
                }
                let incident = incident::load(&transaction, id)?;
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

    /// Up to `limit` incidents, newest first; with `before`, only those
    /// with a lower id (the cursor for the next page).
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn incidents(&self, before: Option<i64>, limit: u32) -> Result<Vec<Incident>> {
        self.read(move |conn| incident::list(conn, before, limit))
            .await
    }

    /// Every incident not yet resolved and every maintenance not yet
    /// completed, oldest first: what the engine needs to derive component
    /// status. The schema keeps `resolved_at` set exactly when the status is
    /// `resolved` or `completed`.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn open_incidents(&self) -> Result<Vec<OpenIncident>> {
        self.read(incident::open).await
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

    /// Every outage `node_id` has open, ordered by check.
    ///
    /// # Errors
    ///
    /// The read fails.
    pub async fn open_outages(&self, node_id: &str) -> Result<Vec<Outage>> {
        let node_id = node_id.to_owned();
        self.read(move |conn| outage::open(conn, &node_id)).await
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
                let today = Day::containing(now_ms());
                let Some(asked) =
                    since.map_or_else(|| rollup::resume_day(conn, today), |day| Ok(Some(day)))?
                else {
                    return Ok((None, Vec::new()));
                };
                let floor = Day::containing(rollup::purged_before(conn)?.unwrap_or(0));
                let since = asked.max(floor);
                Ok((
                    Some(since),
                    rollup::aggregate(conn, since.start_ms(), i64::MAX)?,
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
    /// through between chunks and readers never wait.
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
            .read(move |conn| rollup::plan_finalize(conn, cutoff))
            .await?
        {
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
