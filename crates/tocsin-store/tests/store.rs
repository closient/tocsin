// SPDX-License-Identifier: AGPL-3.0-only

//! Behaviour of the history store through its public API.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rusqlite::{Connection, params};
use tocsin_core::Health;
use tocsin_store::{Day, NewCheckResult, NewOutage, Retention, Store, latest_schema_version};

fn day(text: &str) -> Day {
    text.parse().unwrap()
}

fn result(check: &str, started_at_ms: i64, health: Health) -> NewCheckResult {
    NewCheckResult {
        node_id: "node-a".into(),
        check_id: check.into(),
        started_at_ms,
        duration_ms: 12,
        health,
        detail: Some(serde_json::json!({ "status": { "expected": 200, "got": 200 } })),
        error: None,
        status_code: Some(200),
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
    store: Store,
}

fn open() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(tocsin_store::DB_FILE);
    let store = Store::open(&path).unwrap();
    Fixture {
        _dir: dir,
        path,
        store,
    }
}

/// A second, raw connection: what a buggy caller or a hand-run `sqlite3`
/// would use.
fn raw(path: &Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    conn.busy_timeout(Duration::from_secs(5)).unwrap();
    conn
}

/// Bulk-inserts `count` results for `check`, `step_ms` apart from `from_ms`,
/// cycling up, up, degraded, down.
fn seed(path: &Path, check: &str, from_ms: i64, step_ms: i64, count: i64) {
    let mut conn = raw(path);
    let transaction = conn.transaction().unwrap();
    {
        let mut insert = transaction
            .prepare(
                "INSERT INTO check_result (node_id, check_id, started_at, duration_ms, health)
                 VALUES ('node-a', ?1, ?2, 5, ?3)",
            )
            .unwrap();
        for i in 0..count {
            let health = ["up", "up", "degraded", "down"][usize::try_from(i % 4).unwrap()];
            insert
                .execute(params![check, from_ms + i * step_ms, health])
                .unwrap();
        }
    }
    transaction.commit().unwrap();
}

fn count_results(path: &Path) -> i64 {
    raw(path)
        .query_row("SELECT count(*) FROM check_result", [], |row| row.get(0))
        .unwrap()
}

#[tokio::test]
async fn opens_in_wal_mode_at_the_latest_schema_and_reopens() {
    let fixture = open();
    let mode: String = fixture
        .store
        .read(|conn| Ok(conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?))
        .await
        .unwrap();
    assert_eq!(mode, "wal");
    let info = fixture.store.info().await.unwrap();
    assert_eq!(
        info.schema_version,
        i64::try_from(latest_schema_version()).unwrap()
    );
    let Fixture { _dir, path, store } = fixture;
    store.close().await.unwrap();
    let again = Store::open(&path).unwrap();
    assert_eq!(
        again.info().await.unwrap().schema_version,
        info.schema_version
    );
    again.close().await.unwrap();
}

#[tokio::test]
async fn appends_and_reads_back() {
    let fixture = open();
    let id = fixture
        .store
        .append(result("web", 1_000, Health::Degraded))
        .await
        .unwrap();
    let row: (String, String, i64, String, String, Option<i64>) = fixture
        .store
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT node_id, check_id, started_at, health, detail, status_code
                 FROM check_result WHERE id = ?1",
                [id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )?)
        })
        .await
        .unwrap();
    assert_eq!(row.0, "node-a");
    assert_eq!(row.1, "web");
    assert_eq!(row.2, 1_000);
    assert_eq!(row.3, "degraded");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&row.4).unwrap()["status"]["got"],
        200
    );
    assert_eq!(row.5, Some(200));
}

#[tokio::test]
async fn read_connections_refuse_writes() {
    let fixture = open();
    let attempt = fixture
        .store
        .read(|conn| Ok(conn.execute("DELETE FROM check_result", [])?))
        .await;
    assert!(attempt.is_err(), "a reader wrote");
}

