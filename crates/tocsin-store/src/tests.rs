// SPDX-License-Identifier: AGPL-3.0-only

//! Tests that need the writer's internals.

use std::time::Duration;

use rusqlite::{TransactionBehavior, params};

use crate::{DB_FILE, Day, Error, NewCheckResult, Retention, Store, now_ms};

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
