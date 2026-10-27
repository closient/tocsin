// SPDX-License-Identifier: AGPL-3.0-only

//! Reading history back a page at a time: a check's raw results and the
//! outages overlapping a window.

use tocsin_core::Health::{self, Degraded, Down, Up};
use tocsin_core::incident::Timestamp;
use tocsin_core::release::TimelineEvent;
use tocsin_core::status::{CheckKind, OutageEnding};
use tocsin_store::{
    Checks, Drafts, HistoryPage, NewCheckResult, NewOutage, Newest, Order, Projects, ResultFilter,
    Store, SubjectKind, TimelineQuery,
};

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
    let all = store
        .outages(page(1_000, None, 10), Checks::All)
        .await
        .unwrap();
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
            all[1].subject_kind,
            all[1].health,
            all[1].component_id.as_deref()
        ),
        ("c", CheckKind::Heartbeat, Degraded, Some("api"))
    );
    assert_eq!(all[0].subject_kind, CheckKind::Check);

    let one = store
        .outages(page(1_000, None, 2), Checks::All)
        .await
        .unwrap();
    assert_eq!(ids(&one), [open, inside]);
    let two = store
        .outages(page(1_000, Some(inside), 2), Checks::All)
        .await
        .unwrap();
    assert_eq!(ids(&two), [straddles]);
}