#[tokio::test]
async fn check_results_cannot_be_updated_but_can_be_deleted() {
    let fixture = open();
    let id = fixture
        .store
        .append(result("web", 1_000, Health::Down))
        .await
        .unwrap();
    let conn = raw(&fixture.path);
    let update = conn.execute("UPDATE check_result SET health = 'up' WHERE id = ?1", [id]);
    let error = update
        .expect_err("an UPDATE of check_result succeeded")
        .to_string();
    assert!(error.contains("append-only"), "{error}");
    let health: String = conn
        .query_row(
            "SELECT health FROM check_result WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(health, "down");
    assert_eq!(
        conn.execute("DELETE FROM check_result WHERE id = ?1", [id])
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn schema_rejects_values_outside_the_enums() {
    let fixture = open();
    let conn = raw(&fixture.path);
    let bad = conn.execute(
        "INSERT INTO check_result (node_id, check_id, started_at, duration_ms, health)
         VALUES ('n', 'c', 0, 0, 'sideways')",
        [],
    );
    assert!(bad.is_err());
    let empty_id = fixture.store.append(result("", 0, Health::Up)).await;
    assert!(empty_id.is_err());
    let up_outage = fixture
        .store
        .open_outage(NewOutage {
            node_id: "node-a".into(),
            check_id: "web".into(),
            component_id: None,
            health: Health::Up,
            started_at_ms: 0,
        })
        .await;
    assert!(up_outage.is_err());
}

#[tokio::test]
async fn an_outage_only_ever_gets_ended_once() {
    let fixture = open();
    let outage = || NewOutage {
        node_id: "node-a".into(),
        check_id: "web".into(),
        component_id: Some("website".into()),
        health: Health::Down,
        started_at_ms: 5_000,
    };
    let id = fixture.store.open_outage(outage()).await.unwrap();
    assert!(
        fixture.store.open_outage(outage()).await.is_err(),
        "two open outages for one check on one node"
    );
    let conn = raw(&fixture.path);
    let error = conn
        .execute("UPDATE outage SET health = 'degraded' WHERE id = ?1", [id])
        .expect_err("changed an outage's health")
        .to_string();
    assert!(error.contains("ended_at set, once"), "{error}");
    assert!(
        fixture.store.end_outage(id, 1_000).await.is_err(),
        "ended before it started"
    );
    fixture.store.end_outage(id, 9_000).await.unwrap();
    assert!(
        fixture.store.end_outage(id, 10_000).await.is_err(),
        "ended twice"
    );
    assert!(
        conn.execute("UPDATE outage SET ended_at = NULL WHERE id = ?1", [id])
            .is_err(),
        "reopened"
    );
    assert!(fixture.store.end_outage(id + 100, 9_000).await.is_err());
    // Once ended, the check can have a new outage.
    fixture.store.open_outage(outage()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_appends_batch_and_a_bad_row_spoils_only_itself() {
    let fixture = open();
    let store = Arc::new(fixture.store);
    let tasks: Vec<_> = (0..400)
        .map(|i| {
            let store = Arc::clone(&store);
            let check = if i == 200 {
                String::new()
            } else {
                format!("check-{i}")
            };
            tokio::spawn(async move { store.append(result(&check, i, Health::Up)).await })
        })
        .collect();
    let mut ids = BTreeSet::new();
    let mut failures = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(id) => assert!(ids.insert(id)),
            Err(_) => failures += 1,
        }
    }
    assert_eq!(failures, 1);
    assert_eq!(ids.len(), 399);
    assert_eq!(count_results(&fixture.path), 399);
}

#[tokio::test]
async fn rollups_are_recomputable_from_raw_results() {
    let fixture = open();
    let first = day("2026-10-01");
    // Three UTC days of results for two checks, one ending on a day boundary.
    seed(&fixture.path, "web", first.start_ms(), 60_000, 3 * 1440);
    seed(
        &fixture.path,
        "api",
        first.plus(1).start_ms() - 30_000,
        3_600_000,
        30,
    );
    fixture
        .store
        .append(result("web", first.plus(2).start_ms() + 5, Health::Down))
        .await
        .unwrap();

    let report = fixture.store.rollup_since(first).await.unwrap();
    assert_eq!(report.since, Some(first));
    let rolled = fixture.store.daily_rollups(first).await.unwrap();
    assert_eq!(report.rows, rolled.len());
    // web: 3 days; api: one result on 2026-10-01, 24 on the 2nd, 5 on the 3rd.
    assert_eq!(rolled.len(), 6);
    let web_first = rolled
        .iter()
        .find(|r| r.check_id == "web" && r.date == first)
        .unwrap();
    assert_eq!(
        (
            web_first.total,
            web_first.up,
            web_first.degraded,
            web_first.down
        ),
        (1440, 720, 360, 360)
    );
    assert_eq!(web_first.worst, Health::Down);
    let api_first = rolled
        .iter()
        .find(|r| r.check_id == "api" && r.date == first)
        .unwrap();
    assert_eq!((api_first.total, api_first.worst), (1, Health::Up));
    let web_third = rolled
        .iter()
        .find(|r| r.check_id == "web" && r.date == first.plus(2))
        .unwrap();
    assert_eq!(web_third.total, 1441);

    raw(&fixture.path)
        .execute("DELETE FROM daily_rollup", [])
        .unwrap();
    assert_eq!(
        fixture.store.daily_rollups(first).await.unwrap(),
        Vec::new()
    );
    fixture.store.rollup_since(first).await.unwrap();
    assert_eq!(fixture.store.daily_rollups(first).await.unwrap(), rolled);
    // Idempotent.
    fixture.store.rollup_since(first).await.unwrap();
    assert_eq!(fixture.store.daily_rollups(first).await.unwrap(), rolled);
}

#[tokio::test]
async fn hourly_rollup_resumes_from_the_latest_rolled_day() {
    let fixture = open();
    let first = day("2026-10-01");
    seed(&fixture.path, "web", first.start_ms(), 3_600_000, 24);
    let report = fixture.store.rollup_recent().await.unwrap();
    assert_eq!((report.since, report.rows), (Some(first), 1));
    // More results that day and the next: the partial day is recomputed.
    seed(&fixture.path, "web", first.start_ms() + 1, 3_600_000, 48);
    let report = fixture.store.rollup_recent().await.unwrap();
    assert_eq!((report.since, report.rows), (Some(first), 2));
    let rolled = fixture.store.daily_rollups(first).await.unwrap();
    assert_eq!(rolled.iter().map(|r| r.total).collect::<Vec<_>>(), [48, 24]);
}

#[tokio::test]
async fn retention_rolls_up_before_deleting_and_never_rewrites_finalized_days() {
    let fixture = open();
    let first = day("2026-10-01");
    // Ten days, 100 results a day.
    seed(&fixture.path, "web", first.start_ms(), 864_000, 1000);
    let now = first.plus(10).start_ms() + 3_600_000;
    let report = fixture
        .store
        .apply_retention(Retention::days(3), now)
        .await
        .unwrap();
    // Today is day 10; days 7, 8 and 9 are kept whole.
    let cutoff = first.plus(7).start_ms();
    assert_eq!(report.purged_before_ms, Some(cutoff));
    assert_eq!(report.rolled_up, 7);
    assert_eq!(report.deleted, 700);
    assert_eq!(count_results(&fixture.path), 300);
    let oldest = fixture.store.info().await.unwrap().oldest_result_ms;
    assert_eq!(oldest, Some(cutoff));

    // Recomputing from the start leaves the purged days' rollups intact.
    let recompute = fixture.store.rollup_since(first).await.unwrap();
    assert_eq!(recompute.since, Some(first.plus(7)));
    let rolled = fixture.store.daily_rollups(first).await.unwrap();
    assert_eq!(rolled.len(), 10);
    assert!(rolled.iter().all(|r| r.total == 100), "{rolled:?}");

    // A second pass in the same day has nothing to do.
    let again = fixture
        .store
        .apply_retention(Retention::days(3), now)
        .await
        .unwrap();
    assert_eq!((again.rolled_up, again.deleted), (0, 0));
}

#[tokio::test]
async fn retention_finishes_a_pass_that_stopped_part_way() {
    let fixture = open();
    let first = day("2026-10-01");
    seed(&fixture.path, "web", first.start_ms(), 864_000, 1000);
    let now = first.plus(10).start_ms();
    fixture
        .store
        .apply_retention(Retention::days(3), now)
        .await
        .unwrap();
    // Simulate rows left behind by an interrupted delete.
    seed(&fixture.path, "web", first.start_ms(), 864_000, 50);
    let report = fixture
        .store
        .apply_retention(Retention::days(3), now)
        .await
        .unwrap();
    assert_eq!((report.rolled_up, report.deleted), (0, 50));
}

/// Retention deletes in chunks through the writer: a reader sees the count
/// fall in steps rather than all at once, and appends queued meanwhile are
/// committed between chunks instead of waiting for the whole purge.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_deletes_in_chunks_while_reads_and_writes_continue() {
    const OLD: i64 = 60_000;
    let fixture = open();
    let first = day("2026-01-01");
    seed(&fixture.path, "old", first.start_ms(), 1_000, OLD);
    let now = first.plus(100).start_ms();
    let store = Arc::new(fixture.store);
    let done = Arc::new(AtomicBool::new(false));

    let reader = {
        let (store, done) = (Arc::clone(&store), Arc::clone(&done));
        let cutoff = first.plus(10).start_ms();
        tokio::spawn(async move {
            let mut seen = BTreeSet::new();
            let mut reads = 0;
            while !done.load(Ordering::Acquire) {
                let old: i64 = store
                    .read(move |conn| {
                        Ok(conn.query_row(
                            "SELECT count(*) FROM check_result WHERE started_at < ?1",
                            [cutoff],
                            |row| row.get(0),
                        )?)
                    })
                    .await
                    .unwrap();
                seen.insert(old);
                reads += 1;
            }
            (seen, reads)
        })
    };
    let writer = {
        let (store, done) = (Arc::clone(&store), Arc::clone(&done));
        tokio::spawn(async move {
            let mut acked = Vec::new();
            while !done.load(Ordering::Acquire) {
                store
                    .append(result("fresh", now, Health::Up))
                    .await
                    .unwrap();
                acked.push(Instant::now());
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            acked
        })
    };

    let policy = Retention {
        days: 30,
        chunk_rows: 500,
        pause: Duration::from_millis(2),
    };
    let started = Instant::now();
    let report = store.apply_retention(policy, now).await.unwrap();
    let finished = Instant::now();
    done.store(true, Ordering::Release);
    let (seen, reads) = reader.await.unwrap();
    let acked = writer.await.unwrap();

    assert_eq!(report.deleted, usize::try_from(OLD).unwrap());
    assert_eq!(report.chunks, 120);
    let intermediate = seen.iter().filter(|&&n| n > 0 && n < OLD).count();
    assert!(
        intermediate >= 10,
        "a reader saw only {intermediate} intermediate counts in {reads} reads; the delete was not chunked"
    );
    let interleaved = acked
        .iter()
        .filter(|&&at| at > started && at < finished)
        .count();
    assert!(
        interleaved >= 10,
        "only {interleaved} appends committed during the purge"
    );
}

#[tokio::test]
async fn a_database_from_a_newer_build_is_refused() {
    let fixture = open();
    let path = fixture.path.clone();
    fixture.store.close().await.unwrap();
    raw(&path).pragma_update(None, "user_version", 99).unwrap();
    assert!(Store::open(&path).is_err());
}
