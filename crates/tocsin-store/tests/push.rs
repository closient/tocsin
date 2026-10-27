// SPDX-License-Identifier: AGPL-3.0-only

//! Reading this node's rows for its upstreams (docs/design/federation.md).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, params};
use tocsin_core::Health;
use tocsin_core::federation::wire::{BatchOutageChange, Positions};
use tocsin_core::status::OutageEnding;
use tocsin_store::{
    Day, NewCheckResult, NewOutage, PushCursor, RemoteBatch, RemoteOutageChange, RemoteResult,
    Retention, Store, SubjectKind,
};

const LOCAL: &str = "node-local";
const REMOTE: &str = "node-b";
const STORE: &str = "0123456789abcdef0123456789abcdef";
const HUB: &str = "hub";

fn day(text: &str) -> Day {
    text.parse().unwrap()
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

fn result(node: &str, check: &str, started_at_ms: i64) -> NewCheckResult {
    NewCheckResult {
        node_id: node.into(),
        check_id: check.into(),
        started_at_ms,
        duration_ms: 12,
        health: Health::Up,
        detail: Some(serde_json::json!({ "status": { "got": 200 } })),
        error: None,
        status_code: Some(200),
    }
}

fn outage(node: &str, check: &str, started_at_ms: i64) -> NewOutage {
    NewOutage {
        node_id: node.into(),
        subject_kind: SubjectKind::Check,
        check_id: check.into(),
        component_id: None,
        health: Health::Down,
        started_at_ms,
        alerted: false,
    }
}

/// A batch from `REMOTE` with one result and one outage: rows this store
/// holds that are not its own to push.
async fn receive_remote_rows(fixture: &Fixture) {
    fixture
        .store
        .apply_batch(
            RemoteBatch {
                node_id: REMOTE.into(),
                store_id: STORE.into(),
                results: vec![RemoteResult {
                    origin_id: 1,
                    kind: SubjectKind::Check,
                    result: result(REMOTE, "web", 1_500),
                }],
                outages: vec![RemoteOutageChange::Open {
                    seq: 1,
                    origin_id: 1,
                    outage: outage(REMOTE, "web", 1_500),
                }],
                rollups: vec![],
            },
            LOCAL.into(),
            2_000,
        )
        .await
        .unwrap();
}

async fn cursor(fixture: &Fixture) -> PushCursor {
    fixture
        .store
        .push_cursor(HUB.into(), LOCAL.into())
        .await
        .unwrap()
}

fn result_ids(unpushed: &tocsin_store::Unpushed) -> Vec<i64> {
    unpushed.results.iter().map(|row| row.origin_id).collect()
}

/// Each outage change as (seq, open or end, local id).
fn changes(unpushed: &tocsin_store::Unpushed) -> Vec<(i64, &'static str, i64)> {
    unpushed
        .outages
        .iter()
        .map(|change| match change {
            BatchOutageChange::Open { seq, origin_id, .. } => (*seq, "open", *origin_id),
            BatchOutageChange::End { seq, origin_id, .. } => (*seq, "end", *origin_id),
        })
        .collect()
}

#[tokio::test]
async fn only_this_nodes_own_rows_past_the_cursor_are_read_in_order() {
    let fixture = open();
    let first = fixture
        .store
        .append(result(LOCAL, "web", 1_000))
        .await
        .unwrap();
    receive_remote_rows(&fixture).await;
    let web = fixture
        .store
        .open_outage(outage(LOCAL, "web", 1_000))
        .await
        .unwrap();
    let api = fixture
        .store
        .open_outage(outage(LOCAL, "api", 1_100))
        .await
        .unwrap();
    fixture.store.end_outage(web, 3_000).await.unwrap();
    let second = fixture
        .store
        .append(result(LOCAL, "api", 3_000))
        .await
        .unwrap();

    let start = cursor(&fixture).await;
    assert_eq!(start.positions(), Positions::default());
    let unpushed = fixture
        .store
        .unpushed(LOCAL.into(), start.clone(), 100)
        .await
        .unwrap();
    assert_eq!(result_ids(&unpushed), [first, second]);
    assert!(unpushed.results.iter().all(|row| row.node_id == LOCAL));
    assert_eq!(
        unpushed.results[0].detail,
        Some(serde_json::json!({ "status": { "got": 200 } }))
    );
    // The remote outage took no seq of this store's.
    assert_eq!(
        changes(&unpushed),
        [(1, "open", web), (2, "open", api), (3, "end", web)]
    );
    let BatchOutageChange::End {
        ending, ended_at, ..
    } = &unpushed.outages[2]
    else {
        unreachable!()
    };
    assert_eq!(*ending, OutageEnding::Recovered);
    assert_eq!(ended_at.as_millis(), 3_000);
    assert_eq!(
        unpushed.head,
        Positions {
            results: Some(second),
            outages: Some(3),
        }
    );

    // Past the first of each, only the rest.
    let moved = PushCursor {
        results_through: first,
        outages_through: 1,
        ..start
    };
    fixture
        .store
        .advance_push_cursor(HUB.into(), moved.clone())
        .await
        .unwrap();
    assert_eq!(cursor(&fixture).await, moved);
    let rest = fixture
        .store
        .unpushed(LOCAL.into(), moved, 100)
        .await
        .unwrap();
    assert_eq!(result_ids(&rest), [second]);
    assert_eq!(changes(&rest), [(2, "open", api), (3, "end", web)]);
}

#[tokio::test]
async fn outage_changes_take_the_batch_before_results() {
    let fixture = open();
    for at in [1_000, 2_000, 3_000] {
        fixture
            .store
            .append(result(LOCAL, "web", at))
            .await
            .unwrap();
    }
    let web = fixture
        .store
        .open_outage(outage(LOCAL, "web", 1_000))
        .await
        .unwrap();
    fixture.store.end_outage(web, 2_500).await.unwrap();
    let start = cursor(&fixture).await;

    let two = fixture
        .store
        .unpushed(LOCAL.into(), start.clone(), 2)
        .await
        .unwrap();
    assert_eq!(changes(&two), [(1, "open", web), (2, "end", web)]);
    assert_eq!(two.results.len(), 0);

    let three = fixture
        .store
        .unpushed(LOCAL.into(), start, 3)
        .await
        .unwrap();
    assert_eq!(three.outages.len(), 2);
    assert_eq!(three.results.len(), 1);
}

fn put_rollup(path: &Path, node: &str, check: &str, date: &str) {
    raw(path)
        .execute(
            "INSERT INTO daily_rollup (check_id, node_id, date, total, up, degraded, down, worst, kind)
             VALUES (?1, ?2, ?3, 10, 10, 0, 0, 'up', 'check')",
            params![check, node, date],
        )
        .unwrap();
}

fn rollup_days(unpushed: &tocsin_store::Unpushed) -> Vec<(String, String)> {
    unpushed
        .rollups
        .iter()
        .map(|row| (row.date.clone(), row.check_id.clone()))
        .collect()
}

#[tokio::test]
async fn rollups_go_in_whole_days_from_the_cursor() {
    let fixture = open();
    for date in ["2026-01-01", "2026-01-02", "2026-01-03"] {
        put_rollup(&fixture.path, LOCAL, "web", date);
        put_rollup(&fixture.path, LOCAL, "api", date);
        put_rollup(&fixture.path, REMOTE, "web", date);
    }
    // A new upstream gets every day this node has.
    let start = cursor(&fixture).await;
    assert_eq!(start.rollups_from, Some(day("2026-01-01")));

    let five = fixture
        .store
        .unpushed(LOCAL.into(), start.clone(), 5)
        .await
        .unwrap();
    assert_eq!(
        rollup_days(&five),
        [
            ("2026-01-01".into(), "api".into()),
            ("2026-01-01".into(), "web".into()),
            ("2026-01-02".into(), "api".into()),
            ("2026-01-02".into(), "web".into()),
        ]
    );
    assert!(five.rollups.iter().all(|row| row.node_id == LOCAL));
    assert_eq!(five.rollups_next, Some(day("2026-01-03")));

    // A day bigger than the batch still goes, alone, when nothing else does.
    let one = fixture
        .store
        .unpushed(LOCAL.into(), start.clone(), 1)
        .await
        .unwrap();
    assert_eq!(one.rollups.len(), 2);
    assert_eq!(one.rollups_next, Some(day("2026-01-02")));

    // But not when results fill the batch.
    fixture
        .store
        .append(result(LOCAL, "web", 1_000))
        .await
        .unwrap();
    let busy = fixture
        .store
        .unpushed(LOCAL.into(), start.clone(), 1)
        .await
        .unwrap();
    assert_eq!(busy.results.len(), 1);
    assert_eq!(busy.rollups.len(), 0);
    assert_eq!(busy.rollups_next, Some(day("2026-01-01")));

    // A trickle of results that leaves room does not hold a day back for
    // ever: the day goes whole beside it.
    let trickle = fixture
        .store
        .unpushed(LOCAL.into(), start.clone(), 2)
        .await
        .unwrap();
    assert_eq!(trickle.results.len(), 1);
    assert_eq!(trickle.rollups.len(), 2);
    assert_eq!(trickle.rollups_next, Some(day("2026-01-02")));

    let all = fixture
        .store
        .unpushed(LOCAL.into(), start, 100)
        .await
        .unwrap();
    assert_eq!(all.rollups.len(), 6);
    assert_eq!(all.rollups_next, None);

    // Nothing from a cursor with no rollups pending.
    let none = fixture
        .store
        .unpushed(
            LOCAL.into(),
            PushCursor {
                rollups_from: None,
                ..cursor(&fixture).await
            },
            100,
        )
        .await
        .unwrap();
    assert_eq!(none.rollups.len(), 0);
}

#[tokio::test]
async fn a_cursor_is_made_once_and_kept() {
    let fixture = open();
    let made = cursor(&fixture).await;
    assert_eq!(
        made,
        PushCursor {
            results_through: 0,
            outages_through: 0,
            rollups_from: None,
            acked_at_ms: None,
            rollups_rewound: 0,
        }
    );
    let moved = PushCursor {
        results_through: 7,
        outages_through: 3,
        rollups_from: Some(day("2026-01-02")),
        acked_at_ms: Some(9_000),
        rollups_rewound: 0,
    };
    fixture
        .store
        .advance_push_cursor(HUB.into(), moved.clone())
        .await
        .unwrap();
    put_rollup(&fixture.path, LOCAL, "web", "2026-01-01");
    assert_eq!(cursor(&fixture).await, moved);
}

#[tokio::test]
async fn retention_counts_the_rows_each_upstream_never_received() {
    let fixture = open();
    let now = day("2026-02-01").start_ms();
    let old = day("2026-01-01").start_ms();
    let mut ids = Vec::new();
    for offset in 0..4 {
        ids.push(
            fixture
                .store
                .append(result(LOCAL, "web", old + offset))
                .await
                .unwrap(),
        );
    }
    receive_remote_rows(&fixture).await;
    // "hub" has the first; "other" has none; a third has them all.
    for (upstream, through) in [("hub", ids[0]), ("other", 0), ("caught-up", ids[3])] {
        let start = fixture
            .store
            .push_cursor(upstream.into(), LOCAL.into())
            .await
            .unwrap();
        fixture
            .store
            .advance_push_cursor(
                upstream.into(),
                PushCursor {
                    results_through: through,
                    ..start
                },
            )
            .await
            .unwrap();
    }
    let policy = Retention {
        chunk_rows: 2,
        pause: Duration::ZERO,
        ..Retention::days(3)
    };
    let report = fixture.store.apply_retention(policy, now).await.unwrap();
    // The remote result is deleted too, but it was never this store's.
    assert_eq!(report.deleted, 5);
    assert_eq!(
        report.unpushed,
        BTreeMap::from([("hub".to_owned(), 3), ("other".to_owned(), 4)])
    );
}

/// `upstream`'s cursor moved to `rollups_from`, as read before the move.
async fn set_rollups_from(fixture: &Fixture, upstream: &str, from: Option<Day>) -> PushCursor {
    let start = fixture
        .store
        .push_cursor(upstream.into(), LOCAL.into())
        .await
        .unwrap();
    fixture
        .store
        .advance_push_cursor(
            upstream.into(),
            PushCursor {
                rollups_from: from,
                ..start.clone()
            },
        )
        .await
        .unwrap();
    PushCursor {
        rollups_from: from,
        ..start
    }
}

#[tokio::test]
async fn rewriting_a_day_moves_every_later_cursor_back_to_it() {
    let fixture = open();
    for (upstream, from) in [
        ("unset", None),
        ("later", Some(day("2026-01-03"))),
        ("earlier", Some(day("2026-01-01"))),
    ] {
        set_rollups_from(&fixture, upstream, from).await;
    }
    fixture
        .store
        .append(result(LOCAL, "web", day("2026-01-02").start_ms() + 5))
        .await
        .unwrap();
    // A rollup recompute, the CLI's included, rewinds in the transaction
    // that writes the day; no caller has to.
    let report = fixture.store.rollup_since(day("2026-01-02")).await.unwrap();
    assert!(report.rows > 0);
    for (upstream, from) in [
        ("unset", "2026-01-02"),
        ("later", "2026-01-02"),
        ("earlier", "2026-01-01"),
    ] {
        assert_eq!(
            fixture
                .store
                .push_cursor(upstream.into(), LOCAL.into())
                .await
                .unwrap()
                .rollups_from,
            Some(day(from)),
            "{upstream}"
        );
    }
}

#[tokio::test]
async fn a_push_read_before_a_rewrite_does_not_move_past_it() {
    let fixture = open();
    let first = day("2026-01-02");
    fixture
        .store
        .append(result(LOCAL, "web", first.start_ms() + 5))
        .await
        .unwrap();
    // "same-day" is already at the day the rewrite is of: a rewind that
    // moves nothing still counts.
    let unset = set_rollups_from(&fixture, "unset", None).await;
    let same_day = set_rollups_from(&fixture, "same-day", Some(first)).await;
    let fresh = set_rollups_from(&fixture, "fresh", None).await;
    fixture.store.rollup_since(first).await.unwrap();
    let fresh = PushCursor {
        rollups_rewound: cursor_of(&fixture, "fresh").await.rollups_rewound,
        ..fresh
    };

    // Each push read its cursor before the rewrite and sent everything.
    for (upstream, read) in [("unset", unset), ("same-day", same_day)] {
        let pushed = PushCursor {
            results_through: 1,
            rollups_from: None,
            ..read
        };
        fixture
            .store
            .advance_push_cursor(upstream.into(), pushed)
            .await
            .unwrap();
        let stored = cursor_of(&fixture, upstream).await;
        assert_eq!(stored.rollups_from, Some(first), "{upstream}");
        assert_eq!(stored.results_through, 1, "{upstream}");
    }
    // One that read it after moves on.
    fixture
        .store
        .advance_push_cursor(
            "fresh".into(),
            PushCursor {
                rollups_from: None,
                ..fresh
            },
        )
        .await
        .unwrap();
    assert_eq!(cursor_of(&fixture, "fresh").await.rollups_from, None);
}

async fn cursor_of(fixture: &Fixture, upstream: &str) -> PushCursor {
    fixture
        .store
        .push_cursor(upstream.into(), LOCAL.into())
        .await
        .unwrap()
}
