// SPDX-License-Identifier: AGPL-3.0-only

//! Applying batches other nodes push (docs/design/federation.md).

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::Connection;
use rusqlite::types::Value;
use tocsin_core::Health;
use tocsin_core::status::OutageEnding;
use tocsin_store::{
    DailyRollup, Day, Error, NewCheckResult, NewOutage, RemoteBatch, RemoteOutageChange,
    RemoteResult, Store, SubjectKind,
};

const LOCAL: &str = "node-local";
const REMOTE: &str = "node-b";
const STORE: &str = "0123456789abcdef0123456789abcdef";

fn day(text: &str) -> Day {
    text.parse().unwrap()
}

/// Noon on 2026-01-05, the day the batches arrive.
fn noon() -> i64 {
    day("2026-01-05").start_ms() + 12 * 3_600_000
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

fn raw(path: &Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    conn.busy_timeout(Duration::from_secs(5)).unwrap();
    conn
}

fn count(path: &Path, sql: &str) -> i64 {
    raw(path).query_row(sql, [], |row| row.get(0)).unwrap()
}

/// Every row of every table a batch writes, in a stable order.
fn dump(path: &Path) -> Vec<Vec<Value>> {
    let conn = raw(path);
    let mut rows = Vec::new();
    for sql in [
        "SELECT * FROM check_result ORDER BY id",
        "SELECT * FROM outage ORDER BY id",
        "SELECT * FROM daily_rollup ORDER BY check_id, node_id, date",
        "SELECT * FROM node_store ORDER BY id",
    ] {
        let mut statement = conn.prepare(sql).unwrap();
        let columns = statement.column_count();
        let found = statement
            .query_map([], |row| {
                (0..columns)
                    .map(|index| row.get::<_, Value>(index))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        rows.push(vec![Value::Text(sql.to_owned())]);
        rows.extend(found);
    }
    rows
}

fn result(origin_id: i64, started_at_ms: i64) -> RemoteResult {
    RemoteResult {
        origin_id,
        kind: SubjectKind::Check,
        result: NewCheckResult {
            node_id: REMOTE.into(),
            check_id: "web".into(),
            started_at_ms,
            duration_ms: 12,
            health: Health::Down,
            detail: Some(serde_json::json!({ "status": { "expected": 200, "got": 503 } })),
            error: Some("503".into()),
            status_code: Some(503),
        },
    }
}

fn open_change(seq: i64, origin_id: i64, started_at_ms: i64) -> RemoteOutageChange {
    RemoteOutageChange::Open {
        seq,
        origin_id,
        outage: NewOutage {
            node_id: REMOTE.into(),
            subject_kind: SubjectKind::Check,
            check_id: "web".into(),
            component_id: Some("website".into()),
            health: Health::Down,
            started_at_ms,
            alerted: true,
        },
    }
}

fn end_change(seq: i64, origin_id: i64, ended_at_ms: i64) -> RemoteOutageChange {
    RemoteOutageChange::End {
        seq,
        origin_id,
        ended_at_ms,
        ending: OutageEnding::Recovered,
    }
}

fn rollup(date: &str, up: i64, down: i64) -> DailyRollup {
    DailyRollup {
        check_id: "web".into(),
        node_id: REMOTE.into(),
        date: day(date),
        kind: SubjectKind::Check,
        total: up + down,
        up,
        degraded: 0,
        down,
        worst: if down > 0 { Health::Down } else { Health::Up },
    }
}

fn batch(
    results: Vec<RemoteResult>,
    outages: Vec<RemoteOutageChange>,
    rollups: Vec<DailyRollup>,
) -> RemoteBatch {
    RemoteBatch {
        node_id: REMOTE.into(),
        store_id: STORE.into(),
        results,
        outages,
        rollups,
    }
}

async fn apply(
    fixture: &Fixture,
    batch: RemoteBatch,
) -> tocsin_store::Result<tocsin_store::BatchReport> {
    fixture.store.apply_batch(batch, LOCAL.into(), noon()).await
}

fn invalid(outcome: tocsin_store::Result<tocsin_store::BatchReport>) -> String {
    match outcome {
        Err(Error::Invalid(errors)) => errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; "),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[tokio::test]
async fn applying_a_batch_twice_changes_nothing() {
    let fixture = open();
    let base = day("2026-01-05").start_ms();
    let pushed = || {
        batch(
            vec![result(7, base + 1_000), result(9, base + 2_000)],
            vec![
                open_change(1, 3, base + 1_000),
                end_change(2, 3, base + 5_000),
                open_change(3, 4, base + 9_000),
            ],
            vec![rollup("2026-01-04", 90, 10)],
        )
    };
    let first = apply(&fixture, pushed()).await.unwrap();
    assert_eq!(first.results_applied, 2);
    assert_eq!(first.results_through, Some(9));
    assert_eq!(first.outage_changes_applied, 3);
    assert_eq!(first.outages_through, Some(3));
    assert_eq!(first.rollups_applied, 1);
    let once = dump(&fixture.path);

    let second = fixture
        .store
        .apply_batch(pushed(), LOCAL.into(), noon() + 60_000)
        .await
        .unwrap();
    assert_eq!(second.results_applied, 0);
    assert_eq!(second.results_through, Some(9));
    assert_eq!(second.outage_changes_applied, 0);
    assert_eq!(second.outages_through, Some(3));
    assert_eq!(dump(&fixture.path), once);
}

#[tokio::test]
async fn outage_changes_apply_in_seq_order_and_a_reordered_batch_is_refused() {
    let fixture = open();
    let base = day("2026-01-05").start_ms();
    let before = dump(&fixture.path);

    // Out of seq order within the batch.
    let error = invalid(
        apply(
            &fixture,
            batch(
                vec![result(1, base)],
                vec![
                    open_change(1, 3, base + 1_000),
                    open_change(3, 4, base + 9_000),
                    end_change(2, 3, base + 5_000),
                ],
                vec![],
            ),
        )
        .await,
    );
    assert!(error.contains("outages[2].seq"), "{error}");
    // In seq order, but the second outage opens before the first ends.
    let error = invalid(
        apply(
            &fixture,
            batch(
                vec![],
                vec![
                    open_change(1, 3, base + 1_000),
                    open_change(2, 4, base + 9_000),
                    end_change(3, 3, base + 5_000),
                ],
                vec![],
            ),
        )
        .await,
    );
    assert!(error.contains("still open"), "{error}");
    assert_eq!(dump(&fixture.path), before, "a refused batch wrote");

    // The previous outage's end comes before the next one's open.
    let report = apply(
        &fixture,
        batch(
            vec![],
            vec![
                open_change(1, 3, base + 1_000),
                end_change(2, 3, base + 5_000),
                open_change(3, 4, base + 9_000),
            ],
            vec![],
        ),
    )
    .await
    .unwrap();
    assert_eq!(report.outage_changes_applied, 3);
    let seqs: Vec<(i64, Option<i64>)> = raw(&fixture.path)
        .prepare("SELECT opened_seq, ended_seq FROM outage ORDER BY origin_id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(seqs, [(1, Some(2)), (3, None)]);

    // A later batch with a change numbered before ones already applied.
    let error = invalid(
        apply(
            &fixture,
            batch(vec![], vec![end_change(2, 4, base + 10_000)], vec![]),
        )
        .await,
    );
    assert!(error.contains("arrived after"), "{error}");
    // An end for an outage the store never opened.
    let error = invalid(
        apply(
            &fixture,
            batch(vec![], vec![end_change(4, 99, base + 10_000)], vec![]),
        )
        .await,
    );
    assert!(error.contains("never opened"), "{error}");
}

#[tokio::test]
async fn a_differing_end_is_a_conflict_and_writes_nothing() {
    let fixture = open();
    let base = day("2026-01-05").start_ms();
    apply(
        &fixture,
        batch(
            vec![],
            vec![open_change(1, 3, base), end_change(2, 3, base + 5_000)],
            vec![],
        ),
    )
    .await
    .unwrap();
    let before = dump(&fixture.path);
    let outcome = apply(
        &fixture,
        batch(
            vec![result(1, base + 6_000)],
            vec![end_change(2, 3, base + 7_000)],
            vec![rollup("2026-01-05", 1, 0)],
        ),
    )
    .await;
    assert!(matches!(outcome, Err(Error::Conflict(_))), "{outcome:?}");
    assert_eq!(dump(&fixture.path), before, "a conflicting batch wrote");

    let reopened = apply(
        &fixture,
        batch(
            vec![],
            vec![RemoteOutageChange::Open {
                seq: 1,
                origin_id: 3,
                outage: NewOutage {
                    health: Health::Degraded,
                    ..match open_change(1, 3, base) {
                        RemoteOutageChange::Open { outage, .. } => outage,
                        RemoteOutageChange::End { .. } => unreachable!(),
                    }
                },
            }],
            vec![],
        ),
    )
    .await;
    assert!(matches!(reopened, Err(Error::Conflict(_))), "{reopened:?}");
}

#[tokio::test]
async fn results_before_the_watermark_are_skipped_and_counted() {
    let fixture = open();
    let watermark = day("2026-01-04").start_ms();
    raw(&fixture.path)
        .execute(
            "INSERT INTO retention (id, purged_before) VALUES (1, ?1)",
            [watermark],
        )
        .unwrap();
    let report = apply(
        &fixture,
        batch(
            vec![
                result(1, watermark - 1),
                result(2, watermark),
                result(3, watermark - 1_000),
            ],
            vec![],
            // The origin's rollup for a day this store has finalized is
            // complete, so it lands.
            vec![rollup("2026-01-02", 100, 0)],
        ),
    )
    .await
    .unwrap();
    assert_eq!(report.results_applied, 1);
    assert_eq!(report.results_expired, 2);
    assert_eq!(report.results_through, Some(3));
    assert_eq!(report.rollups_applied, 1);
    assert_eq!(count(&fixture.path, "SELECT count(*) FROM check_result"), 1);
    assert_eq!(
        count(
            &fixture.path,
            "SELECT up FROM daily_rollup WHERE date = '2026-01-02'"
        ),
        100
    );
}

#[tokio::test]
async fn a_row_from_the_future_stops_its_kind_and_keeps_what_came_before() {
    let fixture = open();
    let base = day("2026-01-05").start_ms();
    let ahead = noon() + 2 * 3_600_000;
    let report = apply(
        &fixture,
        batch(
            vec![result(1, base), result(2, ahead), result(3, base + 1_000)],
            vec![open_change(1, 3, base), end_change(2, 3, ahead)],
            vec![rollup("2026-01-05", 5, 0), rollup("2026-01-07", 5, 0)],
        ),
    )
    .await
    .unwrap();
    assert_eq!(report.results_applied, 1);
    assert_eq!(report.results_through, Some(1));
    assert_eq!(report.outage_changes_applied, 1);
    assert_eq!(report.outages_through, Some(1));
    assert_eq!(report.rollups_applied, 1);
    assert_eq!(report.skewed_at_ms, Some(ahead));
    assert_eq!(
        count(
            &fixture.path,
            "SELECT count(*) FROM outage WHERE ended_at IS NULL"
        ),
        1
    );
}

#[tokio::test]
async fn a_batch_from_this_node_or_store_or_with_another_nodes_rows_is_refused() {
    let fixture = open();
    let base = day("2026-01-05").start_ms();
    let mut own = batch(vec![result(1, base)], vec![], vec![]);
    own.node_id = LOCAL.into();
    own.results[0].result.node_id = LOCAL.into();
    assert!(invalid(apply(&fixture, own).await).contains("node_id"));

    let mut own_store = batch(vec![result(1, base)], vec![], vec![]);
    own_store.store_id = fixture.store.store_id().await.unwrap();
    assert!(invalid(apply(&fixture, own_store).await).contains("store_id"));

    let mut relayed = batch(vec![result(1, base), result(2, base)], vec![], vec![]);
    relayed.results[1].result.node_id = "node-c".into();
    let error = invalid(apply(&fixture, relayed).await);
    assert!(error.contains("results[1].node_id"), "{error}");

    let unordered = batch(vec![result(2, base), result(2, base)], vec![], vec![]);
    let error = invalid(apply(&fixture, unordered).await);
    assert!(error.contains("results[1].origin_id"), "{error}");

    assert_eq!(count(&fixture.path, "SELECT count(*) FROM check_result"), 0);
    assert_eq!(
        count(
            &fixture.path,
            "SELECT count(*) FROM node_store WHERE local = 0"
        ),
        0
    );
}

#[tokio::test]
async fn the_local_recompute_leaves_other_nodes_rollups_alone() {
    let fixture = open();
    let base = day("2026-01-05").start_ms();
    fixture
        .store
        .append(NewCheckResult {
            node_id: LOCAL.into(),
            ..result(0, base + 500).result
        })
        .await
        .unwrap();
    // The origin's rollup counts results this store never received.
    apply(
        &fixture,
        batch(
            vec![result(1, base + 1_000), result(2, base + 2_000)],
            vec![],
            vec![rollup("2026-01-05", 40, 2)],
        ),
    )
    .await
    .unwrap();
    fixture.store.rollup_since(day("2026-01-05")).await.unwrap();
    let rollups: Vec<(String, i64, i64)> = raw(&fixture.path)
        .prepare("SELECT node_id, total, down FROM daily_rollup ORDER BY node_id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        rollups,
        [(REMOTE.to_owned(), 42, 2), (LOCAL.to_owned(), 1, 1)]
    );
}

#[tokio::test]
async fn local_outages_are_numbered_as_they_open_and_end() {
    let fixture = open();
    let outage = |check: &str| NewOutage {
        node_id: LOCAL.into(),
        subject_kind: SubjectKind::Check,
        check_id: check.into(),
        component_id: None,
        health: Health::Down,
        started_at_ms: 1_000,
        alerted: false,
    };
    let web = fixture.store.open_outage(outage("web")).await.unwrap();
    let api = fixture.store.open_outage(outage("api")).await.unwrap();
    fixture.store.end_outage(web, 2_000).await.unwrap();
    assert!(fixture.store.end_outage(web, 3_000).await.is_err());
    fixture.store.withdraw_outage(api, 4_000).await.unwrap();
    let seqs: Vec<(i64, Option<i64>)> = raw(&fixture.path)
        .prepare("SELECT opened_seq, ended_seq FROM outage ORDER BY id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    // The refused second end took no seq.
    assert_eq!(seqs, [(1, Some(3)), (2, Some(4))]);
    assert_eq!(
        count(
            &fixture.path,
            "SELECT next_seq FROM node_store WHERE local = 1"
        ),
        5
    );
}

#[tokio::test]
async fn a_rollup_from_the_future_leaves_the_others_and_skew_is_the_earliest_across_kinds() {
    let fixture = open();
    let base = day("2026-01-05").start_ms();
    let outage_ahead = noon() + 2 * 3_600_000;
    let result_ahead = noon() + 3 * 3_600_000;
    let mut other_check = rollup("2026-01-05", 7, 1);
    other_check.check_id = "api".into();
    let report = apply(
        &fixture,
        batch(
            vec![result(1, base), result(2, result_ahead)],
            vec![open_change(1, 3, base), end_change(2, 3, outage_ahead)],
            // Not ordered by day: the future one comes first.
            vec![rollup("2026-01-07", 5, 0), other_check],
        ),
    )
    .await
    .unwrap();
    assert_eq!(report.rollups_applied, 1);
    assert_eq!(report.skewed_at_ms, Some(outage_ahead));
    assert_eq!(
        count(
            &fixture.path,
            "SELECT count(*) FROM daily_rollup WHERE check_id = 'api'"
        ),
        1
    );
}

#[tokio::test]
async fn a_value_the_schema_refuses_is_named_rather_than_failing_the_write() {
    let fixture = open();
    let base = day("2026-01-05").start_ms();
    let mut bad = batch(
        vec![result(1, base), result(2, base)],
        vec![open_change(1, 3, base)],
        vec![rollup("2026-01-05", 5, 1)],
    );
    bad.store_id = "0123456789ABCDEF0123456789ABCDEF".into();
    bad.results[0].result.check_id = String::new();
    bad.results[0].result.duration_ms = -1;
    bad.results[1].result.status_code = Some(1_000);
    if let RemoteOutageChange::Open { outage, .. } = &mut bad.outages[0] {
        outage.health = Health::Up;
    }
    bad.rollups[0].total = 7;
    let error = invalid(apply(&fixture, bad).await);
    for field in [
        "store_id",
        "results[0].check_id",
        "results[0].duration_ms",
        "results[1].status_code",
        "outages[0].health",
        "rollups[0].total",
    ] {
        assert!(error.contains(field), "{field} missing from {error}");
    }

    apply(
        &fixture,
        batch(vec![], vec![open_change(1, 3, base)], vec![]),
    )
    .await
    .unwrap();
    let early = batch(vec![], vec![end_change(2, 3, base - 1)], vec![]);
    let error = invalid(apply(&fixture, early).await);
    assert!(error.contains("outages[0].ended_at"), "{error}");
    assert_eq!(
        count(
            &fixture.path,
            "SELECT count(*) FROM outage WHERE ended_at IS NULL"
        ),
        1
    );
}

#[tokio::test]
async fn the_hourly_rollup_resumes_from_this_nodes_own_days() {
    let fixture = open();
    fixture
        .store
        .append(NewCheckResult {
            node_id: LOCAL.into(),
            ..result(0, day("2026-01-02").start_ms()).result
        })
        .await
        .unwrap();
    apply(
        &fixture,
        batch(
            vec![result(1, day("2026-01-05").start_ms())],
            vec![],
            vec![rollup("2026-01-05", 5, 0)],
        ),
    )
    .await
    .unwrap();
    let report = fixture.store.rollup_recent().await.unwrap();
    assert_eq!(report.since, Some(day("2026-01-02")));
}
