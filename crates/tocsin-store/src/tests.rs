// SPDX-License-Identifier: AGPL-3.0-only

//! Tests that need the writer's internals.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use rusqlite::{TransactionBehavior, params};

use tocsin_core::incident::{
    Actor, Impact, Incident, IncidentKind, IncidentStatus, NewIncident, NewUpdate, Timestamp,
};

use crate::checkpoint::{self, Checkpointer, Nudge};
use crate::rollup;
use crate::writer::Writer;
use crate::{DB_FILE, Day, Error, NewCheckResult, Retention, Store, now_ms, schema};

fn open() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join(DB_FILE)).unwrap();
    (dir, store)
}

fn result(check: &str) -> NewCheckResult {
    NewCheckResult {
        node_id: "node-a".into(),
        check_id: check.into(),
        started_at_ms: now_ms(),
        duration_ms: 3,
        health: tocsin_core::Health::Up,
        detail: None,
        error: None,
        status_code: None,
    }
}

async fn stored(store: &Store, check: &'static str) -> i64 {
    store
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT count(*) FROM check_result WHERE check_id = ?1",
                [check],
                |row| row.get(0),
            )?)
        })
        .await
        .unwrap()
}

/// A write that panics, here halfway through a transaction it opened by
/// hand, reaches its caller as an error; the transaction is rolled back and
/// the writer thread keeps serving.
#[tokio::test]
async fn a_panicking_write_is_an_error_and_the_writer_carries_on() {
    let (_dir, store) = open();
    let panicked = store
        .writer
        .run::<(), _>(|conn| {
            conn.execute_batch(
                "BEGIN IMMEDIATE;
                 INSERT INTO check_result (node_id, check_id, started_at, duration_ms, health)
                 VALUES ('node-a', 'half-done', 1, 1, 'up');",
            )?;
            panic!("a write bug (expected in this test)");
        })
        .await;
    assert!(
        matches!(panicked, Err(Error::WriterPanicked)),
        "{panicked:?}"
    );
    store.append(result("after")).await.unwrap();
    assert_eq!(stored(&store, "half-done").await, 0);
    assert_eq!(stored(&store, "after").await, 1);
}

/// Some failures (`SQLITE_FULL`, `IOERR`, `NOMEM`, a trigger's `RAISE(ROLLBACK)`)
/// end the whole batch transaction, not just the failing insert's savepoint.
/// Every reply must then match what is in the table: an `Ok` row is stored
/// and an `Err` row is not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failure_that_ends_the_transaction_fails_the_whole_batch() {
    let (_dir, store) = open();
    store
        .writer
        .run(|conn| {
            conn.execute_batch(
                "CREATE TEMP TRIGGER poison BEFORE INSERT ON check_result
                 WHEN NEW.check_id = 'poison'
                 BEGIN SELECT RAISE(ROLLBACK, 'poisoned row'); END",
            )?;
            Ok(())
        })
        .await
        .unwrap();
    // Hold the writer so the three inserts queue up and share one batch.
    let park = store.writer.run(|_| {
        std::thread::sleep(Duration::from_millis(200));
        Ok(())
    });
    let (parked, first, poison, last) = tokio::join!(
        park,
        store.append(result("first")),
        store.append(result("poison")),
        store.append(result("last")),
    );
    parked.unwrap();
    for (check, outcome) in [("first", first), ("poison", poison), ("last", last)] {
        let rows = stored(&store, check).await;
        assert_eq!(
            rows == 1,
            outcome.is_ok(),
            "{check}: replied {outcome:?} but {rows} rows are stored"
        );
    }
    store.append(result("later")).await.unwrap();
    assert_eq!(stored(&store, "later").await, 1);
}

/// WAL is what keeps readers unblocked: a reader runs to completion, and sees
/// the last committed state, while the writer holds an uncommitted delete
/// under the strongest lock SQLite has. Under a rollback journal an
/// EXCLUSIVE transaction shuts every reader out, and the read below times out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readers_are_not_blocked_by_an_uncommitted_delete() {
    const ROWS: i64 = 20_000;
    let (_dir, store) = open();
    store
        .writer
        .run(|conn| {
            let transaction = conn.transaction()?;
            for i in 0..ROWS {
                transaction.execute(
                    "INSERT INTO check_result (node_id, check_id, started_at, duration_ms, health)
                     VALUES ('n', 'c', ?1, 1, 'up')",
                    params![i],
                )?;
            }
            transaction.commit()?;
            Ok(())
        })
        .await
        .unwrap();

    let (holding, held) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let store = std::sync::Arc::new(store);
    let delete = {
        let store = std::sync::Arc::clone(&store);
        tokio::spawn(async move {
            store
                .writer
                .run(move |conn| {
                    let transaction =
                        conn.transaction_with_behavior(TransactionBehavior::Exclusive)?;
                    let deleted = transaction.execute("DELETE FROM check_result", [])?;
                    holding.send(()).unwrap();
                    released.recv_timeout(Duration::from_secs(30)).unwrap();
                    transaction.commit()?;
                    Ok(deleted)
                })
                .await
        })
    };
    tokio::task::spawn_blocking(move || held.recv().unwrap())
        .await
        .unwrap();

    let read = tokio::time::timeout(
        Duration::from_secs(2),
        store.read(|conn| {
            Ok(
                conn.query_row("SELECT count(*) FROM check_result", [], |row| {
                    row.get::<_, i64>(0)
                })?,
            )
        }),
    )
    .await;
    release.send(()).unwrap();
    assert_eq!(delete.await.unwrap().unwrap(), 20_000);
    let count = read
        .expect("a reader waited for an uncommitted write")
        .unwrap();
    assert_eq!(count, ROWS, "a reader saw an uncommitted delete");
}

