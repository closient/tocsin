// SPDX-License-Identifier: AGPL-3.0-only

//! The alerting outbox: deliveries are queued in the outage change's
//! transaction, in an order that never lets a resolve overtake its fire.

use std::path::PathBuf;

use tocsin_core::Health;
use tocsin_store::{
    AlertRouting, AlertTarget, DeliveryKind, DeliveryOutcome, DeliveryRetention, NewOutage,
    OutageChange, OutageEnd, Store, SubjectKind,
};

const T0: i64 = 1_800_000_000_000;

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

fn target(notifier: &str, resolve: bool, repeat_ms: Option<i64>) -> AlertTarget {
    AlertTarget {
        notifier_id: notifier.into(),
        route_id: "ops".into(),
        resolve,
        repeat_ms,
    }
}

fn routing(targets: Vec<AlertTarget>) -> AlertRouting {
    AlertRouting {
        subject_kind: SubjectKind::Check,
        targets,
    }
}

fn outage(health: Health, started_at_ms: i64, alerted: bool) -> NewOutage {
    NewOutage {
        node_id: "node-a".into(),
        check_id: "api".into(),
        component_id: Some("web".into()),
        health,
        started_at_ms,
        alerted,
    }
}

/// Opens an outage (ending `previous`, if any); resolves with its id.
async fn open_outage(
    store: &Store,
    previous: Option<i64>,
    opened: NewOutage,
    routing: impl Into<Option<AlertRouting>>,
) -> i64 {
    let at = opened.started_at_ms;
    let change = OutageChange {
        end: previous.map(|id| OutageEnd {
            id,
            ended_at_ms: at,
        }),
        open: Some(opened),
    };
    store
        .change_outage_alerting(change, routing.into(), at)
        .await
        .unwrap()
        .opened
        .unwrap()
}

async fn recover(store: &Store, id: i64, at: i64) -> usize {
    let change = OutageChange {
        end: Some(OutageEnd {
            id,
            ended_at_ms: at,
        }),
        open: None,
    };
    store
        .change_outage_alerting(change, None, at)
        .await
        .unwrap()
        .queued
}

/// Sends every head the outbox offers, as delivered, until it offers none;
/// returns (notifier, kind, health, alert id) in send order.
async fn drain(store: &Store, now: i64) -> Vec<(String, DeliveryKind, Health, i64)> {
    let mut sent = Vec::new();
    loop {
        let outbox = store.outbox().await.unwrap();
        let due: Vec<_> = outbox
            .pending
            .into_iter()
            .filter(|head| head.next_attempt_at_ms <= now)
            .collect();
        if due.is_empty() {
            return sent;
        }
        for head in due {
            sent.push((
                head.alert.notifier_id.clone(),
                head.kind,
                head.health,
                head.alert.id,
            ));
            store
                .finish_delivery(head.id, DeliveryOutcome::Delivered, now)
                .await
                .unwrap();
        }
    }
}

#[tokio::test]
async fn a_recovery_resolves_the_alert_its_outage_fired_with_the_same_identity() {
    let Fixture { _dir, store, .. } = open();
    let id = open_outage(
        &store,
        None,
        outage(Health::Down, T0, true),
        routing(vec![
            target("slack", true, None),
            target("pager", true, None),
        ]),
    )
    .await;
    assert_eq!(recover(&store, id, T0 + 90_000).await, 2);

    let alerts = store.alerts().await.unwrap();
    assert_eq!(alerts.len(), 2);
    for alert in &alerts {
        assert_eq!(
            (alert.node_id.as_str(), alert.subject_id.as_str()),
            ("node-a", "api")
        );
        assert_eq!(alert.subject_kind, SubjectKind::Check);
        assert_eq!(alert.started_at_ms, T0);
        assert_eq!(alert.ended_at_ms, Some(T0 + 90_000));
    }
    let sent = drain(&store, T0 + 90_000).await;
    let mut by_alert: Vec<_> = sent
        .iter()
        .map(|(n, k, _, a)| (*a, n.clone(), *k))
        .collect();
    by_alert.sort_by_key(|(alert, _, kind)| (*alert, *kind == DeliveryKind::Resolve));
    assert_eq!(
        by_alert,
        vec![
            (alerts[0].id, "slack".into(), DeliveryKind::Fire),
            (alerts[0].id, "slack".into(), DeliveryKind::Resolve),
            (alerts[1].id, "pager".into(), DeliveryKind::Fire),
            (alerts[1].id, "pager".into(), DeliveryKind::Resolve),
        ]
    );
}

