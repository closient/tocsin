// SPDX-License-Identifier: AGPL-3.0-only

//! Tests that need the writer's internals.

use std::path::Path;
use std::time::{Duration, Instant};

use rusqlite::{TransactionBehavior, params};

use tocsin_core::incident::{
    Actor, Impact, Incident, IncidentKind, IncidentStatus, NewIncident, NewUpdate, Timestamp,
};

use crate::checkpoint::{self, Checkpointer, Nudge};
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

/// Writes that never pause race every checkpoint the checkpointer runs, so
/// none of them leaves the whole log copied and SQLite never rewinds it.
/// Once the log reaches its limit the writer copies the rest itself, which
/// keeps the log file under twice the limit however long the writes go on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_log_stays_bounded_under_writes_that_never_pause() {
    let (dir, store) = open();
    let path = dir.path().join(DB_FILE);
    let bound = 2 * u64::try_from(checkpoint::LOG_LIMIT_BYTES).unwrap();
    let limit_frames = u64::try_from(checkpoint::LOG_LIMIT_FRAMES).unwrap();
    let mut largest = 0;
    let mut appended = 0;
    while appended < 4 * limit_frames {
        appended = store
            .writer
            .run(|conn| {
                conn.execute(
                    "INSERT INTO check_result (node_id, check_id, started_at, duration_ms, health, error)
                     VALUES ('node-a', 'steady', 1, 1, 'up', hex(randomblob(1500)))",
                    [],
                )?;
                Ok(checkpoint::appended())
            })
            .await
            .unwrap();
        largest = largest.max(log_bytes(&path));
    }
    assert!(
        largest < bound,
        "the log file reached {largest} bytes, over {bound}"
    );
    store.close().await.unwrap();
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