/// Retention counts a day on a reader, then finalizes it on the writer. A
/// result for that day committed in between (a node catching up on a
/// backlog, a hand-run insert) must be in the day's rollup before the delete
/// removes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_result_committed_between_count_and_finalize_is_counted() {
    let (_dir, store) = open();
    let day: Day = "2026-10-01".parse().unwrap();
    let raw = rusqlite::Connection::open(store.path()).unwrap();
    raw.busy_timeout(Duration::from_secs(5)).unwrap();
    let insert = "INSERT INTO check_result (node_id, check_id, started_at, duration_ms, health)
                  VALUES ('node-a', 'web', ?1, 1, 'up')";
    for i in 0..100 {
        raw.execute(insert, params![day.start_ms() + i * 1000])
            .unwrap();
    }
    let store = std::sync::Arc::new(store);

    // Park the writer so retention's count runs and its write waits.
    let (parked, parking) = std::sync::mpsc::channel();
    let park = {
        let store = std::sync::Arc::clone(&store);
        tokio::spawn(async move {
            store
                .writer
                .run(move |_| {
                    parked.send(()).unwrap();
                    std::thread::sleep(Duration::from_millis(500));
                    Ok(())
                })
                .await
        })
    };
    tokio::task::spawn_blocking(move || parking.recv().unwrap())
        .await
        .unwrap();
    let retention = {
        let store = std::sync::Arc::clone(&store);
        tokio::spawn(async move {
            store
                .apply_retention(Retention::days(3), day.plus(10).start_ms())
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    raw.execute(insert, params![day.start_ms() + 500_000])
        .unwrap();
    park.await.unwrap().unwrap();
    let report = retention.await.unwrap().unwrap();

    assert_eq!(report.deleted, 101);
    let rolled = store.daily_rollups(day).await.unwrap();
    assert_eq!(rolled[0].date, day);
    assert_eq!(
        rolled[0].total, 101,
        "a result was deleted without being counted"
    );
}

fn outage(title: &str) -> NewIncident {
    NewIncident {
        kind: IncidentKind::Incident,
        title: title.into(),
        impact: Impact::Major,
        components: vec!["web".into(), "api".into()],
        status: None,
        message: "Looking into it.".into(),
        scheduled_start: None,
        scheduled_end: None,
    }
}

thread_local! {
    /// Set when the reader starts the statement that reads incident rows.
    static READING_ROWS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Opens a plain connection to the store's file that, while it is reading
/// incident rows, has a second connection resolve incident `id` with a
/// second update. The write commits after the row is read and before the
/// updates are, which is the moment a torn read would show it.
fn reader_raced_by_a_resolution(path: &std::path::Path, id: i64) -> rusqlite::Connection {
    // rusqlite fixes the signature, so the event is taken by value.
    let watch: fn(rusqlite::trace::TraceEvent<'_>) = |event| {
        if let rusqlite::trace::TraceEvent::Stmt(_, sql) = event {
            READING_ROWS.with(|reading| reading.set(sql.contains("FROM incident WHERE")));
        }
    };
    let reader = rusqlite::Connection::open(path).unwrap();
    reader.trace_v2(
        rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
        Some(watch),
    );
    let writer = std::sync::Mutex::new(Some(rusqlite::Connection::open(path).unwrap()));
    reader
        .progress_handler(
            1,
            Some(move || {
                if !READING_ROWS.with(std::cell::Cell::get) {
                    return false;
                }
                if let Some(writer) = writer.lock().unwrap().take() {
                    writer
                        .execute_batch(&format!(
                            "BEGIN IMMEDIATE;
                             INSERT INTO incident_update (incident_id, status, message,
                                 created_at, created_by_kind, created_by)
                             VALUES ({id}, 'resolved', 'Fixed.', 2000, 'token', 'oncall');
                             UPDATE incident SET status = 'resolved', resolved_at = 2000
                             WHERE id = {id};
                             COMMIT;"
                        ))
                        .unwrap();
                }
                false
            }),
        )
        .unwrap();
    reader
}

/// Asserts the incident's status is its latest update's, which a read
/// that took the row and the updates from different moments breaks.
fn assert_untorn(incident: &Incident) {
    let latest = incident.updates.last().unwrap();
    assert_eq!(
        incident.status, latest.status,
        "the row and the updates came from different moments: {incident:?}"
    );
}

/// A write that commits while an incident is being read does not tear it:
/// the row, components and updates all come from one snapshot.
#[tokio::test]
async fn an_incident_is_read_from_one_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DB_FILE);
    let store = Store::open(&path).unwrap();
    let id = store
        .create_incident(
            outage("API errors"),
            Actor::token("oncall"),
            Timestamp::from_millis(1000),
        )
        .await
        .unwrap()
        .id;

    let reader = reader_raced_by_a_resolution(&path, id);
    let loaded = crate::incident::load(&reader, id).unwrap().unwrap();
    assert_untorn(&loaded);
    // The read saw the incident as it was before the racing write.
    assert_eq!(loaded.status, IncidentStatus::Investigating);

    let reader = reader_raced_by_a_resolution(&path, id);
    store
        .add_incident_update(
            id,
            NewUpdate {
                status: IncidentStatus::Identified,
                message: "Found it.".into(),
            },
            Actor::token("oncall"),
            Timestamp::from_millis(1500),
        )
        .await
        .unwrap();
    let listed = crate::incident::list(&reader, None, 10).unwrap();
    assert_untorn(&listed[0]);
    assert_eq!(listed[0].status, IncidentStatus::Identified);
    // The racing writes did land; the reads just did not straddle them.
    let after = store.incident(id).await.unwrap().unwrap();
    assert_eq!(after.status, IncidentStatus::Resolved);
    store.close().await.unwrap();
}

thread_local! {
    static STATEMENTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A page of incidents costs the same handful of queries however many
/// incidents it holds.
#[tokio::test]
async fn listing_a_page_does_not_query_per_incident() {
    fn count(_: rusqlite::trace::TraceEvent<'_>) {
        STATEMENTS.with(|statements| statements.set(statements.get() + 1));
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DB_FILE);
    let store = Store::open(&path).unwrap();
    let reader = rusqlite::Connection::open(&path).unwrap();
    reader.trace_v2(
        rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
        Some(count),
    );
    let mut costs = Vec::new();
    for page in [1, 5] {
        while store.incidents(None, 100).await.unwrap().len() < page {
            store
                .create_incident(
                    outage("API errors"),
                    Actor::token("oncall"),
                    Timestamp::from_millis(1000),
                )
                .await
                .unwrap();
        }
        STATEMENTS.with(|statements| statements.set(0));
        let listed = crate::incident::list(&reader, None, 100).unwrap();
        assert_eq!(listed.len(), page);
        assert!(
            listed.iter().all(
                |incident| incident.components == ["api", "web"] && incident.updates.len() == 1
            )
        );
        costs.push(STATEMENTS.with(std::cell::Cell::get));
    }
    assert_eq!(
        costs[0], costs[1],
        "statements for 1 and 5 incidents: {costs:?}"
    );
    store.close().await.unwrap();
}

/// Incidents naming a component are found however far back they are,
/// newest first, limited, complete, and none that only name another.
#[tokio::test]
async fn incidents_naming_a_component_skip_the_others() {
    let (_dir, store) = open();
    let mut named = Vec::new();
    for (title, components) in [
        ("Old database outage", vec!["db"]),
        ("API errors", vec!["web", "api"]),
        ("Database and API", vec!["db", "api"]),
        ("Web only", vec!["web"]),
    ] {
        let incident = store
            .create_incident(
                NewIncident {
                    components: components.into_iter().map(Into::into).collect(),
                    ..outage(title)
                },
                Actor::token("oncall"),
                Timestamp::from_millis(1000),
            )
            .await
            .unwrap();
        if incident
            .components
            .iter()
            .any(|component| component == "db")
        {
            named.push(incident.id);
        }
    }
    let found = store.incidents_naming("db", 10).await.unwrap();
    let ids: Vec<i64> = found.iter().map(|incident| incident.id).collect();
    named.reverse();
    assert_eq!(ids, named);
    assert!(found.iter().all(|incident| incident.updates.len() == 1));
    assert_eq!(found[0].components, ["api", "db"]);
    assert_eq!(store.incidents_naming("db", 1).await.unwrap().len(), 1);
    assert_eq!(store.incidents_naming("cache", 10).await.unwrap(), []);
    store.close().await.unwrap();
}

/// Writes about 5 MB in one transaction: past the 1000 pages (4 MB) at which
/// SQLite's automatic checkpoint would run inside the commit.
async fn fill_log(writer: &Writer) {
    writer
        .run(|conn| {
            conn.execute_batch(
                "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 10000)
                 INSERT INTO check_result (node_id, check_id, started_at, duration_ms, health, error)
                 SELECT 'node-a', 'bulk', i, 1, 'up', hex(randomblob(250)) FROM n;",
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

/// The database file only grows when a checkpoint copies the log into it.
fn database_bytes(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().len()
}

/// A commit that pushes the log past SQLite's checkpoint threshold leaves
/// the copy to the checkpointer (TOC-57): the database file is untouched and
/// the checkpointer is nudged.
#[tokio::test]
async fn writes_leave_the_checkpoint_to_the_checkpointer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DB_FILE);
    let (nudge, nudges) = Nudge::detached();
    let writer = Writer::spawn(schema::open_writer(&path).unwrap(), nudge).unwrap();
    let before = database_bytes(&path);
    fill_log(&writer).await;
    fill_log(&writer).await;
    assert_eq!(
        database_bytes(&path),
        before,
        "a commit checkpointed the log on the writer thread"
    );
    assert!(nudges.try_recv().is_ok(), "the checkpointer was not nudged");
    writer.close().join().unwrap();
}

/// The checkpointer copies the log into the database file soon after a
/// write, without anything else happening, and closing the store removes
/// the log.
#[tokio::test]
async fn the_checkpointer_copies_the_log_after_writes() {
    let (dir, store) = open();
    let path = dir.path().join(DB_FILE);
    let before = database_bytes(&path);
    fill_log(&store.writer).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while database_bytes(&path) == before {
        assert!(
            Instant::now() < deadline,
            "the log was not checkpointed within 10s"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    store.close().await.unwrap();
    // The last connection to close checkpoints and removes the log.
    assert!(!dir.path().join(format!("{DB_FILE}-wal")).exists());
}

/// If the checkpointer is gone, writes checkpoint the log themselves again
/// rather than letting it grow without bound.
#[tokio::test]
async fn without_a_checkpointer_writes_checkpoint_again() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DB_FILE);
    let (nudge, nudges) = Nudge::detached();
    drop(nudges);
    let writer = Writer::spawn(schema::open_writer(&path).unwrap(), nudge).unwrap();
    // The first write finds the checkpointer gone.
    writer.run(|_| Ok(())).await.unwrap();
    let before = database_bytes(&path);
    fill_log(&writer).await;
    assert!(
        database_bytes(&path) > before,
        "the log was not checkpointed by the write"
    );
    writer.close().join().unwrap();
}

fn log_bytes(path: &Path) -> u64 {
    let mut log = path.as_os_str().to_owned();
    log.push("-wal");
    std::fs::metadata(log).map_or(0, |metadata| metadata.len())
}

/// Copies the whole log into the database file and empties it, from a
/// connection of the test's own.
fn empty_log(path: &Path) {
    rusqlite::Connection::open(path)
        .unwrap()
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .unwrap();
}

/// The size of a log file of `frames` frames: a 32-byte header, then a
/// 24-byte header and a page for each frame.
fn log_file_bytes(path: &Path, frames: u64) -> u64 {
    let page: u32 = rusqlite::Connection::open(path)
        .unwrap()
        .pragma_query_value(None, "page_size", |row| row.get(0))
        .unwrap();
    32 + frames * (24 + u64::from(page))
}

/// The most frames one [`write_row`] appends while the table and its indexes
/// are two levels deep, as they stay in these tests: page 1, whose header
/// records the file's size; `sqlite_sequence`, for the `AUTOINCREMENT` id;
/// the table's root and at most two leaves; and for each of the three
/// indexes its root and at most four leaves, when a full leaf is rebalanced
/// with two neighbours into one more page. Most writes take 7; the most
/// measured is 16.
const WRITE_FRAMES: u64 = 1 + 1 + 3 + 3 * 5;

/// The writer at the hard limit copies the log and the next commit rewinds
/// it, so the log holds at most the hard limit's frames, less one, plus the
/// frames of the one write that reached it.
fn log_bound() -> u64 {
    u64::try_from(checkpoint::HARD_LIMIT_FRAMES).unwrap() - 1 + WRITE_FRAMES
}

/// The largest the log got, in frames and file bytes, and the most frames
/// one write appended.
#[derive(Default)]
struct LogHighs {
    frames: u64,
    bytes: u64,
    write_frames: u64,
    appended: u64,
}

impl LogHighs {
    /// Records one write that left `frames` in the log and `appended` ever.
    fn record(&mut self, path: &Path, (appended, frames): (u64, u64)) {
        self.write_frames = self.write_frames.max(appended - self.appended);
        self.appended = appended;
        self.frames = self.frames.max(frames);
        self.bytes = self.bytes.max(log_bytes(path));
    }

    /// Asserts no write appended more than [`WRITE_FRAMES`] and the log
    /// stayed within [`log_bound`] in frames and in bytes.
    fn assert_bounded(&self, path: &Path) {
        assert!(
            self.write_frames <= WRITE_FRAMES,
            "a write appended {} frames, over the {WRITE_FRAMES} a row this size takes",
            self.write_frames
        );
        let bound = log_bound();
        assert!(
            self.frames <= bound,
            "the log reached {} frames, over {bound} (the limit plus one write)",
            self.frames
        );
        let bytes = log_file_bytes(path, bound);
        assert!(
            self.bytes <= bytes,
            "the log file reached {} bytes, over {bytes}",
            self.bytes
        );
    }
}

/// Writes one ~3 KB row; returns the frames ever appended and those in the
/// log after it.
async fn write_row(writer: &Writer) -> (u64, u64) {
    writer
        .run(|conn| {
            conn.execute(
                "INSERT INTO check_result (node_id, check_id, started_at, duration_ms, health, error)
                 VALUES ('node-a', 'steady', 1, 1, 'up', hex(randomblob(1500)))",
                [],
            )?;
            Ok((checkpoint::appended(), checkpoint::log_frames()))
        })
        .await
        .unwrap()
}

/// Writes that never pause race every checkpoint the checkpointer runs, so
/// none of them leaves the whole log copied and SQLite never rewinds it.
/// Once the log reaches its hard limit the writer copies the rest itself,
/// waiting out any pass of the checkpointer's still running, which keeps
/// the log within [`log_bound`] however long the writes go on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_log_stays_bounded_under_writes_that_never_pause() {
    let (dir, store) = open();
    let path = dir.path().join(DB_FILE);
    // The migrations' frames are not writes of this test's.
    empty_log(&path);
    let limit_frames = u64::try_from(checkpoint::HARD_LIMIT_FRAMES).unwrap();
    let mut highs = LogHighs {
        appended: store
            .writer
            .run(|_| Ok(checkpoint::appended()))
            .await
            .unwrap(),
        ..LogHighs::default()
    };
    while highs.appended < 4 * limit_frames {
        let write = write_row(&store.writer).await;
        highs.record(&path, write);
    }
    highs.assert_bounded(&path);
    // What the store reports it did, as the bench prints it (TOC-62).
    let stats = store.checkpoint_stats();
    assert!(stats.passes > 0, "{stats:?}");
    assert!(stats.rewinds > 0, "{stats:?}");
    assert!(stats.frames_high > 0, "{stats:?}");
    store.close().await.unwrap();
}

// A busy handler is a plain function, so it reaches these through statics,
// shared by the whole test binary: only one test may hold a checkpoint at a
// time this way, which [`HOLDER_TAKEN`] sees to.

/// A [`CheckpointHolder`] exists.
static HOLDER_TAKEN: AtomicBool = AtomicBool::new(false);
/// Set by [`keep_checkpointing`] once it is holding the checkpoint lock.
static CHECKPOINT_HELD: AtomicBool = AtomicBool::new(false);
/// Lets [`keep_checkpointing`] go.
static CHECKPOINT_RELEASED: AtomicBool = AtomicBool::new(false);

/// A busy handler for a `FULL` checkpoint waiting for the write lock: it
/// waits, holding the checkpoint lock, until [`CHECKPOINT_RELEASED`], then
/// gives up the write lock, so the checkpoint finishes as a `PASSIVE` one.
fn keep_checkpointing(_: i32) -> bool {
    CHECKPOINT_HELD.store(true, Ordering::SeqCst);
    while !CHECKPOINT_RELEASED.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(1));
    }
    false
}

/// A checkpoint of the test's own, holding SQLite's checkpoint lock and no
/// other lock, as the checkpointer does through a slow pass.
struct CheckpointHolder {
    thread: Option<std::thread::JoinHandle<()>>,
    interrupt: rusqlite::InterruptHandle,
}

impl CheckpointHolder {
    /// Takes the checkpoint lock on `path` and holds it until
    /// [`CheckpointHolder::release`], once no other test holds one.
    fn hold(path: &Path) -> CheckpointHolder {
        let deadline = Instant::now() + Duration::from_secs(60);
        while HOLDER_TAKEN.swap(true, Ordering::SeqCst) {
            assert!(
                Instant::now() < deadline,
                "another test held a checkpoint for a minute"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        CHECKPOINT_HELD.store(false, Ordering::SeqCst);
        CHECKPOINT_RELEASED.store(false, Ordering::SeqCst);
        // A FULL checkpoint takes the checkpoint lock first, then calls its
        // busy handler while the write lock is taken.
        let write_lock = rusqlite::Connection::open(path).unwrap();
        write_lock.execute_batch("BEGIN IMMEDIATE").unwrap();
        let conn = rusqlite::Connection::open(path).unwrap();
        // With no syncing, nothing it does once released waits on the disk.
        conn.execute_batch("PRAGMA synchronous = OFF").unwrap();
        conn.busy_handler(Some(keep_checkpointing)).unwrap();
        let interrupt = conn.get_interrupt_handle();
        let thread = std::thread::spawn(move || {
            // Interrupted before it copies a frame, unless there was none.
            let _ = conn.query_row("PRAGMA wal_checkpoint(FULL)", [], |_| Ok(()));
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while !CHECKPOINT_HELD.load(Ordering::SeqCst) {
            assert!(
                Instant::now() < deadline,
                "the test's checkpoint never waited"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        // The checkpoint stays in its handler, now holding only its own lock.
        write_lock.execute_batch("ROLLBACK").unwrap();
        CheckpointHolder {
            thread: Some(thread),
            interrupt,
        }
    }

    /// Lets the lock go at once: the checkpoint is interrupted before it
    /// copies anything, so a slow disk cannot keep holding it (a copy here
    /// once ran past the writer's wait under heavy disk load).
    fn release(&self) {
        self.interrupt.interrupt();
        CHECKPOINT_RELEASED.store(true, Ordering::SeqCst);
    }

    /// Lets the lock go and waits until it is free.
    fn join(mut self) {
        self.release();
        self.thread.take().unwrap().join().unwrap();
    }
}

impl Drop for CheckpointHolder {
    fn drop(&mut self) {
        self.release();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        HOLDER_TAKEN.store(false, Ordering::SeqCst);
    }
}

/// The checkpointer's pass can run for seconds on a disk busy flushing
/// other work, holding the one checkpoint lock, and the writer's own
/// checkpoint at the limit then copies nothing (TOC-59: the log reached
/// 12005 frames on CI). The writer must wait for the lock rather than keep
/// appending. Here a connection of the test's own holds that lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_writer_waits_at_the_limit_for_another_checkpoint() {
    let (dir, store) = open();
    let path = dir.path().join(DB_FILE);
    empty_log(&path);
    let limit_frames = u64::try_from(checkpoint::HARD_LIMIT_FRAMES).unwrap();
    let mut highs = LogHighs {
        appended: store
            .writer
            .run(|_| Ok(checkpoint::appended()))
            .await
            .unwrap(),
        ..LogHighs::default()
    };
    let waits = store.checkpointer.waits();
    let holder = CheckpointHolder::hold(&path);
    let probe: (bool, i64, i64) = rusqlite::Connection::open(&path)
        .unwrap()
        .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap();
    assert_eq!(
        probe,
        (true, -1, -1),
        "the test's checkpoint does not hold the lock"
    );
    // Writes below the limit go on as usual. Past it, the next one waits
    // behind the writer for as long as the lock is held.
    let mut stalled = false;
    while highs.appended < 2 * limit_frames {
        let mut write = std::pin::pin!(write_row(&store.writer));
        let written = if !stalled && highs.frames >= limit_frames {
            if let Ok(written) = tokio::time::timeout(Duration::from_millis(500), &mut write).await
            {
                written
            } else {
                stalled = true;
                holder.release();
                write.await
            }
        } else {
            write.await
        };
        highs.record(&path, written);
    }
    holder.join();
    highs.assert_bounded(&path);
    assert!(
        store.checkpointer.waits() > waits,
        "the writer never waited for the checkpoint lock"
    );
    assert!(stalled, "a write past the limit did not wait for the lock");
    store.close().await.unwrap();
}

/// The log events of a thread that runs under [`Captured::run`], with their
/// fields.
///
/// One subscriber, the global default, hands each event to the `Captured`
/// its thread runs under. A subscriber per test, set with `with_default`,
/// loses events: tracing caches whether each log line is wanted once for
/// the whole process, and a thread with no subscriber that reaches a line
/// first, while one test's subscriber is the only one registered, caches
/// "never" for every thread.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<(tracing::Level, String)>>>);

thread_local! {
    static CAPTURING: std::cell::RefCell<Option<Captured>> = const { std::cell::RefCell::new(None) };
}

impl Captured {
    fn run<T>(&self, f: impl FnOnce() -> T) -> T {
        /// Puts back the thread's previous capture, even if `f` panics.
        struct Restore(Option<Captured>);
        impl Drop for Restore {
            fn drop(&mut self) {
                CAPTURING.set(self.0.take());
            }
        }
        static ROUTER: std::sync::Once = std::sync::Once::new();
        ROUTER.call_once(|| {
            use tracing_subscriber::layer::SubscriberExt;
            tracing::subscriber::set_global_default(tracing_subscriber::registry().with(Router))
                .expect("nothing else sets a global subscriber in these tests");
            // A line first reached after the subscriber registered, but
            // before it became the default, cached "never".
            tracing_core::callsite::rebuild_interest_cache();
        });
        let _restore = Restore(CAPTURING.replace(Some(self.clone())));
        f()
    }

    /// The fields of each event at `level`.
    fn at(&self, level: tracing::Level) -> Vec<String> {
        let events = self.0.lock().unwrap();
        events
            .iter()
            .filter(|(at, _)| *at == level)
            .map(|(_, fields)| fields.clone())
            .collect()
    }
}

/// The global subscriber's one layer: records each event in its thread's
/// [`Captured`], if it has one.
struct Router;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Router {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct Fields(String);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                let _ = write!(self.0, "{}={value:?} ", field.name());
            }
        }
        let Some(captured) = CAPTURING.with_borrow(Option::clone) else {
            return;
        };
        let mut fields = Fields(String::new());
        event.record(&mut fields);
        let mut events = captured.0.lock().unwrap();
        events.push((*event.metadata().level(), fields.0));
    }
}

/// A connection set up as the writer's is, on the test's own thread, with a
/// log at its hard limit. Small pages make that 4 MB rather than 32.
fn log_at_limit(path: &Path) -> rusqlite::Connection {
    log_of(path, limit_frames())
}

/// A connection set up as the writer's is, on the test's own thread, with a
/// log of at least `frames` frames of 512-byte pages.
fn log_of(path: &Path, frames: u64) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        "PRAGMA page_size = 512;
         PRAGMA journal_mode = WAL;
         PRAGMA wal_autocheckpoint = 0;
         CREATE TABLE t (x BLOB);",
    )
    .unwrap();
    checkpoint::watch(&conn);
    while checkpoint::log_frames() < frames {
        add_row(&conn);
    }
    conn
}

fn limit_frames() -> u64 {
    u64::try_from(checkpoint::HARD_LIMIT_FRAMES).unwrap()
}

fn soft_limit_frames() -> u64 {
    u64::try_from(checkpoint::SOFT_LIMIT_FRAMES).unwrap()
}

fn add_row(conn: &rusqlite::Connection) {
    conn.execute("INSERT INTO t VALUES (randomblob(400))", [])
        .unwrap();
}

/// Once a wait at the limit times out, the writer logs one error and stops
/// waiting: each later write would otherwise be held for the whole wait.
/// When a checkpoint gets through again it logs one line and the log
/// rewinds.
#[test]
fn a_lockout_logs_once_and_later_writes_do_not_wait() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DB_FILE);
    let conn = log_at_limit(&path);
    let (mut nudge, _nudges) = Nudge::detached();
    let passes = nudge.passes();
    let logs = Captured::default();
    let holder = CheckpointHolder::hold(&path);
    logs.run(|| {
        add_row(&conn);
        let started = Instant::now();
        nudge.after_write(&conn);
        assert!(
            started.elapsed() >= schema::BUSY_TIMEOUT,
            "the first write past the limit waited only {:?}",
            started.elapsed()
        );
        for _ in 0..2 {
            add_row(&conn);
            let started = Instant::now();
            nudge.after_write(&conn);
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "a write in a lockout waited {:?}",
                started.elapsed()
            );
        }
        holder.join();
        add_row(&conn);
        nudge.after_write(&conn);
        add_row(&conn);
        nudge.after_write(&conn);
    });
    assert!(
        checkpoint::log_frames() < limit_frames(),
        "the log did not rewind after the lockout"
    );
    let errors = logs.at(tracing::Level::ERROR);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("longer than"), "{errors:?}");
    let recoveries = logs.at(tracing::Level::INFO);
    assert_eq!(recoveries.len(), 1, "{recoveries:?}");
    assert!(recoveries[0].contains("peak_frames="), "{recoveries:?}");
    assert_eq!(passes.waits(), 1);
    // The test's checkpoint is not the checkpointer's, so the writer polls.
    assert!(passes.polls() > 0);
    let stats = nudge.stats();
    assert_eq!(stats.lockouts, 1, "{stats:?}");
    assert_eq!(stats.limit_waits, 1, "{stats:?}");
    assert!(
        stats.limit_wait_max + Duration::from_millis(100) >= schema::BUSY_TIMEOUT,
        "{stats:?}"
    );
    assert!(stats.inline_copies >= 1, "{stats:?}");
    assert!(stats.rewinds >= 1, "{stats:?}");
}