/// `Checks::Public` narrows the outage page and the timeline alike, pages
/// through only what it shows, and treats a hidden outage's id as a lost
/// cursor, so a reader cannot learn that one exists.
#[tokio::test]
async fn outages_narrow_to_the_checks_shown() {
    let (_dir, store) = open();
    let mut shown_ids = Vec::new();
    let mut hidden = 0;
    for at in [1_000, 2_000, 3_000] {
        let a = store.open_outage(outage("a", at, Down)).await.unwrap();
        store.end_outage(a, at + 500).await.unwrap();
        shown_ids.push(a);
        hidden = store.open_outage(outage("b", at + 1, Down)).await.unwrap();
        store.end_outage(hidden, at + 500).await.unwrap();
    }
    let shown = || Checks::Public {
        public: vec!["a".to_owned()],
        internal: vec!["b".to_owned()],
        internal_components: Vec::new(),
    };
    let read = |rows: Vec<tocsin_core::status::OutageRecord>| -> Vec<(i64, String)> {
        rows.into_iter().map(|row| (row.id, row.check_id)).collect()
    };
    let first = store.outages(page(0, None, 2), shown()).await.unwrap();
    assert_eq!(
        read(first),
        [
            (shown_ids[2], "a".to_owned()),
            (shown_ids[1], "a".to_owned())
        ]
    );
    let rest = store
        .outages(page(0, Some(shown_ids[1]), 2), shown())
        .await
        .unwrap();
    assert_eq!(read(rest), [(shown_ids[0], "a".to_owned())]);
    let error = store
        .outages(page(0, Some(hidden), 2), shown())
        .await
        .unwrap_err();
    assert!(
        matches!(&error, tocsin_store::Error::Invalid(fields) if fields[0].path == "before"),
        "{error}"
    );
    // Every check's, unnarrowed.
    assert_eq!(
        store
            .outages(page(0, None, 10), Checks::All)
            .await
            .unwrap()
            .len(),
        6
    );
    assert_eq!(
        store
            .outages(
                page(0, None, 10),
                Checks::Public {
                    public: Vec::new(),
                    internal: vec!["a".to_owned(), "b".to_owned()],
                    internal_components: Vec::new(),
                }
            )
            .await
            .unwrap(),
        []
    );

    let timeline = store
        .timeline(TimelineQuery {
            drafts: Drafts::Include,
            visible: Projects::All,
            shown: shown(),
            since: Timestamp::from_millis(0),
            after: None,
            order: Order::OldestFirst,
            limit: 10,
            component: None,
        })
        .await
        .unwrap();
    let checks: Vec<_> = timeline
        .events
        .iter()
        .filter_map(|event| match event {
            TimelineEvent::Outage { check_id, .. } => Some(check_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(checks, ["a", "a", "a"]);
}

/// A recovery and a withdrawal both end an outage, and every read says
/// which: a removed check's outage must never read as recovered.
#[tokio::test]
async fn an_outage_reads_back_how_it_ended() {
    let (_dir, store) = open();
    let recovered = store.open_outage(outage("a", 1_000, Down)).await.unwrap();
    store.end_outage(recovered, 2_000).await.unwrap();
    let withdrawn = store.open_outage(outage("b", 1_100, Down)).await.unwrap();
    store.withdraw_outage(withdrawn, 3_000).await.unwrap();
    let open = store.open_outage(outage("d", 1_200, Down)).await.unwrap();

    let endings: Vec<_> = store
        .outages(page(0, None, 10), Checks::All)
        .await
        .unwrap()
        .into_iter()
        .map(|outage| {
            (
                outage.id,
                outage.ended_at.map(Timestamp::as_millis),
                outage.ending,
            )
        })
        .collect();
    assert_eq!(
        endings,
        [
            (open, None, None),
            (withdrawn, Some(3_000), Some(OutageEnding::Withdrawn)),
            (recovered, Some(2_000), Some(OutageEnding::Recovered)),
        ]
    );

    let timeline = store
        .timeline(TimelineQuery {
            drafts: Drafts::Include,
            visible: Projects::All,
            shown: Checks::All,
            since: Timestamp::from_millis(0),
            after: None,
            order: Order::OldestFirst,
            limit: 10,
            component: None,
        })
        .await
        .unwrap();
    let endings: Vec<_> = timeline
        .events
        .iter()
        .filter_map(|event| match event {
            TimelineEvent::Outage { id, ending, .. } => Some((*id, *ending)),
            _ => None,
        })
        .collect();
    assert_eq!(
        endings,
        [
            (recovered, Some(OutageEnding::Recovered)),
            (withdrawn, Some(OutageEnding::Withdrawn)),
            (open, None),
        ]
    );
}

#[tokio::test]
async fn outages_page_through_ties_and_both_kinds_of_straddler() {
    let (_dir, store) = open();
    // Still open since before the window, and ended inside it.
    let open_before = store.open_outage(outage("a", 400, Down)).await.unwrap();
    let ended_inside = store.open_outage(outage("b", 600, Down)).await.unwrap();
    store.end_outage(ended_inside, 1_100).await.unwrap();
    // Three starting in the same millisecond, inside the window.
    let tie_a = store.open_outage(outage("d", 2_000, Down)).await.unwrap();
    let tie_b = store.open_outage(outage("e", 2_000, Down)).await.unwrap();
    let tie_c = store.open_outage(outage("f", 2_000, Down)).await.unwrap();

    let mut seen = Vec::new();
    let mut before = None;
    loop {
        let rows = store
            .outages(page(1_000, before, 2), Checks::All)
            .await
            .unwrap();
        if rows.is_empty() {
            break;
        }
        seen.extend(rows.iter().map(|row| row.id));
        before = rows.last().map(|row| row.id);
    }
    assert_eq!(
        seen,
        [tie_c, tie_b, tie_a, ended_inside, open_before],
        "ties by id, each row once, straddlers last"
    );
}

#[tokio::test]
async fn a_cursor_that_names_no_row_of_the_read_is_invalid() {
    let (_dir, store) = open();
    let api = store.append(result("api", 1_000, Up)).await.unwrap();
    let web = store.append(result("web", 1_000, Up)).await.unwrap();
    let outage = store.open_outage(outage("a", 1_000, Down)).await.unwrap();

    for (check, before) in [("api", web), ("api", web + 100), ("web", api)] {
        let error = store
            .check_results(check, page(0, Some(before), 10))
            .await
            .unwrap_err();
        let tocsin_store::Error::Invalid(fields) = &error else {
            panic!("{check} before {before}: {error}");
        };
        assert_eq!(fields[0].path, "before", "{fields:?}");
        assert!(
            fields[0].message.contains("start again without before"),
            "{fields:?}"
        );
    }
    let error = store
        .outages(page(0, Some(outage + 100), 10), Checks::All)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, tocsin_store::Error::Invalid(fields) if fields[0].path == "before"),
        "{error}"
    );
    // The cursor's own row is fine, on either read.
    let after_own = store
        .check_results("api", page(0, Some(api), 10))
        .await
        .unwrap();
    assert_eq!(after_own, []);
    let after_own = store
        .outages(page(0, Some(outage), 10), Checks::All)
        .await
        .unwrap();
    assert_eq!(after_own, []);
}

#[tokio::test]
async fn a_heartbeats_pings_read_back_as_its_results() {
    let (_dir, store) = open();
    let ping = store
        .record(result("backup", 1_000, Up), SubjectKind::Heartbeat, None)
        .await
        .unwrap();
    assert_eq!(ping, None, "no outage change");
    let missed = store
        .record(result("backup", 2_000, Down), SubjectKind::Heartbeat, None)
        .await
        .unwrap();
    assert_eq!(missed, None);
    let rows = store
        .check_results("backup", page(0, None, 10))
        .await
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|row| (row.started_at.as_millis(), row.health))
            .collect::<Vec<_>>(),
        [(2_000, Down), (1_000, Up)]
    );
    let older = store
        .check_results("backup", page(0, Some(rows[0].id), 10))
        .await
        .unwrap();
    assert_eq!(older.len(), 1, "a heartbeat's cursor pages too");
}

