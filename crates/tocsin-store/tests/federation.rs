// SPDX-License-Identifier: AGPL-3.0-only

//! Applying batches other nodes push (docs/design/federation.md).

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::Connection;
use rusqlite::types::Value;
use std::collections::BTreeMap;
use tocsin_core::Health;
use tocsin_core::status::OutageEnding;

use tocsin_store::{
    AlertRouting, AlertTarget, DailyRollup, Day, EndedBy, Error, NewCheckResult, NewOutage,
    RemoteAlerting, RemoteBatch, RemoteOutageChange, RemoteResult, Store, SubjectKind,
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
    fixture
        .store
        .apply_batch(batch, LOCAL.into(), BTreeMap::default(), noon())
        .await
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
        .apply_batch(pushed(), LOCAL.into(), BTreeMap::default(), noon() + 60_000)
        .await
        .unwrap();
    assert_eq!(second.results_applied, 0);
    assert_eq!(second.results_through, Some(9));
    assert_eq!(second.outage_changes_applied, 0);
    assert_eq!(second.outages_through, Some(3));

    // The second push is heard, and nothing else changes.
    let stores = fixture.store.remote_stores().await.unwrap();
    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0].last_push_at_ms, Some(noon() + 60_000));
    raw(&fixture.path)
        .execute(
            "UPDATE node_store SET last_push_at = ?1 WHERE local = 0",
            [noon()],
        )
        .unwrap();
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
    assert_eq!(
        states(&fixture.path),
        [(STORE.to_owned(), "conflicted".to_owned())]
    );
    // Marking the node conflicted is the one thing it wrote.
    raw(&fixture.path)
        .execute("UPDATE node_store SET state = 'active'", [])
        .unwrap();
    assert_eq!(dump(&fixture.path), before, "a conflicting batch wrote");

    let fixture = open();
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
    assert_eq!(
        states(&fixture.path),
        [(STORE.to_owned(), "conflicted".to_owned())]
    );
}

/// Each store of the node, as (store id, state), oldest first.
fn states(path: &Path) -> Vec<(String, String)> {
    let conn = raw(path);
    let mut statement = conn
        .prepare("SELECT store_id, state FROM node_store WHERE local = 0 ORDER BY id")
        .unwrap();
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
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
    assert!(invalid(apply(&fixture, own).await).contains("node: is this node's own"));

    let mut own_store = batch(vec![result(1, base)], vec![], vec![]);
    own_store.store_id = fixture.store.store_id().await.unwrap();
    assert!(invalid(apply(&fixture, own_store).await).contains("store: is this database's own"));

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
        "store: is not",
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

/// How this node alerts for the outage pushed as `origin_id`: to Slack,
/// opening alerts when `opens`.
fn alerting(
    origin_id: i64,
    opens: bool,
    arrived_late_at_ms: Option<i64>,
) -> BTreeMap<i64, RemoteAlerting> {
    let routing = AlertRouting {
        targets: vec![AlertTarget {
            notifier_id: "slack".into(),
            route_id: "ops".into(),
            repeat_ms: None,
        }],
        ..AlertRouting::default()
    };
    BTreeMap::from([(
        origin_id,
        RemoteAlerting {
            routing,
            opens,
            arrived_late_at_ms,
        },
    )])
}

async fn apply_alerting(
    fixture: &Fixture,
    outages: Vec<RemoteOutageChange>,
    alerting: BTreeMap<i64, RemoteAlerting>,
    now_ms: i64,
) -> tocsin_store::BatchReport {
    fixture
        .store
        .apply_batch(
            batch(vec![], outages, vec![]),
            LOCAL.into(),
            alerting,
            now_ms,
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn a_pushed_outage_fires_and_its_end_resolves_the_same_alert() {
    let fixture = open();
    let started = noon() - 120_000;
    let report = apply_alerting(
        &fixture,
        vec![open_change(1, 1, started)],
        alerting(1, true, None),
        noon(),
    )
    .await;
    assert_eq!(report.alerts_queued, 1);
    let fired = fixture.store.alerts().await.unwrap();
    assert_eq!(fired.len(), 1);
    assert_eq!(
        (fired[0].node_id.as_str(), fired[0].subject_id.as_str()),
        (REMOTE, "web")
    );
    assert_eq!(fired[0].notifier_id, "slack");
    assert_eq!(fired[0].started_at_ms, started);
    assert_eq!(fired[0].ended_at_ms, None);
    assert_eq!(fired[0].arrived_late_at_ms, None);

    let ended = noon() + 30_000;
    let report = apply_alerting(
        &fixture,
        vec![end_change(2, 1, ended)],
        BTreeMap::new(),
        noon() + 60_000,
    )
    .await;
    assert_eq!(report.alerts_queued, 1);
    let resolved = fixture.store.alerts().await.unwrap();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].id, fired[0].id);
    assert_eq!(resolved[0].ended_at_ms, Some(ended));
}

#[tokio::test]
async fn an_outage_received_already_ended_alerts_no_one() {
    let fixture = open();
    let report = apply_alerting(
        &fixture,
        vec![
            open_change(1, 1, noon() - 120_000),
            end_change(2, 1, noon() - 60_000),
        ],
        alerting(1, true, None),
        noon(),
    )
    .await;
    assert_eq!(report.outage_changes_applied, 2);
    assert_eq!(report.alerts_queued, 0);
    assert_eq!(fixture.store.alerts().await.unwrap(), []);
}

#[tokio::test]
async fn behind_a_node_that_is_up_a_pushed_outage_opens_no_alert() {
    let fixture = open();
    let report = apply_alerting(
        &fixture,
        vec![open_change(1, 1, noon() - 120_000)],
        alerting(1, false, None),
        noon(),
    )
    .await;
    assert_eq!(report.alerts_queued, 0);
    assert_eq!(fixture.store.alerts().await.unwrap(), []);
}

#[tokio::test]
async fn an_outage_that_arrived_late_says_when() {
    let fixture = open();
    apply_alerting(
        &fixture,
        vec![open_change(1, 1, noon() - 3_600_000)],
        alerting(1, true, Some(noon())),
        noon(),
    )
    .await;
    let fired = fixture.store.alerts().await.unwrap();
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].arrived_late_at_ms, Some(noon()));
    assert_eq!(fired[0].outage_started_at_ms, Some(noon() - 3_600_000));
}