/// A checkpoint that fails is a lockout too: one error, naming SQLite's,
/// and no waiting.
#[test]
fn a_failing_checkpoint_is_a_lockout_that_logs_its_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DB_FILE);
    let conn = log_at_limit(&path);
    let (mut nudge, _nudges) = Nudge::detached();
    let passes = nudge.passes();
    let logs = Captured::default();
    logs.run(|| {
        // A checkpoint inside a transaction fails with SQLITE_LOCKED.
        conn.execute_batch("BEGIN").unwrap();
        add_row(&conn);
        for _ in 0..3 {
            nudge.after_write(&conn);
        }
        conn.execute_batch("COMMIT").unwrap();
        nudge.after_write(&conn);
    });
    let errors = logs.at(tracing::Level::ERROR);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors[0].contains("error=database table is locked"),
        "{errors:?}"
    );
    let recoveries = logs.at(tracing::Level::INFO);
    assert_eq!(recoveries.len(), 1, "{recoveries:?}");
    assert_eq!(passes.waits(), 0);
}

/// A thread with no capture that reaches a log line first does not hide it
/// from a capture that reaches it later.
#[test]
fn a_capture_sees_a_line_another_thread_reached_first() {
    let logs = Captured::default();
    let (errors, recoveries, lockouts) = logs.run(|| {
        // The same lockout, on a thread that captures nothing. It ends in
        // its transaction, so it reaches the error but not the recovery.
        std::thread::spawn(|| {
            let dir = tempfile::tempdir().unwrap();
            let conn = log_at_limit(&dir.path().join(DB_FILE));
            let (mut nudge, _nudges) = Nudge::detached();
            conn.execute_batch("BEGIN").unwrap();
            add_row(&conn);
            nudge.after_write(&conn);
            assert_eq!(nudge.stats().lockouts, 1);
        })
        .join()
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let conn = log_at_limit(&dir.path().join(DB_FILE));
        let (mut nudge, _nudges) = Nudge::detached();
        conn.execute_batch("BEGIN").unwrap();
        add_row(&conn);
        nudge.after_write(&conn);
        conn.execute_batch("COMMIT").unwrap();
        nudge.after_write(&conn);
        (
            logs.at(tracing::Level::ERROR),
            logs.at(tracing::Level::INFO),
            nudge.stats().lockouts,
        )
    });
    assert_eq!(lockouts, 1);
    assert_eq!(recoveries.len(), 1, "{recoveries:?}");
    assert_eq!(errors.len(), 1, "{errors:?}, with {recoveries:?}");
}

