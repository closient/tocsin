// SPDX-License-Identifier: AGPL-3.0-only

//! Behaviour of the history store through its public API.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rusqlite::{Connection, params};
use tocsin_core::Health;
use tocsin_store::{
    AlertRouting, AlertTarget, Day, LastUp, LatestResult, NewCheckResult, NewOutage, OutageChange,
    OutageEnd, Retention, Store, SubjectKind, latest_schema_version,
};

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
            subject_kind: SubjectKind::Check,
            check_id: "web".into(),
            component_id: None,
            health: Health::Up,
            started_at_ms: 0,
            alerted: true,
        })
        .await;
    assert!(up_outage.is_err());
}

#[tokio::test]
async fn an_outage_only_ever_gets_ended_once() {
    let fixture = open();
    let outage = || NewOutage {
        node_id: "node-a".into(),
        subject_kind: SubjectKind::Check,
        check_id: "web".into(),
        component_id: Some("website".into()),
        health: Health::Down,
        started_at_ms: 5_000,
        alerted: true,
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
    let error = conn
        .execute("UPDATE outage SET alerted = 0 WHERE id = ?1", [id])
        .expect_err("changed whether an outage alerted")
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

#[tokio::test]
async fn open_outages_lists_only_this_nodes_unended_outages() {
    let fixture = open();
    let outage = |node: &str, check: &str, health| NewOutage {
        node_id: node.into(),
        subject_kind: SubjectKind::Check,
        check_id: check.into(),
        component_id: None,
        health,
        started_at_ms: 5_000,
        // Only the degraded one opened in maintenance.
        alerted: health == Health::Down,
    };
    let ended = fixture
        .store
        .open_outage(outage("node-a", "api", Health::Down))
        .await
        .unwrap();
    fixture.store.end_outage(ended, 6_000).await.unwrap();
    let web = fixture
        .store
        .open_outage(outage("node-a", "web", Health::Degraded))
        .await
        .unwrap();
    let api = fixture
        .store
        .open_outage(outage("node-a", "api", Health::Down))
        .await
        .unwrap();
    fixture
        .store
        .open_outage(outage("node-b", "web", Health::Down))
        .await
        .unwrap();
    let open = fixture.store.open_outages("node-a").await.unwrap();
    let summary: Vec<_> = open
        .iter()
        .map(|outage| {
            (
                outage.id,
                outage.check_id.as_str(),
                outage.health,
                outage.ended_at_ms,
                outage.alerted,
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (api, "api", Health::Down, None, true),
            (web, "web", Health::Degraded, None, false)
        ]
    );
    assert_eq!(fixture.store.open_outages("node-c").await.unwrap(), []);
}

#[tokio::test]
async fn changing_an_outage_ends_and_opens_together_or_not_at_all() {
    let fixture = open();
    let outage = |check: &str, health, started_at_ms| NewOutage {
        node_id: "node-a".into(),
        subject_kind: SubjectKind::Check,
        check_id: check.into(),
        component_id: None,
        health,
        started_at_ms,
        alerted: true,
    };
    let degraded = fixture
        .store
        .open_outage(outage("web", Health::Degraded, 1_000))
        .await
        .unwrap();
    let down = fixture
        .store
        .change_outage(OutageChange {
            end: Some(OutageEnd {
                id: degraded,
                ended_at_ms: 2_000,
            }),
            open: Some(outage("web", Health::Down, 2_000)),
        })
        .await
        .unwrap()
        .expect("opened one");
    let open = fixture.store.open_outages("node-a").await.unwrap();
    assert_eq!(
        open.iter().map(|outage| outage.id).collect::<Vec<_>>(),
        [down]
    );

    // The open half fails (an outage is never up), so the end is undone too.
    let failed = fixture
        .store
        .change_outage(OutageChange {
            end: Some(OutageEnd {
                id: down,
                ended_at_ms: 3_000,
            }),
            open: Some(outage("web", Health::Up, 3_000)),
        })
        .await;
    assert!(failed.is_err());
    let open = fixture.store.open_outages("node-a").await.unwrap();
    assert_eq!(
        open.iter()
            .map(|outage| (outage.id, outage.ended_at_ms))
            .collect::<Vec<_>>(),
        [(down, None)],
        "a failed change left the outage ended"
    );

    // Ending alone resolves with no new id.
    let ended = fixture
        .store
        .change_outage(OutageChange {
            end: Some(OutageEnd {
                id: down,
                ended_at_ms: 3_000,
            }),
            open: None,
        })
        .await
        .unwrap();
    assert_eq!(ended, None);
    assert_eq!(fixture.store.open_outages("node-a").await.unwrap(), []);
}

#[tokio::test]
async fn a_result_is_stored_with_its_outage_change_or_not_at_all() {
    let fixture = open();
    let down = NewOutage {
        node_id: "node-a".into(),
        subject_kind: SubjectKind::Heartbeat,
        check_id: "backup".into(),
        component_id: None,
        health: Health::Down,
        started_at_ms: 1_000,
        alerted: true,
    };
    let opened = fixture
        .store
        .record(
            result("backup", 1_000, Health::Down),
            SubjectKind::Heartbeat,
            Some(OutageChange {
                end: None,
                open: Some(down.clone()),
            }),
        )
        .await
        .unwrap()
        .expect("opened one");

    // The change fails (the outage is already open), so the result goes too.
    let failed = fixture
        .store
        .record(
            result("backup", 2_000, Health::Down),
            SubjectKind::Heartbeat,
            Some(OutageChange {
                end: None,
                open: Some(NewOutage {
                    started_at_ms: 2_000,
                    ..down
                }),
            }),
        )
        .await;
    assert!(failed.is_err());
    assert_eq!(
        count_results(&fixture.path),
        1,
        "a result without its change"
    );
    let open = fixture.store.open_outages("node-a").await.unwrap();
    assert_eq!(
        open.iter().map(|outage| outage.id).collect::<Vec<_>>(),
        [opened]
    );
}

#[tokio::test]
async fn an_alerting_result_is_stored_with_its_change_and_deliveries_or_not_at_all() {
    let fixture = open();
    let routing = || AlertRouting {
        targets: vec![AlertTarget {
            notifier_id: "slack".into(),
            route_id: "ops".into(),
            repeat_ms: None,
        }],
        ringing: Vec::new(),
    };
    let down = NewOutage {
        node_id: "node-a".into(),
        subject_kind: SubjectKind::Heartbeat,
        check_id: "backup".into(),
        component_id: None,
        health: Health::Down,
        started_at_ms: 1_000,
        alerted: true,
    };
    let change = |down: NewOutage| OutageChange {
        end: None,
        open: Some(down),
    };
    let alerts = fixture
        .store
        .record_alerting(
            result("backup", 1_000, Health::Down),
            SubjectKind::Heartbeat,
            change(down.clone()),
            Some(routing()),
            1_000,
        )
        .await
        .unwrap();
    assert!(alerts.opened.is_some());
    assert_eq!(alerts.queued, 1, "the fire is queued with the result");
    assert_eq!(count_results(&fixture.path), 1);

    // The change fails (the outage is already open): no result, no alert.
    let failed = fixture
        .store
        .record_alerting(
            result("backup", 2_000, Health::Down),
            SubjectKind::Heartbeat,
            change(NewOutage {
                started_at_ms: 2_000,
                ..down
            }),
            Some(routing()),
            2_000,
        )
        .await;
    assert!(failed.is_err());
    assert_eq!(
        count_results(&fixture.path),
        1,
        "a result without its change"
    );
    assert_eq!(fixture.store.alerts().await.unwrap().len(), 1);
    assert_eq!(fixture.store.outbox().await.unwrap().pending.len(), 1);
}

#[tokio::test]
async fn last_up_is_the_latest_up_result_since_the_last_outage() {
    let fixture = open();
    let on = |node: &str, check: &str, started_at_ms, health| NewCheckResult {
        node_id: node.into(),
        ..result(check, started_at_ms, health)
    };
    for (node, check, at, health) in [
        ("node-a", "web", 1_000, Health::Up),
        ("node-a", "web", 2_000, Health::Up),
        ("node-a", "web", 3_000, Health::Down),
        ("node-a", "api", 1_500, Health::Up),
        ("node-a", "api", 2_500, Health::Up),
        ("node-a", "db", 1_000, Health::Down),
        ("node-b", "web", 9_000, Health::Up),
    ] {
        fixture
            .store
            .append(on(node, check, at, health))
            .await
            .unwrap();
    }
    let api = fixture
        .store
        .open_outage(NewOutage {
            node_id: "node-a".into(),
            subject_kind: SubjectKind::Check,
            check_id: "api".into(),
            component_id: None,
            health: Health::Down,
            started_at_ms: 1_700,
            alerted: true,
        })
        .await
        .unwrap();
    fixture.store.end_outage(api, 2_500).await.unwrap();

    let checks = ["web", "api", "db", "gone"].map(String::from).to_vec();
    let found = fixture.store.last_up("node-a", checks).await.unwrap();
    assert_eq!(
        found,
        [
            LastUp {
                check_id: "web".into(),
                last_up_ms: 2_000,
                since_ms: 1_000,
            },
            LastUp {
                check_id: "api".into(),
                last_up_ms: 2_500,
                since_ms: 2_500,
            },
        ]
    );
}

#[tokio::test]
async fn latest_results_are_the_newest_result_of_any_health_on_the_node() {
    let fixture = open();
    for (node, check, at, health) in [
        ("node-a", "backup", 1_000, Health::Up),
        ("node-a", "backup", 3_000, Health::Down),
        ("node-a", "backup", 2_000, Health::Up),
        ("node-a", "web", 1_500, Health::Up),
        ("node-b", "web", 9_000, Health::Up),
    ] {
        let row = NewCheckResult {
            node_id: node.into(),
            ..result(check, at, health)
        };
        fixture.store.append(row).await.unwrap();
    }
    let ids = ["backup", "web", "never"].map(String::from).to_vec();
    let found = fixture.store.latest_results("node-a", ids).await.unwrap();
    assert_eq!(
        found,
        [
            LatestResult {
                check_id: "backup".into(),
                started_at_ms: 3_000,
            },
            LatestResult {
                check_id: "web".into(),
                started_at_ms: 1_500,
            },
        ]
    );
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
async fn a_heartbeat_rollup_weighs_each_result_by_the_time_it_held() {
    let fixture = open();
    let first = day("2026-10-01");
    let hour = 3_600_000;
    // Sixty pings in the first hour, then a miss every hour for the other 23:
    // counted, the day would be 60 / 83 up; held, it is one hour in 24.
    for minute in 0..60 {
        fixture
            .store
            .record(
                result("backup", first.start_ms() + minute * 60_000, Health::Up),
                SubjectKind::Heartbeat,
                None,
            )
            .await
            .unwrap();
    }
    for h in 1..24 {
        fixture
            .store
            .record(
                result("backup", first.start_ms() + h * hour, Health::Down),
                SubjectKind::Heartbeat,
                None,
            )
            .await
            .unwrap();
    }
    // A check on the same day keeps counting results.
    fixture
        .store
        .append(result("web", first.start_ms() + hour, Health::Up))
        .await
        .unwrap();

    let report = fixture.store.rollup_since(first).await.unwrap();
    assert_eq!(
        report.rows, 2,
        "the heartbeat's results are not counted as a check's too"
    );
    let rolled = fixture.store.daily_rollups(first).await.unwrap();
    let backup = rolled.iter().find(|r| r.check_id == "backup").unwrap();
    assert_eq!(backup.kind, SubjectKind::Heartbeat);
    assert_eq!(backup.total, 24 * hour);
    assert_eq!(
        (backup.up, backup.degraded, backup.down),
        (hour, 0, 23 * hour)
    );
    assert_eq!(backup.worst, Health::Down);
    assert_eq!(backup.up * 24, backup.total, "one hour in 24, about 4% up");
    let web = rolled.iter().find(|r| r.check_id == "web").unwrap();
    assert_eq!((web.kind, web.total, web.up), (SubjectKind::Check, 1, 1));
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
    // A pass cut off after its first 100-row chunk (serve stopping, say).
    let slow = Retention {
        days: 3,
        chunk_rows: 100,
        pause: Duration::from_secs(60),
    };
    let cut = tokio::time::timeout(
        Duration::from_secs(1),
        fixture.store.apply_retention(slow, now),
    )
    .await;
    assert!(cut.is_err(), "the pass was meant to be cut off");
    assert_eq!(count_results(&fixture.path), 900);
    let report = fixture
        .store
        .apply_retention(Retention::days(3), now)
        .await
        .unwrap();
    assert_eq!((report.rolled_up, report.deleted), (0, 600));
    assert_eq!(count_results(&fixture.path), 300);
}

/// Once retention has finalized a day, a result for it is refused rather
/// than stored and later deleted without ever being counted.
#[tokio::test]
async fn a_result_for_a_finalized_day_is_refused() {
    let fixture = open();
    let first = day("2026-10-01");
    seed(&fixture.path, "web", first.start_ms(), 864_000, 1000);
    let now = first.plus(10).start_ms();
    fixture
        .store
        .apply_retention(Retention::days(3), now)
        .await
        .unwrap();
    let late = fixture
        .store
        .append(result("web", first.plus(6).start_ms() + 5, Health::Down))
        .await;
    let error = late.expect_err("a result for a finalized day was stored");
    assert!(error.to_string().contains("retention watermark"), "{error}");
    // The first day kept is still open.
    fixture
        .store
        .append(result("web", first.plus(7).start_ms(), Health::Down))
        .await
        .unwrap();
}

/// Holds SQLite's write lock from a second connection, as `tocsin db rollup`
/// or a hand-run `sqlite3` beside `serve` would, with a commit in it, for
/// `hold`. Returns once the lock is held.
fn hold_write_lock(path: &Path, hold: Duration) -> std::thread::JoinHandle<()> {
    let conn = raw(path);
    let (locked, lock_taken) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        conn.execute_batch(&format!(
            "BEGIN IMMEDIATE;
             INSERT INTO check_result (node_id, check_id, started_at, duration_ms, health)
             VALUES ('node-b', 'web', {}, 1, 'up');",
            tocsin_store::now_ms()
        ))
        .unwrap();
        locked.send(()).unwrap();
        std::thread::sleep(hold);
        conn.execute_batch("COMMIT").unwrap();
    });
    lock_taken.recv().unwrap();
    thread
}

/// Rollups and retention read before they write. In a DEFERRED transaction
/// that upgrade fails with `SQLITE_BUSY` without waiting when another
/// connection holds the write lock; IMMEDIATE waits for it like any write.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rollups_and_retention_wait_for_another_process_writing() {
    let fixture = open();
    let first = day("2026-10-01");
    seed(&fixture.path, "web", first.start_ms(), 864_000, 1000);

    let other = hold_write_lock(&fixture.path, Duration::from_millis(300));
    let rolled = fixture.store.rollup_since(first).await;
    other.join().unwrap();
    assert!(rolled.is_ok(), "{rolled:?}");

    let other = hold_write_lock(&fixture.path, Duration::from_millis(300));
    let retained = fixture
        .store
        .apply_retention(Retention::days(3), first.plus(10).start_ms())
        .await;
    other.join().unwrap();
    assert_eq!(retained.unwrap().deleted, 700);
}

/// A rollup dated in the future (a node with a wrong clock, before appends
/// were bounded) must not freeze the hourly recompute of earlier days.
#[tokio::test]
async fn a_future_dated_rollup_does_not_stop_todays_recompute() {
    let fixture = open();
    raw(&fixture.path)
        .execute(
            "INSERT INTO daily_rollup (check_id, node_id, date, total, up, degraded, down, worst)
             VALUES ('web', 'node-a', '2099-01-01', 1, 1, 0, 0, 'up')",
            [],
        )
        .unwrap();
    let now = tocsin_store::now_ms();
    fixture
        .store
        .append(result("web", now, Health::Degraded))
        .await
        .unwrap();
    fixture.store.rollup_recent().await.unwrap();
    let today = Day::containing(now);
    let rolled = fixture.store.daily_rollups(today).await.unwrap();
    assert!(
        rolled
            .iter()
            .any(|r| r.date == today && r.worst == Health::Degraded),
        "today was not rolled up: {rolled:?}"
    );
}

#[tokio::test]
async fn a_result_far_in_the_future_is_refused() {
    let fixture = open();
    let tolerance = i64::try_from(tocsin_store::FUTURE_TOLERANCE.as_millis()).unwrap();
    let now = tocsin_store::now_ms();
    let refused = fixture
        .store
        .append(result("web", now + tolerance + 60_000, Health::Up))
        .await;
    assert!(
        matches!(refused, Err(tocsin_store::Error::FutureResult { .. })),
        "{refused:?}"
    );
    // Clock skew inside the tolerance is fine.
    fixture
        .store
        .append(result("web", now + 60_000, Health::Up))
        .await
        .unwrap();
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

/// A read whose caller stops waiting (a timeout here; a `select!` or an
/// abort is the same) still returns its connection when the query ends.
/// More abandoned reads than the pool has connections, then a normal read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abandoned_reads_return_their_connections() {
    let fixture = open();
    for _ in 0..10 {
        let slow = fixture.store.read(|_| {
            std::thread::sleep(Duration::from_millis(50));
            Ok(())
        });
        if let Ok(finished) = tokio::time::timeout(Duration::from_millis(1), slow).await {
            finished.expect("a read failed after earlier reads were abandoned");
        }
    }
    let after = tokio::time::timeout(
        Duration::from_secs(5),
        fixture
            .store
            .read(|conn| Ok(conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))?)),
    )
    .await
    .expect("a read waited on connections lost to abandoned reads");
    assert_eq!(after.unwrap(), 1);
}

/// A panicking query is reported as an error and its connection replaced, so
/// panics, however many, never starve the pool and `read` never hangs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panicking_reads_are_errors_and_do_not_shrink_the_pool() {
    let fixture = open();
    for _ in 0..10 {
        let panicked = tokio::time::timeout(
            Duration::from_secs(5),
            fixture
                .store
                .read::<(), _>(|_| panic!("a query bug (expected in this test)")),
        )
        .await
        .expect("a read hung after earlier reads panicked");
        assert!(matches!(panicked, Err(tocsin_store::Error::ReaderPanicked)));
    }
    let after = tokio::time::timeout(
        Duration::from_secs(5),
        fixture
            .store
            .read(|conn| Ok(conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))?)),
    )
    .await
    .expect("a read hung after earlier reads panicked");
    assert_eq!(after.unwrap(), 1);
}