fn newest(check: &str, since_ms: i64, filter: ResultFilter) -> Newest {
    Newest {
        check_id: check.into(),
        since: Timestamp::from_millis(since_ms),
        filter,
    }
}

#[tokio::test]
async fn the_newest_result_of_each_kind_is_found_per_check() {
    let (_dir, store) = open();
    let failed = store.append(result("api", 1_000, Degraded)).await.unwrap();
    let latest = store.append(result("api", 2_000, Up)).await.unwrap();
    store.append(result("web", 3_000, Down)).await.unwrap();
    let mut ping = result("job", 1_000, Up);
    ping.error = None;
    let ping = store.append(ping).await.unwrap();
    let mut miss = result("job", 2_000, Down);
    miss.error = Some("no ping within the interval of 1h plus 1m grace".to_owned());
    let miss = store.append(miss).await.unwrap();

    let found = store
        .newest_results(vec![
            newest("api", 0, ResultFilter::Any),
            newest("api", 0, ResultFilter::NotUp),
            // The failure is before the window.
            newest("api", 1_500, ResultFilter::NotUp),
            newest("job", 0, ResultFilter::Any),
            newest(
                "job",
                0,
                ResultFilter::ErrorNotStartingWith("no ping within".to_owned()),
            ),
            newest("gone", 0, ResultFilter::Any),
        ])
        .await
        .unwrap();
    let ids: Vec<Option<i64>> = found
        .iter()
        .map(|row| row.as_ref().map(|row| row.id))
        .collect();
    assert_eq!(
        ids,
        [
            Some(latest),
            Some(failed),
            None,
            Some(miss),
            Some(ping),
            None
        ]
    );
    let failed = found[1].as_ref().unwrap();
    assert_eq!(failed.error.as_deref(), Some("status 503"));
    assert!(failed.detail.is_some(), "the detail reads back");
}

#[tokio::test]
async fn results_slice_into_latency_and_failure_counts() {
    let (_dir, store) = open();
    for (at, health, duration) in [
        (900, Up, 10),   // before the window
        (1_000, Up, 10), // slice 0
        (1_500, Degraded, 30),
        (1_999, Down, 5_000), // down: counted, not timed
        (2_000, Down, 5_000), // slice 1: down only
        (3_000, Up, 20),      // slice 2
    ] {
        let mut row = result("api", at, health);
        row.duration_ms = duration;
        store.append(row).await.unwrap();
    }
    store.append(result("web", 1_200, Up)).await.unwrap();

    let slices = store
        .result_slices(Timestamp::from_millis(1_000), 1_000, None)
        .await
        .unwrap();
    let api: Vec<_> = slices
        .iter()
        .filter(|slice| slice.check_id == "api")
        .collect();
    assert_eq!(api.len(), 3, "{api:?}");
    assert_eq!(
        (api[0].slice, api[0].results, api[0].degraded, api[0].down),
        (0, 3, 1, 1)
    );
    assert_eq!(api[0].mean_ms, Some(20.0));
    assert_eq!((api[0].min_ms, api[0].max_ms), (Some(10), Some(30)));
    assert_eq!((api[1].slice, api[1].down, api[1].mean_ms), (1, 1, None));
    assert_eq!((api[2].slice, api[2].min_ms), (2, Some(20)));
    assert!(
        slices
            .iter()
            .any(|slice| slice.check_id == "web" && slice.slice == 0)
    );

    // Asked for one check, the same slices of that check alone.
    let only = store
        .result_slices(Timestamp::from_millis(1_000), 1_000, Some("api".into()))
        .await
        .unwrap();
    assert_eq!(only.iter().collect::<Vec<_>>(), api);
}