/// When the checkpointer's own pass holds the lock, the writer at the limit
/// sleeps until the pass ends, then checkpoints, rather than polling.
#[test]
fn the_writer_waits_out_the_checkpointers_pass_without_polling() {
    const PASS: Duration = Duration::from_millis(300);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DB_FILE);
    let conn = log_at_limit(&path);
    let (mut nudge, _nudges) = Nudge::detached();
    let passes = nudge.passes();
    let holder = CheckpointHolder::hold(&path);
    let (begun, has_begun) = mpsc::channel();
    let pass = std::thread::spawn({
        let passes = Arc::clone(&passes);
        move || {
            let _pass = passes.begin().expect("nothing to stand aside for");
            begun.send(Instant::now()).unwrap();
            std::thread::sleep(PASS);
            // A pass lets go of the lock, then ends.
            holder.join();
        }
    });
    // Measured from when the pass began, not from when this thread heard
    // of it: the pass is already asleep by then.
    let pass_began = has_begun.recv().unwrap();
    add_row(&conn);
    nudge.after_write(&conn);
    let took = pass_began.elapsed();
    pass.join().unwrap();
    assert_eq!(passes.waits(), 1);
    assert_eq!(passes.polls(), 0, "the writer polled");
    let stats = nudge.stats();
    assert_eq!((stats.limit_writes, stats.limit_waits), (1, 1), "{stats:?}");
    assert!(!stats.limit_wait_max.is_zero(), "{stats:?}");
    assert!(
        took >= PASS && took < schema::BUSY_TIMEOUT,
        "the writer was done {took:?} after the pass began"
    );
    add_row(&conn);
    nudge.after_write(&conn);
    assert!(
        checkpoint::log_frames() < limit_frames(),
        "the writer's checkpoint after the pass left the log unrewound"
    );
}