#[tokio::test]
async fn a_resolve_waits_until_its_fire_is_delivered_or_has_failed() {
    let Fixture { _dir, store, .. } = open();
    let id = open_outage(
        &store,
        None,
        outage(Health::Down, T0, true),
        routing(vec![target("slack", true, None)]),
    )
    .await;
    recover(&store, id, T0 + 1_000).await;

    // Only the fire is offered while it is pending, even retrying.
    let outbox = store.outbox().await.unwrap();
    assert_eq!(outbox.pending.len(), 1);
    assert_eq!(outbox.pending[0].kind, DeliveryKind::Fire);
    let fire = outbox.pending[0].id;
    store
        .finish_delivery(
            fire,
            DeliveryOutcome::Retry {
                at_ms: T0 + 60_000,
                error: "503".into(),
            },
            T0 + 2_000,
        )
        .await
        .unwrap();
    let outbox = store.outbox().await.unwrap();
    assert_eq!(outbox.pending.len(), 1);
    assert_eq!(outbox.pending[0].kind, DeliveryKind::Fire);
    assert_eq!(outbox.pending[0].attempts, 1);
    assert_eq!(outbox.pending[0].next_attempt_at_ms, T0 + 60_000);

    // A fire that failed for good unblocks the resolve.
    store
        .finish_delivery(
            fire,
            DeliveryOutcome::Failed {
                error: "410".into(),
            },
            T0 + 60_000,
        )
        .await
        .unwrap();
    let outbox = store.outbox().await.unwrap();
    assert_eq!(outbox.pending.len(), 1);
    assert_eq!(outbox.pending[0].kind, DeliveryKind::Resolve);
    let failed = store.failed_deliveries(10).await.unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].kind, DeliveryKind::Fire);
    assert_eq!(failed[0].attempts, 2);
    assert_eq!(failed[0].error.as_deref(), Some("410"));
    assert_eq!(failed[0].alert.notifier_id, "slack");
}

#[tokio::test]
async fn an_outage_that_did_not_alert_queues_nothing_and_its_recovery_resolves_nothing() {
    let Fixture { _dir, store, .. } = open();
    // The engine decided this outage, opened in maintenance, does not alert:
    // there is no routing and alerted is false.
    let id = open_outage(&store, None, outage(Health::Down, T0, false), None).await;
    assert_eq!(recover(&store, id, T0 + 1_000).await, 0);
    assert_eq!(store.alerts().await.unwrap().len(), 0);
    assert_eq!(store.outbox().await.unwrap().pending.len(), 0);
}

#[tokio::test]
async fn a_health_change_refires_the_same_alert_and_reaches_new_targets() {
    let Fixture { _dir, store, .. } = open();
    let degraded = open_outage(
        &store,
        None,
        outage(Health::Degraded, T0, true),
        routing(vec![target("slack", true, None)]),
    )
    .await;
    drain(&store, T0).await;
    // Down also matches a route that wants only down.
    let down = open_outage(
        &store,
        Some(degraded),
        outage(Health::Down, T0 + 60_000, true),
        routing(vec![
            target("slack", true, None),
            target("pager", true, None),
        ]),
    )
    .await;
    let sent = drain(&store, T0 + 60_000).await;
    let alerts = store.alerts().await.unwrap();
    assert_eq!(alerts.len(), 2, "slack keeps its alert, pager gets one");
    assert_eq!(alerts[0].notifier_id, "slack");
    assert_eq!(
        alerts[0].started_at_ms, T0,
        "the slack alert spans both outages"
    );
    assert_eq!(alerts[1].started_at_ms, T0 + 60_000);
    assert_eq!(
        sent,
        vec![
            (
                "slack".into(),
                DeliveryKind::Fire,
                Health::Down,
                alerts[0].id
            ),
            (
                "pager".into(),
                DeliveryKind::Fire,
                Health::Down,
                alerts[1].id
            ),
        ]
    );
    recover(&store, down, T0 + 120_000).await;
    let sent = drain(&store, T0 + 120_000).await;
    assert_eq!(
        sent,
        vec![
            (
                "slack".into(),
                DeliveryKind::Resolve,
                Health::Down,
                alerts[0].id
            ),
            (
                "pager".into(),
                DeliveryKind::Resolve,
                Health::Down,
                alerts[1].id
            ),
        ]
    );
}