/// A check no longer configured has only its outages to say where it was:
/// one recorded on a public component stays public, one recorded on none
/// or on a component that is not public does not. A configured check follows where it is now, whatever its outages
/// recorded.
#[tokio::test]
async fn outages_of_removed_checks_follow_the_component_they_recorded() {
    let (_dir, store) = open();
    let on = |check: &str, component: Option<&str>, at| NewOutage {
        component_id: component.map(Into::into),
        ..outage(check, at, Down)
    };
    // Removed from the status page, and removed while internal.
    let removed_public = store
        .open_outage(on("gone", Some("api"), 1_000))
        .await
        .unwrap();
    store
        .open_outage(on("gone-internal", None, 1_001))
        .await
        .unwrap();
    // Moved off its component: recorded on one, internal now.
    store
        .open_outage(on("moved-off", Some("api"), 1_002))
        .await
        .unwrap();
    // Moved onto one: recorded on none, public now.
    let moved_on = store
        .open_outage(on("moved-on", None, 1_003))
        .await
        .unwrap();
    // Removed while on a component that is not public.
    store
        .open_outage(on("gone-private", Some("hosts"), 1_004))
        .await
        .unwrap();
    let shown = Checks::Public {
        public: vec!["moved-on".to_owned()],
        internal: vec!["moved-off".to_owned()],
        internal_components: vec!["hosts".to_owned()],
    };
    let ids: Vec<i64> = store
        .outages(page(0, None, 10), shown.clone())
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.id)
        .collect();
    assert_eq!(ids, [moved_on, removed_public]);

    let timeline = store
        .timeline(TimelineQuery {
            drafts: Drafts::Include,
            visible: Projects::All,
            shown,
            since: Timestamp::from_millis(0),
            after: None,
            order: Order::OldestFirst,
            limit: 10,
            component: None,
        })
        .await
        .unwrap();
    let checks: Vec<_> = timeline
        .events
        .iter()
        .filter_map(|event| match event {
            TimelineEvent::Outage { check_id, .. } => Some(check_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(checks, ["gone", "moved-on"]);
}

#[tokio::test]
async fn outages_open_under_other_nodes_are_found() {
    let (_dir, store) = open();
    let on = |node: &str, check: &str| NewOutage {
        node_id: node.into(),
        ..outage(check, 1_000, Down)
    };
    store.open_outage(on("node-a", "a")).await.unwrap();
    store.open_outage(on("old-host", "c")).await.unwrap();
    store.open_outage(on("old-host", "b")).await.unwrap();
    let ended = store.open_outage(on("old-host", "d")).await.unwrap();
    store.end_outage(ended, 2_000).await.unwrap();

    let elsewhere = store.open_outages_elsewhere("node-a").await.unwrap();
    let found: Vec<(&str, SubjectKind, &str)> = elsewhere
        .iter()
        .map(|outage| {
            (
                outage.node_id.as_str(),
                outage.subject_kind,
                outage.check_id.as_str(),
            )
        })
        .collect();
    assert_eq!(
        found,
        [
            ("old-host", SubjectKind::Check, "b"),
            ("old-host", SubjectKind::Heartbeat, "c"),
        ],
        "open ones only, never this node's"
    );
    assert!(
        store
            .open_outages_elsewhere("old-host")
            .await
            .unwrap()
            .iter()
            .all(|outage| outage.node_id == "node-a")
    );
}
