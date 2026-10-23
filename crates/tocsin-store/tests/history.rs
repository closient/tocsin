// SPDX-License-Identifier: AGPL-3.0-only

//! Reading history back a page at a time: a check's raw results and the
//! outages overlapping a window.

use tocsin_core::Health::{self, Degraded, Down, Up};
use tocsin_core::incident::Timestamp;
use tocsin_core::status::CheckKind;
use tocsin_store::{HistoryPage, NewCheckResult, NewOutage, Store, SubjectKind};

fn result(check: &str, started_at_ms: i64, health: Health) -> NewCheckResult {
    NewCheckResult {
        node_id: "node-a".into(),
        check_id: check.into(),
        started_at_ms,
        duration_ms: 12,
        health,
        detail: Some(serde_json::json!({ "status": { "expected": 200, "got": 503 } })),
        error: (health != Up).then(|| "status 503".to_owned()),
        status_code: Some(503),
    }
}

fn outage(check: &str, started_at_ms: i64, health: Health) -> NewOutage {
    NewOutage {
        node_id: "node-a".into(),
        subject_kind: if check == "c" {
            SubjectKind::Heartbeat
        } else {
            SubjectKind::Check
        },
        check_id: check.into(),
        component_id: Some("api".into()),
        health,
        started_at_ms,
        alerted: false,
    }
}

fn page(since_ms: i64, before: Option<i64>, limit: u32) -> HistoryPage {
    HistoryPage {
        since: Timestamp::from_millis(since_ms),
        before,
        limit,
    }
}

fn open() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join(tocsin_store::DB_FILE)).unwrap();
    (dir, store)
}

#[tokio::test]
async fn results_page_newest_first_inside_the_window_ties_by_id() {
    let (_dir, store) = open();
    let early = store.append(result("api", 500, Up)).await.unwrap();
    let first = store.append(result("api", 1_000, Up)).await.unwrap();
    let tie_a = store.append(result("api", 2_000, Degraded)).await.unwrap();
    let tie_b = store.append(result("api", 2_000, Down)).await.unwrap();
    store.append(result("web", 2_500, Down)).await.unwrap();
    let last = store.append(result("api", 3_000, Down)).await.unwrap();

    let ids = |rows: &[tocsin_core::status::CheckResult]| -> Vec<i64> {
        rows.iter().map(|row| row.id).collect()
    };
    let one = store
        .check_results("api", page(1_000, None, 2))
        .await
        .unwrap();
    assert_eq!(ids(&one), [last, tie_b]);
    let two = store
        .check_results("api", page(1_000, Some(tie_b), 2))
        .await
        .unwrap();
    assert_eq!(
        ids(&two),
        [tie_a, first],
        "ties page by id, `since` is inclusive"
    );
    let three = store
        .check_results("api", page(1_000, Some(first), 2))
        .await
        .unwrap();
    assert!(three.is_empty(), "{early} is before the window: {three:?}");

    let row = &one[0];
    assert_eq!(
        (
            row.started_at,
            row.health,
            row.error.as_deref(),
            row.status_code
        ),
        (
            Timestamp::from_millis(3_000),
            Down,
            Some("status 503"),
            Some(503)
        )
    );
    assert_eq!(
        row.detail,
        Some(serde_json::json!({ "status": { "expected": 200, "got": 503 } }))
    );
    let none = store
        .check_results("nope", page(0, None, 10))
        .await
        .unwrap();
    assert!(none.is_empty(), "another check's results: {none:?}");
}

#[tokio::test]
async fn outages_overlapping_the_window_page_newest_first() {
    let (_dir, store) = open();
    let gone = store.open_outage(outage("a", 100, Down)).await.unwrap();
    store.end_outage(gone, 900).await.unwrap();
    let straddles = store.open_outage(outage("b", 500, Down)).await.unwrap();
    store.end_outage(straddles, 1_500).await.unwrap();
    let inside = store
        .open_outage(outage("c", 1_200, Degraded))
        .await
        .unwrap();
    store.end_outage(inside, 1_300).await.unwrap();
    let open = store.open_outage(outage("d", 2_000, Down)).await.unwrap();

    let ids = |rows: &[tocsin_core::status::OutageRecord]| -> Vec<i64> {
        rows.iter().map(|row| row.id).collect()
    };
    let all = store.outages(page(1_000, None, 10)).await.unwrap();
    assert_eq!(
        ids(&all),
        [open, inside, straddles],
        "one that ended before `since` is out"
    );
    assert_eq!(all[0].ended_at, None);
    assert_eq!(all[2].ended_at, Some(Timestamp::from_millis(1_500)));
    assert_eq!(
        (
            all[1].check_id.as_str(),
            all[1].check_kind,
            all[1].health,
            all[1].component_id.as_deref()
        ),
        ("c", CheckKind::Heartbeat, Degraded, Some("api"))
    );
    assert_eq!(all[0].check_kind, CheckKind::Check);

    let one = store.outages(page(1_000, None, 2)).await.unwrap();
    assert_eq!(ids(&one), [open, inside]);
    let two = store.outages(page(1_000, Some(inside), 2)).await.unwrap();
    assert_eq!(ids(&two), [straddles]);
}