/// While the writer checkpoints at the limit the checkpointer starts no
/// pass, which would only take the lock from it; once the writer has gone
/// quiet it checkpoints again.
#[test]
fn the_checkpointer_stands_aside_while_the_writer_is_at_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DB_FILE);
    let conn = log_at_limit(&path);
    let before = database_bytes(&path);
    let (checkpointer, mut nudge) =
        Checkpointer::spawn(schema::open_checkpointer(&path).unwrap()).unwrap();
    let passes = nudge.passes();
    // A checkpoint inside a transaction fails, so the writer at the limit
    // copies nothing itself; it nudges the checkpointer all the same.
    conn.execute_batch("BEGIN").unwrap();
    add_row(&conn);
    nudge.after_write(&conn);
    conn.execute_batch("COMMIT").unwrap();
    let deadline = Instant::now() + schema::BUSY_TIMEOUT;
    while passes.skips() == 0 {
        assert!(Instant::now() < deadline, "the checkpointer did not skip");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(checkpointer.stats().stand_asides > 0);
    assert_eq!(database_bytes(&path), before, "the checkpointer copied");
    while database_bytes(&path) == before {
        assert!(
            Instant::now() < deadline,
            "the checkpointer never came back to the log"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(nudge);
    checkpointer.join().unwrap();
}

/// Writes old results in transactions of 100 until the log holds at least
/// `frames` frames, returning how many it wrote.
async fn old_results_until(store: &Store, day: Day, frames: u64) -> i64 {
    let mut written = 0;
    loop {
        let log = store
            .writer
            .run(move |conn| {
                for i in 0..100 {
                    conn.execute(
                        "INSERT INTO check_result (node_id, check_id, started_at, duration_ms, health, error)
                         VALUES ('node-a', 'web', ?1, 1, 'up', hex(randomblob(1500)))",
                        params![day.start_ms() + written + i],
                    )?;
                }
                Ok(checkpoint::log_frames())
            })
            .await
            .unwrap();
        written += 100;
        if log >= frames {
            return written;
        }
    }
}

fn results(path: &Path) -> i64 {
    rusqlite::Connection::open(path)
        .unwrap()
        .query_row("SELECT count(*) FROM check_result", [], |row| row.get(0))
        .unwrap()
}

/// Retention writes no faster than the checkpointer copies (TOC-62): with
/// the log past the soft limit and a pass unable to copy it, retention
/// writes nothing, and once a pass can, it carries on well inside its wait.
/// `finalized` rolls the old day up first, so retention's first write is a
/// delete rather than the rollup.
async fn retention_waits_for_a_pass_past_the_soft_limit(finalized: bool) {
    let (_dir, store) = open();
    let day: Day = "2026-10-01".parse().unwrap();
    let now = day.plus(10).start_ms();
    let policy = Retention {
        days: 3,
        chunk_rows: 50,
        pause: Duration::from_millis(10),
    };
    let cutoff = day.plus(7).start_ms();
    // Every pass finds the lock taken, so none copies the log.
    let holder = CheckpointHolder::hold(store.path());
    let written = old_results_until(&store, day, soft_limit_frames() + 200).await;
    if finalized {
        while let Some(plan) = store
            .read(move |conn| rollup::plan_finalize(conn, cutoff, now))
            .await
            .unwrap()
        {
            store
                .writer
                .run(move |conn| rollup::finalize(conn, plan))
                .await
                .unwrap();
        }
    }
    let store = Arc::new(store);
    let retention = {
        let store = Arc::clone(&store);
        tokio::spawn(async move { store.apply_retention(policy, now).await })
    };
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        store.daily_rollups(day).await.unwrap().len(),
        usize::from(finalized),
        "retention rolled up past the soft limit before a pass copied the log"
    );
    assert_eq!(
        results(store.path()),
        written,
        "retention deleted past the soft limit before a pass copied the log"
    );
    assert!(!retention.is_finished());
    // Waiting holds neither the writer nor a transaction: a check result
    // still commits.
    tokio::time::timeout(
        Duration::from_secs(1),
        store.writer.run(move |conn| {
            conn.execute(
                "INSERT INTO check_result (node_id, check_id, started_at, duration_ms, health)
                 VALUES ('node-a', 'web', ?1, 1, 'up')",
                params![now],
            )?;
            Ok(())
        }),
    )
    .await
    .expect("a write waited behind retention's wait for the checkpointer")
    .unwrap();
    assert!(!retention.is_finished());
    holder.join();
    let released = Instant::now();
    // The checkpointer tries a busy pass again a second later.
    let report = tokio::time::timeout(Duration::from_secs(3), retention)
        .await
        .expect("retention waited past a pass that copied the log")
        .unwrap()
        .unwrap();
    assert!(released.elapsed() < Duration::from_secs(3));
    assert_eq!(report.deleted, usize::try_from(written).unwrap());
    let stats = store.checkpoint_stats();
    assert!(stats.paced_writes > 0);
    assert!(stats.paced_wait_max >= Duration::from_millis(500));
    assert_eq!(stats.limit_writes, 0, "retention reached the hard limit");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_rolls_up_only_once_a_pass_has_copied_the_log() {
    retention_waits_for_a_pass_past_the_soft_limit(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_deletes_only_once_a_pass_has_copied_the_log() {
    retention_waits_for_a_pass_past_the_soft_limit(true).await;
}

/// A pass that never copies the log holds retention up for no longer than
/// the writer would wait at the hard limit, and says so: one warning with
/// the log's length and the wait, then retention writes.
#[test]
fn retention_that_waits_out_its_cap_warns_and_carries_on() {
    let logs = Captured::default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (waited, stats) = logs.run(|| {
        runtime.block_on(async {
            let (_dir, store) = open();
            let day: Day = "2026-10-01".parse().unwrap();
            let holder = CheckpointHolder::hold(store.path());
            old_results_until(&store, day, soft_limit_frames() + 200).await;
            let started = Instant::now();
            store.checkpointer.make_room().await;
            let waited = started.elapsed();
            holder.join();
            (waited, store.checkpoint_stats())
        })
    });
    assert!(waited >= checkpoint::PACE_WAIT, "{waited:?}");
    assert!(stats.paced_wait_max >= checkpoint::PACE_WAIT, "{stats:?}");
    let warnings = logs.at(tracing::Level::WARN);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("frames="), "{warnings:?}");
    assert!(warnings[0].contains("waited=5"), "{warnings:?}");
}

/// A wait that a pass ends before the cap logs nothing.
#[test]
fn retention_that_a_pass_lets_through_does_not_warn() {
    let logs = Captured::default();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (waited, stats) = logs.run(|| {
        runtime.block_on(async {
            let (_dir, store) = open();
            let day: Day = "2026-10-01".parse().unwrap();
            let holder = CheckpointHolder::hold(store.path());
            old_results_until(&store, day, soft_limit_frames() + 200).await;
            let release = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                holder.join();
            });
            let started = Instant::now();
            store.checkpointer.make_room().await;
            let waited = started.elapsed();
            release.join().unwrap();
            (waited, store.checkpoint_stats())
        })
    });
    assert!(waited >= Duration::from_millis(200), "{waited:?}");
    assert!(waited < checkpoint::PACE_WAIT, "{waited:?}");
    assert_eq!(stats.paced_writes, 1, "{stats:?}");
    let warnings = logs.at(tracing::Level::WARN);
    assert!(warnings.is_empty(), "{warnings:?}");
}

/// Below the soft limit retention never waits for the checkpointer.
#[tokio::test]
async fn retention_below_the_soft_limit_does_not_wait() {
    let (_dir, store) = open();
    let day: Day = "2026-10-01".parse().unwrap();
    let written = old_results_until(&store, day, 0).await;
    let report = store
        .apply_retention(Retention::days(3), day.plus(10).start_ms())
        .await
        .unwrap();
    assert_eq!(report.deleted, usize::try_from(written).unwrap());
    assert_eq!(store.checkpoint_stats().paced_writes, 0);
}

/// Below the hard limit the writer does no checkpoint work at all, so no
/// write waits for a copy's fsyncs or for another checkpoint's pass, however
/// slow (TOC-60): past the soft limit the database file stays untouched, and
/// with another checkpoint holding the lock, as a slow pass of the
/// checkpointer's does, the writer never waits for it.
#[test]
fn below_the_hard_limit_the_writer_neither_copies_nor_waits() {
    /// More frames than one [`add_row`] appends.
    const ROW_FRAMES: u64 = 20;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DB_FILE);
    let conn = log_of(&path, soft_limit_frames());
    let (mut nudge, _nudges) = Nudge::detached();
    let passes = nudge.passes();
    let before = database_bytes(&path);
    // A checkpoint would let the next commit rewind the log.
    let write = |nudge: &mut Nudge| {
        let frames = checkpoint::log_frames();
        add_row(&conn);
        nudge.after_write(&conn);
        assert!(
            checkpoint::log_frames() > frames,
            "the log rewound below the hard limit"
        );
    };
    let halfway = u64::midpoint(soft_limit_frames(), limit_frames());
    while checkpoint::log_frames() < halfway {
        write(&mut nudge);
    }
    assert_eq!(
        database_bytes(&path),
        before,
        "the writer checkpointed below the hard limit"
    );
    let holder = CheckpointHolder::hold(&path);
    while checkpoint::log_frames() + ROW_FRAMES < limit_frames() {
        write(&mut nudge);
    }
    holder.join();
    assert_eq!(passes.waits(), 0, "the writer waited below the hard limit");
    let stats = nudge.stats();
    assert_eq!(stats.limit_writes, 0, "{stats:?}");
    assert!(stats.frames_high >= halfway, "{stats:?}");
    assert_eq!(passes.polls(), 0, "the writer polled below the hard limit");
}

