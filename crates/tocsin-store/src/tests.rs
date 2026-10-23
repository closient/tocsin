// SPDX-License-Identifier: AGPL-3.0-only

//! Tests that need the writer's internals.

use std::time::Duration;

use rusqlite::{TransactionBehavior, params};

use crate::{DB_FILE, Store};

/// WAL is what keeps readers unblocked: a reader runs to completion, and sees
/// the last committed state, while the writer holds an uncommitted delete
/// under the strongest lock SQLite has. Under a rollback journal an
/// EXCLUSIVE transaction shuts every reader out, and the read below times out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readers_are_not_blocked_by_an_uncommitted_delete() {
    const ROWS: i64 = 20_000;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join(DB_FILE)).unwrap();
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
