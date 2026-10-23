// SPDX-License-Identifier: AGPL-3.0-only

//! The SQLite store: check results, outages, incidents and releases.
//!
//! The database holds history, never configuration. Results are
//! append-only and every row carries the `node_id` that produced it.
//!
//! One thread owns the only write connection and is fed through a channel;
//! reads go to a small pool of read connections. The database runs in WAL
//! mode, so reads never wait for writes. Every timestamp is `i64` unix
//! milliseconds, UTC; see [`day`]. The schema is documented in
//! `docs/storage.md`.

pub mod day;
mod error;
mod readers;
mod rollup;
mod schema;
#[cfg(test)]
mod tests;
mod writer;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use tocsin_core::Health;

pub use crate::day::Day;
pub use crate::error::{Error, Result};
use crate::readers::Readers;
pub use crate::rollup::DailyRollup;
use crate::writer::{Insert, Writer};

/// The database file's name inside the data folder.
pub const DB_FILE: &str = "tocsin.db";

/// Read connections in the pool.
const READERS: usize = 4;

/// The tables `tocsin db info` counts, in schema order.
pub const TABLES: [&str; 8] = [
    "check_result",
    "outage",
    "daily_rollup",
    "heartbeat_ping",
    "incident",
    "incident_update",
    "release",
    "retention",
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

/// A machine-detected outage, as handed to [`Store::open_outage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOutage {
    /// The node that saw it.
    pub node_id: String,
    /// The failing check.
    pub check_id: String,
    /// The component the check belongs to, if any.
    pub component_id: Option<String>,
    /// `Degraded` or `Down`; an outage is never `Up`.
    pub health: Health,
    /// When it began, unix milliseconds UTC.
    pub started_at_ms: i64,
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
    /// The schema version (number of migrations applied).
    pub schema_version: i64,
    /// Row count per table, in [`TABLES`] order.
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
        let writer = Writer::spawn(conn)?;
        Ok(Store {
            path: path.to_owned(),
            writer,
            readers,
        })
    }

    /// The database file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends a check result; resolves with its id once committed.
    ///
    /// # Errors
    ///
    /// A value breaks a constraint (an empty id, a negative time), or the
    /// write fails.
    pub async fn append(&self, result: NewCheckResult) -> Result<i64> {
        self.writer.insert(Insert::CheckResult(result)).await
    }

    /// Records the start of an outage; resolves with its id.
    ///
    /// # Errors
    ///
    /// The check already has an open outage on that node, the health is
    /// `Up`, or the write fails.
    pub async fn open_outage(&self, outage: NewOutage) -> Result<i64> {
        self.writer.insert(Insert::OpenOutage(outage)).await
    }

    /// Ends an open outage. This is the only change an outage ever receives.
    ///
    /// # Errors
    ///
    /// No outage has that id, it has already ended, it ends before it
    /// started, or the write fails.
    pub async fn end_outage(&self, id: i64, ended_at_ms: i64) -> Result<()> {
        self.writer
            .insert(Insert::EndOutage { id, ended_at_ms })
            .await
            .map(drop)
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
                let Some(asked) =
                    since.map_or_else(|| rollup::resume_day(conn), |day| Ok(Some(day)))?
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
                let transaction = conn.transaction()?;
                let rows = rollup::upsert(&transaction, &rollups)?;
                transaction.commit()?;
                Ok(rows)
            })
            .await?;
        Ok(RollupReport { since, rows })
    }

    /// Deletes results older than `policy.days` whole UTC days before
    /// `now_ms`. The days being dropped are rolled up first, in the same
    /// transaction that moves the watermark, so the daily bar never loses a
    /// day. The delete runs in chunks through the writer, so writes queued
    /// meanwhile go through between chunks and readers never wait.
    ///
    /// # Errors
    ///
    /// A read or write fails. A pass that fails part-way is finished by the
    /// next one.
    pub async fn apply_retention(&self, policy: Retention, now_ms: i64) -> Result<RetentionReport> {
        let cutoff = Day::containing(now_ms)
            .plus(-i64::from(policy.days))
            .start_ms();
        let (watermark, rollups) = self
            .read(move |conn| {
                let watermark = rollup::purged_before(conn)?;
                if cutoff <= 0 || watermark.is_some_and(|ms| ms >= cutoff) {
                    return Ok((watermark, None));
                }
                let from = watermark.unwrap_or(0);
                Ok((watermark, Some(rollup::aggregate(conn, from, cutoff)?)))
            })
            .await?;
        let mut rolled_up = 0;
        let target = match rollups {
            Some(rollups) => {
                rolled_up = self
                    .writer
                    .run(move |conn| {
                        let transaction = conn.transaction()?;
                        let rows = rollup::upsert(&transaction, &rollups)?;
                        rollup::raise_purged_before(&transaction, cutoff)?;
                        transaction.commit()?;
                        Ok(rows)
                    })
                    .await?;
                Some(cutoff)
            }
            // Finish any earlier pass that stopped part-way.
            None => watermark,
        };
        let Some(target) = target else {
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
        self.read(move |conn| {
            let schema_version: i64 =
                conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            let rows = TABLES
                .into_iter()
                .map(|table| {
                    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                        row.get(0)
                    })
                    .map(|count| (table, count))
                })
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let (oldest_result_ms, newest_result_ms) = conn.query_row(
                "SELECT min(started_at), max(started_at) FROM check_result",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            Ok(DbInfo {
                size_bytes: size_on_disk(&path)?,
                path,
                schema_version,
                rows,
                oldest_result_ms,
                newest_result_ms,
                purged_before_ms: rollup::purged_before(conn)?,
            })
        })
        .await
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
    /// The writer thread panicked.
    pub async fn close(self) -> Result<()> {
        let Store {
            writer, readers, ..
        } = self;
        drop(readers);
        let thread = writer.close();
        tokio::task::spawn_blocking(move || thread.join())
            .await
            .map_err(|_| Error::Closed)?
            .map_err(|_| Error::Closed)
    }
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