/// Past the soft limit a write wakes the checkpointer at once, though fewer
/// frames than its threshold are waiting and its minute is not up. Here a
/// read keeps the next commit from rewinding a log the checkpointer has just
/// copied, so the frames after it would otherwise wait a minute.
#[test]
fn past_the_soft_limit_a_write_wakes_the_checkpointer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DB_FILE);
    let conn = log_of(&path, soft_limit_frames() - 200);
    let (checkpointer, mut nudge) =
        Checkpointer::spawn(schema::open_checkpointer(&path).unwrap()).unwrap();
    let passes = nudge.passes();
    // A read of the log as it is: the pass may copy all of it, but no
    // commit can rewind it while the read is open.
    let reader = rusqlite::Connection::open(&path).unwrap();
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader
        .query_row("SELECT count(*) FROM t", [], |row| row.get(0))
        .unwrap();
    // Thousands of frames are waiting, past the threshold.
    nudge.after_write(&conn);
    let deadline = Instant::now() + Duration::from_secs(10);
    while passes.ran() == 0 {
        assert!(Instant::now() < deadline, "the checkpointer never ran");
        std::thread::sleep(Duration::from_millis(10));
    }
    let copied = checkpoint::log_frames();
    add_row(&conn);
    nudge.after_write(&conn);
    assert!(
        checkpoint::log_frames() > copied,
        "the log rewound past an open read"
    );
    reader.execute_batch("COMMIT").unwrap();
    while checkpoint::log_frames() < soft_limit_frames() {
        add_row(&conn);
        nudge.after_write(&conn);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while passes.ran() < 2 {
        assert!(
            Instant::now() < deadline,
            "a write past the soft limit did not wake the checkpointer within 5s"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(nudge);
    checkpointer.join().unwrap();
}

/// A few small writes stay in the log: the checkpointer waits for a
/// threshold's worth of frames (or a minute) rather than copying each one.
#[tokio::test]
async fn small_writes_wait_for_the_threshold() {
    let (dir, store) = open();
    let path = dir.path().join(DB_FILE);
    empty_log(&path);
    // Small writes change pages the file already has, so it is the contents
    // a checkpoint changes, not the size.
    let before = std::fs::read(&path).unwrap();
    for _ in 0..20 {
        store.append(result("small")).await.unwrap();
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        std::fs::read(&path).unwrap() == before,
        "a few small writes were checkpointed"
    );
    assert!(log_bytes(&path) > 0);
    store.close().await.unwrap();
}

/// A checkpoint a reader held back is tried again once the reader is done,
/// without waiting for another write.
#[tokio::test]
async fn a_checkpoint_a_reader_held_back_is_retried_without_writes() {
    let (dir, store) = open();
    let path = dir.path().join(DB_FILE);
    empty_log(&path);
    let reader = rusqlite::Connection::open(&path).unwrap();
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader
        .query_row("SELECT count(*) FROM check_result", [], |row| row.get(0))
        .unwrap();
    let before = database_bytes(&path);
    fill_log(&store.writer).await;
    // Long enough for the checkpointer to have tried while the read is open.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        database_bytes(&path),
        before,
        "the log was copied past an open read"
    );
    reader.execute_batch("COMMIT").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while database_bytes(&path) == before {
        assert!(
            Instant::now() < deadline,
            "the held-back checkpoint was not retried within 5s"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    store.close().await.unwrap();
}

/// A panic that ends the writer thread still closes its connection before
/// it lets go of the checkpointer, so the checkpointer's connection closes
/// last and removes the log. With no checkpointer connection here, the
/// writer's own close removes the log, so the log must be gone by the time
/// the checkpointer's end of the channel sees the writer go.
#[tokio::test]
async fn a_writer_thread_panic_closes_its_connection_first() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join(format!("{DB_FILE}-wal"));
    let (nudge, nudges) = Nudge::detached();
    let writer = Writer::spawn(
        schema::open_writer(&dir.path().join(DB_FILE)).unwrap(),
        nudge,
    )
    .unwrap();
    fill_log(&writer).await;
    let watcher = {
        let log = log.clone();
        std::thread::spawn(move || {
            while nudges.recv().is_ok() {}
            log.exists()
        })
    };
    writer
        .run_unguarded(|_| panic!("a bug outside any one write"))
        .await;
    assert!(writer.close().join().is_err());
    assert!(
        !watcher.join().unwrap(),
        "the writer let go of the checkpointer before closing its connection"
    );
}

/// A checkpointer panic is a bug, so closing fails, naming the thread.
#[test]
fn a_checkpointer_panic_fails_close_by_name() {
    let writer = std::thread::spawn(|| {});
    assert!(matches!(
        crate::join(writer, Checkpointer::panicked()),
        Err(Error::CheckpointerPanicked)
    ));
}

/// The checkpointer's connection checkpoints but cannot write.
#[test]
fn the_checkpointer_connection_refuses_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DB_FILE);
    drop(schema::open_writer(&path).unwrap());
    let conn = schema::open_checkpointer(&path).unwrap();
    let busy: bool = conn
        .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| row.get(0))
        .unwrap();
    assert!(!busy);
    let error = conn
        .execute(
            "INSERT INTO check_result (node_id, check_id, started_at, duration_ms, health)
             VALUES ('node-a', 'c', 1, 1, 'up')",
            [],
        )
        .unwrap_err();
    assert_eq!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::ReadOnly)
    );
}