#[tokio::test]
async fn a_route_with_resolve_off_ends_the_alert_without_a_resolve() {
    let Fixture { _dir, store, .. } = open();
    let id = open_outage(
        &store,
        None,
        outage(Health::Down, T0, true),
        routing(vec![target("slack", false, None)]),
    )
    .await;
    drain(&store, T0).await;
    assert_eq!(recover(&store, id, T0 + 1_000).await, 0);
    assert_eq!(
        store.alerts().await.unwrap()[0].ended_at_ms,
        Some(T0 + 1_000)
    );
    assert_eq!(store.outbox().await.unwrap().pending.len(), 0);
}

#[tokio::test]
async fn repeats_are_queued_from_the_open_alert_until_it_resolves() {
    let Fixture { _dir, store, .. } = open();
    let id = open_outage(
        &store,
        None,
        outage(Health::Down, T0, true),
        routing(vec![target("slack", true, Some(3_600_000))]),
    )
    .await;
    // Nothing repeats while the fire is pending.
    assert_eq!(store.outbox().await.unwrap().next_repeat_at_ms, None);
    drain(&store, T0).await;
    assert_eq!(
        store.outbox().await.unwrap().next_repeat_at_ms,
        Some(T0 + 3_600_000)
    );
    assert_eq!(store.queue_repeats(T0 + 3_599_999).await.unwrap(), 0);
    assert_eq!(store.queue_repeats(T0 + 3_600_000).await.unwrap(), 1);
    assert_eq!(
        store.queue_repeats(T0 + 3_600_000).await.unwrap(),
        0,
        "one repeat at a time"
    );
    let sent = drain(&store, T0 + 3_600_000).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1, DeliveryKind::Repeat);
    assert_eq!(
        store.outbox().await.unwrap().next_repeat_at_ms,
        Some(T0 + 7_200_000)
    );

    // A repeat still pending at recovery is dropped: the resolve says more.
    assert_eq!(store.queue_repeats(T0 + 7_200_000).await.unwrap(), 1);
    recover(&store, id, T0 + 7_200_001).await;
    let sent = drain(&store, T0 + 7_200_001).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1, DeliveryKind::Resolve);
    assert_eq!(store.outbox().await.unwrap().next_repeat_at_ms, None);
    assert_eq!(store.queue_repeats(T0 + 99_000_000).await.unwrap(), 0);
}

#[tokio::test]
async fn queued_deliveries_survive_a_restart() {
    let Fixture { _dir, path, store } = open();
    let id = open_outage(
        &store,
        None,
        outage(Health::Down, T0, true),
        routing(vec![target("slack", true, None)]),
    )
    .await;
    recover(&store, id, T0 + 1_000).await;
    store.close().await.unwrap();

    let store = Store::open(&path).unwrap();
    let sent = drain(&store, T0 + 1_000).await;
    let kinds: Vec<_> = sent.iter().map(|(_, kind, _, _)| *kind).collect();
    assert_eq!(kinds, vec![DeliveryKind::Fire, DeliveryKind::Resolve]);
}

#[tokio::test]
async fn a_failed_outage_change_queues_nothing() {
    let Fixture { _dir, store, .. } = open();
    open_outage(
        &store,
        None,
        outage(Health::Down, T0, true),
        routing(vec![target("slack", true, None)]),
    )
    .await;
    drain(&store, T0).await;
    // A second open outage for the same check breaks the unique index: the
    // whole transaction, alert rows included, rolls back.
    let change = OutageChange {
        end: None,
        open: Some(outage(Health::Down, T0 + 1, true)),
    };
    let routed = routing(vec![target("pager", true, None)]);
    assert!(
        store
            .change_outage_alerting(change, Some(routed), T0 + 1)
            .await
            .is_err()
    );
    assert_eq!(store.alerts().await.unwrap().len(), 1);
    assert_eq!(store.outbox().await.unwrap().pending.len(), 0);
}

#[tokio::test]
async fn pruning_drops_old_finished_deliveries_and_their_ended_alerts() {
    let Fixture { _dir, store, .. } = open();
    let id = open_outage(
        &store,
        None,
        outage(Health::Down, T0, true),
        routing(vec![target("slack", true, None)]),
    )
    .await;
    recover(&store, id, T0 + 1_000).await;
    drain(&store, T0 + 1_000).await;
    let retention = DeliveryRetention {
        delivered_ms: 10_000,
        failed_ms: 100_000,
    };
    assert_eq!(
        store.prune_deliveries(retention, T0 + 5_000).await.unwrap(),
        0
    );
    assert_eq!(store.alerts().await.unwrap().len(), 1);
    assert_eq!(
        store
            .prune_deliveries(retention, T0 + 20_000)
            .await
            .unwrap(),
        2
    );
    assert_eq!(store.alerts().await.unwrap().len(), 0);
}