/// The store a rebuilt node pushes from: its database is new.
const REBUILT: &str = "fedcba9876543210fedcba9876543210";

fn from(store_id: &str, outages: Vec<RemoteOutageChange>) -> RemoteBatch {
    RemoteBatch {
        store_id: store_id.into(),
        ..batch(vec![], outages, vec![])
    }
}

async fn apply_from(
    fixture: &Fixture,
    batch: RemoteBatch,
    alerting: BTreeMap<i64, RemoteAlerting>,
    now_ms: i64,
) -> tocsin_store::Result<tocsin_store::BatchReport> {
    fixture
        .store
        .apply_batch(batch, LOCAL.into(), alerting, now_ms)
        .await
}

/// One outage, as (origin store id, origin id, ending, ended at, opened
/// seq, ended seq).
type OutageRow = (String, i64, Option<String>, Option<i64>, i64, Option<i64>);

/// The node's outages, oldest first.
fn outages(path: &Path) -> Vec<OutageRow> {
    let conn = raw(path);
    let mut statement = conn
        .prepare(
            "SELECT s.store_id, o.origin_id, o.ending, o.ended_at, o.opened_seq, o.ended_seq
             FROM outage o JOIN node_store s ON s.id = o.origin_store
             WHERE o.node_id = ?1 ORDER BY o.id",
        )
        .unwrap();
    statement
        .query_map([REMOTE], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn superseded_at(path: &Path, store_id: &str) -> Option<i64> {
    raw(path)
        .query_row(
            "SELECT superseded_at FROM node_store WHERE store_id = ?1",
            [store_id],
            |row| row.get(0),
        )
        .unwrap()
}

/// A rebuilt node pushes from a new store with an outage of the same check
/// open: the old store is retired, its outage withdrawn with the seq after
/// its own open (here past anything this node numbered), its alert ended
/// by config, and the new store's outage opens and fires.
#[tokio::test]
async fn a_new_store_for_a_known_node_replaces_the_old_and_withdraws_its_outages() {
    let fixture = open();
    let started = noon() - 120_000;
    apply_from(
        &fixture,
        from(STORE, vec![open_change(1_000, 1, started)]),
        alerting(1, true, None),
        noon(),
    )
    .await
    .unwrap();
    let fired = fixture.store.alerts().await.unwrap();
    assert_eq!(fired.len(), 1);

    let replaced_at = noon() + 60_000;
    let report = apply_from(
        &fixture,
        from(REBUILT, vec![open_change(1, 1, noon() + 30_000)]),
        alerting(1, true, None),
        replaced_at,
    )
    .await
    .unwrap();
    assert_eq!(report.replaced, [STORE]);
    assert_eq!(report.outage_changes_applied, 1);
    assert_eq!(
        report.alerts_queued, 2,
        "the old alert's resolve and the new one's fire"
    );

    assert_eq!(
        outages(&fixture.path),
        [
            (
                STORE.to_owned(),
                1,
                Some("withdrawn".to_owned()),
                Some(replaced_at),
                1_000,
                Some(1_001)
            ),
            (REBUILT.to_owned(), 1, None, None, 1, None),
        ]
    );
    let alerts = fixture.store.alerts().await.unwrap();
    let old = alerts.iter().find(|alert| alert.id == fired[0].id).unwrap();
    assert_eq!(old.ended_at_ms, Some(replaced_at));
    assert_eq!(old.ended_by, Some(EndedBy::Config));
    assert!(
        alerts
            .iter()
            .any(|alert| alert.id != old.id && alert.ended_at_ms.is_none()),
        "{alerts:?}"
    );
    assert_eq!(
        states(&fixture.path),
        [
            (STORE.to_owned(), "retired".to_owned()),
            (REBUILT.to_owned(), "active".to_owned()),
        ]
    );
    assert_eq!(superseded_at(&fixture.path, STORE), Some(replaced_at));
    assert_eq!(superseded_at(&fixture.path, REBUILT), None);

    // Accepted once: its next push replaces nothing.
    let again = apply_from(
        &fixture,
        from(REBUILT, vec![]),
        BTreeMap::new(),
        replaced_at,
    )
    .await
    .unwrap();
    assert_eq!(again.replaced, Vec::<String>::new());
}

/// The replaced store pushing again means two machines are pushing as one
/// node: both stores are marked conflicted and refused, the marking
/// survives a restart, and only retiring the node clears it, to retired.
#[tokio::test]
async fn a_replaced_store_pushing_again_conflicts_the_node_across_a_restart() {
    let fixture = open();
    let started = noon() - 120_000;
    for store in [STORE, REBUILT] {
        apply_from(
            &fixture,
            from(store, vec![open_change(1, 1, started)]),
            BTreeMap::new(),
            noon(),
        )
        .await
        .unwrap();
    }
    let before = outages(&fixture.path);

    let outcome = apply_from(&fixture, from(STORE, vec![]), BTreeMap::new(), noon()).await;
    assert!(matches!(outcome, Err(Error::Conflict(_))), "{outcome:?}");
    let conflicted = [
        (STORE.to_owned(), "conflicted".to_owned()),
        (REBUILT.to_owned(), "conflicted".to_owned()),
    ];
    assert_eq!(states(&fixture.path), conflicted);
    assert_eq!(
        outages(&fixture.path),
        before,
        "a refused push changed an outage"
    );

    let Fixture {
        _dir: dir,
        path,
        store,
    } = fixture;
    drop(store);
    let fixture = Fixture {
        store: Store::open(&path).unwrap(),
        _dir: dir,
        path,
    };
    for store in [STORE, REBUILT] {
        let outcome = apply_from(&fixture, from(store, vec![]), BTreeMap::new(), noon()).await;
        assert!(
            matches!(outcome, Err(Error::Conflict(_))),
            "{store}: {outcome:?}"
        );
    }
    assert_eq!(states(&fixture.path), conflicted);

    fixture
        .store
        .retire_nodes(vec![REMOTE.into()], noon() + 60_000)
        .await
        .unwrap();
    assert_eq!(
        states(&fixture.path),
        [
            (STORE.to_owned(), "retired".to_owned()),
            (REBUILT.to_owned(), "retired".to_owned()),
        ]
    );
    for store in [STORE, REBUILT] {
        let outcome = apply_from(&fixture, from(store, vec![]), BTreeMap::new(), noon()).await;
        assert!(
            matches!(outcome, Err(Error::Retired(_))),
            "{store}: {outcome:?}"
        );
    }
    assert_eq!(
        states(&fixture.path)[0].1,
        "retired",
        "a push to a retired node conflicted it again"
    );
}

/// Retiring a node in the configuration withdraws what it has open and
/// ends its alerts by config; running it again changes nothing.
#[tokio::test]
async fn retiring_a_node_withdraws_its_open_outages_and_refuses_its_store() {
    let fixture = open();
    apply_from(
        &fixture,
        from(STORE, vec![open_change(1, 1, noon() - 120_000)]),
        alerting(1, true, None),
        noon(),
    )
    .await
    .unwrap();

    let retired_at = noon() + 60_000;
    let queued = fixture
        .store
        .retire_nodes(vec![REMOTE.into(), "never-pushed".into()], retired_at)
        .await
        .unwrap();
    assert_eq!(queued, 1);
    assert_eq!(
        outages(&fixture.path),
        [(
            STORE.to_owned(),
            1,
            Some("withdrawn".to_owned()),
            Some(retired_at),
            1,
            Some(2)
        )]
    );
    let alerts = fixture.store.alerts().await.unwrap();
    assert_eq!(alerts[0].ended_at_ms, Some(retired_at));
    assert_eq!(alerts[0].ended_by, Some(EndedBy::Config));
    assert_eq!(
        states(&fixture.path),
        [(STORE.to_owned(), "retired".to_owned())]
    );
    assert_eq!(superseded_at(&fixture.path, STORE), None);

    let outcome = apply_from(&fixture, from(STORE, vec![]), BTreeMap::new(), retired_at).await;
    assert!(matches!(outcome, Err(Error::Retired(_))), "{outcome:?}");
    let again = fixture
        .store
        .retire_nodes(vec![REMOTE.into()], retired_at + 1)
        .await
        .unwrap();
    assert_eq!(again, 0);
}
