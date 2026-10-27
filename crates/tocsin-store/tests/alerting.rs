// SPDX-License-Identifier: AGPL-3.0-only

//! The alerting outbox: deliveries are queued in the outage change's
//! transaction, in an order that never lets a resolve overtake its fire.

use std::path::PathBuf;
use std::time::Duration;

use tocsin_core::Health;
use tocsin_core::alert::{AlertState, DeliveryStatus, MessageKind};
use tocsin_core::incident::Timestamp;
use tocsin_store::{
    AlertRouting, AlertTarget, DeliveryKind, DeliveryOutcome, DeliveryPrune, DeliveryRetention,
    EndedBy, Error, HistoryPage, NewOutage, OutageChange, OutageEnd, Store, SubjectKind,
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

fn target(notifier: &str, repeat_ms: Option<i64>) -> AlertTarget {
    AlertTarget {
        notifier_id: notifier.into(),
        route_id: "ops".into(),
        repeat_ms,
    }
}

fn routing(targets: Vec<AlertTarget>) -> AlertRouting {
    AlertRouting {
        targets,
        ..AlertRouting::default()
    }
}

/// Routing whose `ringing` notifiers keep ringing at the outage's health.
fn ringing(targets: Vec<AlertTarget>, ringing: &[&str]) -> AlertRouting {
    AlertRouting {
        targets,
        ringing: ringing.iter().map(|id| (*id).to_owned()).collect(),
        ..AlertRouting::default()
    }
}

fn outage(health: Health, started_at_ms: i64, alerted: bool) -> NewOutage {
    NewOutage {
        node_id: "node-a".into(),
        subject_kind: SubjectKind::Check,
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
        routing(vec![target("slack", None), target("pager", None)]),
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
        routing(vec![target("slack", None)]),
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
        routing(vec![target("slack", None)]),
    )
    .await;
    drain(&store, T0).await;
    // Down also matches a route that wants only down.
    let down = open_outage(
        &store,
        Some(degraded),
        outage(Health::Down, T0 + 60_000, true),
        routing(vec![target("slack", None), target("pager", None)]),
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
async fn repeats_are_queued_from_the_open_alert_until_it_resolves() {
    let Fixture { _dir, store, .. } = open();
    let id = open_outage(
        &store,
        None,
        outage(Health::Down, T0, true),
        routing(vec![target("slack", Some(3_600_000))]),
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
        routing(vec![target("slack", None)]),
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
        routing(vec![target("slack", None)]),
    )
    .await;
    drain(&store, T0).await;
    // A second open outage for the same check breaks the unique index: the
    // whole transaction, alert rows included, rolls back.
    let change = OutageChange {
        end: None,
        open: Some(outage(Health::Down, T0 + 1, true)),
    };
    let routed = routing(vec![target("pager", None)]);
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
        routing(vec![target("slack", None)]),
    )
    .await;
    recover(&store, id, T0 + 1_000).await;
    drain(&store, T0 + 1_000).await;
    let retention = DeliveryRetention {
        delivered_ms: 10_000,
        failed_ms: 100_000,
        chunk_rows: 2_000,
        pause: Duration::ZERO,
    };
    assert_eq!(
        store.prune_deliveries(retention, T0 + 5_000).await.unwrap(),
        DeliveryPrune::default()
    );
    assert_eq!(store.alerts().await.unwrap().len(), 1);
    let pruned = store
        .prune_deliveries(retention, T0 + 20_000)
        .await
        .unwrap();
    assert_eq!((pruned.deliveries, pruned.alerts), (2, 1));
    assert_eq!(store.alerts().await.unwrap().len(), 0);
}

/// An outage for `kind` named `backup`.
fn backup(kind: SubjectKind, health: Health, started_at_ms: i64) -> NewOutage {
    NewOutage {
        subject_kind: kind,
        check_id: "backup".into(),
        component_id: None,
        ..outage(health, started_at_ms, true)
    }
}

#[tokio::test]
async fn a_check_and_a_heartbeat_sharing_an_id_fire_and_resolve_independently() {
    let Fixture { _dir, store, .. } = open();
    let check = open_outage(
        &store,
        None,
        backup(SubjectKind::Check, Health::Down, T0),
        routing(vec![target("slack", None)]),
    )
    .await;
    let heartbeat = open_outage(
        &store,
        None,
        backup(SubjectKind::Heartbeat, Health::Down, T0 + 1_000),
        routing(vec![target("slack", None)]),
    )
    .await;
    let sent = drain(&store, T0 + 1_000).await;
    let alerts = store.alerts().await.unwrap();
    assert_eq!(alerts.len(), 2, "one alert per kind, not one shared");
    assert_eq!(
        (alerts[0].subject_kind, alerts[1].subject_kind),
        (SubjectKind::Check, SubjectKind::Heartbeat)
    );
    assert_eq!(
        sent.iter()
            .map(|(_, kind, _, alert)| (*kind, *alert))
            .collect::<Vec<_>>(),
        vec![
            (DeliveryKind::Fire, alerts[0].id),
            (DeliveryKind::Fire, alerts[1].id)
        ]
    );

    // The heartbeat recovering resolves its own alert and leaves the
    // check's firing.
    assert_eq!(recover(&store, heartbeat, T0 + 2_000).await, 1);
    let sent = drain(&store, T0 + 2_000).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(
        (sent[0].1, sent[0].3),
        (DeliveryKind::Resolve, alerts[1].id)
    );
    let alerts = store.alerts().await.unwrap();
    assert_eq!(alerts[0].ended_at_ms, None);
    assert_eq!(alerts[1].ended_at_ms, Some(T0 + 2_000));

    assert_eq!(recover(&store, check, T0 + 3_000).await, 1);
    let sent = drain(&store, T0 + 3_000).await;
    assert_eq!(
        (sent[0].1, sent[0].3),
        (DeliveryKind::Resolve, alerts[0].id)
    );
}

/// Finishes the pending delivery of `kind` as `outcome`.
async fn finish(store: &Store, kind: DeliveryKind, outcome: DeliveryOutcome, now: i64) {
    let outbox = store.outbox().await.unwrap();
    let head = outbox
        .pending
        .iter()
        .find(|head| head.kind == kind)
        .unwrap_or_else(|| panic!("no pending {kind:?} in {:?}", outbox.pending));
    store.finish_delivery(head.id, outcome, now).await.unwrap();
}

/// The kinds the outbox offers, sorted by name: which may go matters here,
/// not the order they are listed in.
fn pending_kinds(outbox: &tocsin_store::Outbox) -> Vec<DeliveryKind> {
    let mut kinds: Vec<_> = outbox.pending.iter().map(|head| head.kind).collect();
    kinds.sort_by_key(|kind| kind.as_str());
    kinds
}

#[tokio::test]
async fn a_downgrade_from_ringing_queues_a_cancel_that_never_blocks_a_message() {
    let Fixture { _dir, store, .. } = open();
    let down = open_outage(
        &store,
        None,
        outage(Health::Down, T0, true),
        ringing(vec![target("pushover", None)], &["pushover"]),
    )
    .await;
    drain(&store, T0).await;
    assert!(store.alerts().await.unwrap()[0].ringing);

    // Down (an emergency) becomes Degraded (not one): the cancel is queued
    // at the downgrade, beside the re-fire, and neither waits for the other.
    let degraded = open_outage(
        &store,
        Some(down),
        outage(Health::Degraded, T0 + 60_000, true),
        routing(vec![target("pushover", None)]),
    )
    .await;
    assert!(!store.alerts().await.unwrap()[0].ringing);
    let outbox = store.outbox().await.unwrap();
    assert_eq!(
        pending_kinds(&outbox),
        vec![DeliveryKind::Cancel, DeliveryKind::Fire]
    );
    let cancel = outbox
        .pending
        .iter()
        .find(|head| head.kind == DeliveryKind::Cancel)
        .unwrap();
    assert_eq!(cancel.health, Health::Down, "it cancels what rang");

    // A cancel that keeps failing holds up nothing: the fire, and the
    // resolve after it, go out while it retries.
    finish(
        &store,
        DeliveryKind::Cancel,
        DeliveryOutcome::Retry {
            at_ms: T0 + 999_000,
            error: "500".into(),
        },
        T0 + 60_000,
    )
    .await;
    finish(
        &store,
        DeliveryKind::Fire,
        DeliveryOutcome::Delivered,
        T0 + 60_000,
    )
    .await;
    // Recovering after the downgrade queues no second cancel: nothing
    // rings any more.
    assert_eq!(recover(&store, degraded, T0 + 120_000).await, 1);
    let outbox = store.outbox().await.unwrap();
    assert_eq!(
        pending_kinds(&outbox),
        vec![DeliveryKind::Cancel, DeliveryKind::Resolve]
    );
    finish(
        &store,
        DeliveryKind::Resolve,
        DeliveryOutcome::Delivered,
        T0 + 120_000,
    )
    .await;
    // The cancel fails for good on its own record.
    finish(
        &store,
        DeliveryKind::Cancel,
        DeliveryOutcome::Failed {
            error: "500".into(),
        },
        T0 + 999_000,
    )
    .await;
    let failed = store.failed_deliveries(10).await.unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(
        (failed[0].kind, failed[0].attempts),
        (DeliveryKind::Cancel, 2)
    );
    assert_eq!(store.outbox().await.unwrap().pending.len(), 0);
}

#[tokio::test]
async fn a_cancel_waits_for_the_ringing_message_it_cancels() {
    let Fixture { _dir, store, .. } = open();
    let id = open_outage(
        &store,
        None,
        outage(Health::Down, T0, true),
        ringing(vec![target("pushover", None)], &["pushover"]),
    )
    .await;
    // The emergency is still pending when the outage recovers: cancelling
    // first would leave it ringing once it lands.
    assert_eq!(recover(&store, id, T0 + 1_000).await, 2);
    let outbox = store.outbox().await.unwrap();
    assert_eq!(pending_kinds(&outbox), vec![DeliveryKind::Fire]);
    finish(
        &store,
        DeliveryKind::Fire,
        DeliveryOutcome::Delivered,
        T0 + 1_000,
    )
    .await;
    let outbox = store.outbox().await.unwrap();
    assert_eq!(
        pending_kinds(&outbox),
        vec![DeliveryKind::Cancel, DeliveryKind::Resolve]
    );
}

#[tokio::test]
async fn a_large_prune_deletes_in_chunks() {
    let Fixture { _dir, store, .. } = open();
    for cycle in 0..40 {
        let at = T0 + cycle * 10;
        let id = open_outage(
            &store,
            None,
            outage(Health::Down, at, true),
            routing(vec![target("slack", None)]),
        )
        .await;
        recover(&store, id, at + 5).await;
        drain(&store, at + 5).await;
    }
    assert_eq!(store.alerts().await.unwrap().len(), 40);
    let retention = DeliveryRetention {
        delivered_ms: 1_000,
        failed_ms: 1_000,
        chunk_rows: 7,
        pause: Duration::from_millis(1),
    };
    let pruned = store
        .prune_deliveries(retention, T0 + 100_000)
        .await
        .unwrap();
    // 80 deliveries in chunks of 7 is 12 deletes; 40 alerts is 6.
    assert_eq!(
        pruned,
        DeliveryPrune {
            deliveries: 80,
            alerts: 40,
            chunks: 12 + 6,
        }
    );
    assert_eq!(store.alerts().await.unwrap().len(), 0);
}

#[tokio::test]
async fn withdrawing_an_outage_resolves_its_alerts_as_ended_by_the_configuration() {
    let Fixture { _dir, store, .. } = open();
    let id = open_outage(
        &store,
        None,
        outage(Health::Down, T0, true),
        ringing(
            vec![target("slack", Some(60_000)), target("pushover", None)],
            &["pushover"],
        ),
    )
    .await;
    drain(&store, T0).await;

    // The check was removed: the pushover emergency that rang is cancelled,
    // and every notifier that heard the fire hears it end, marked as ended
    // by the configuration rather than by a recovery.
    assert_eq!(store.withdraw_outage(id, T0 + 90_000).await.unwrap(), 3);
    assert_eq!(store.open_outages("node-a").await.unwrap(), []);
    for alert in store.alerts().await.unwrap() {
        assert_eq!(alert.ended_at_ms, Some(T0 + 90_000));
        assert_eq!(alert.ended_by, Some(EndedBy::Config));
        assert!(!alert.ringing);
    }
    let sent = drain(&store, T0 + 90_000).await;
    let mut kinds: Vec<_> = sent
        .iter()
        .map(|(notifier, kind, health, _)| (notifier.as_str(), *kind, *health))
        .collect();
    kinds.sort_by_key(|(notifier, kind, _)| (*notifier, format!("{kind:?}")));
    assert_eq!(
        kinds,
        [
            ("pushover", DeliveryKind::Cancel, Health::Down),
            ("pushover", DeliveryKind::Resolve, Health::Down),
            ("slack", DeliveryKind::Resolve, Health::Down),
        ]
    );
    // No repeat follows, however long it waits.
    assert_eq!(store.queue_repeats(T0 + 999_000).await.unwrap(), 0);

    // Withdrawing it again changes nothing.
    assert_eq!(store.withdraw_outage(id, T0 + 99_000).await.unwrap(), 0);
    let alerts = store.alerts().await.unwrap();
    assert!(
        alerts
            .iter()
            .all(|alert| alert.ended_at_ms == Some(T0 + 90_000))
    );
    assert_eq!(store.outbox().await.unwrap().pending.len(), 0);
}

#[tokio::test]
async fn alerts_whose_notifier_is_gone_end_with_a_queued_resolve() {
    let Fixture { _dir, store, .. } = open();
    let id = open_outage(
        &store,
        None,
        outage(Health::Down, T0, true),
        routing(vec![
            target("slack", Some(60_000)),
            target("gone", Some(60_000)),
        ]),
    )
    .await;
    drain(&store, T0).await;

    let ended = store
        .end_alerts_without_notifier(vec!["slack".into()], T0 + 30_000)
        .await
        .unwrap();
    assert_eq!(ended, 1);
    // The gone notifier's alert ends with a resolve queued for whoever can
    // still send it: the worker through the notifier it keeps until then, or
    // nobody, in which case the resolve fails and is counted as abandoned.
    let gone = store
        .alerts()
        .await
        .unwrap()
        .into_iter()
        .find(|alert| alert.notifier_id == "gone")
        .unwrap();
    assert_eq!(gone.ended_at_ms, Some(T0 + 30_000));
    assert_eq!(gone.ended_by, Some(EndedBy::Config));
    let sent = drain(&store, T0 + 30_000).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(
        (sent[0].0.as_str(), sent[0].1),
        ("gone", DeliveryKind::Resolve)
    );
    assert_eq!(
        store
            .notifiers_with_unfinished_alerts(vec!["slack".into(), "gone".into()])
            .await
            .unwrap(),
        ["slack"]
    );
    // Only the kept notifier repeats, and only it hears the recovery.
    assert_eq!(store.queue_repeats(T0 + 60_000).await.unwrap(), 1);
    drain(&store, T0 + 60_000).await;
    assert_eq!(recover(&store, id, T0 + 90_000).await, 1);
    let sent = drain(&store, T0 + 90_000).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(
        (sent[0].0.as_str(), sent[0].1),
        ("slack", DeliveryKind::Resolve)
    );
    let slack = store
        .alerts()
        .await
        .unwrap()
        .into_iter()
        .find(|alert| alert.notifier_id == "slack")
        .unwrap();
    assert_eq!(slack.ended_by, Some(EndedBy::Recovery));
}

fn history_page(since_ms: i64, before: Option<i64>, limit: u32) -> HistoryPage {
    HistoryPage {
        since: Timestamp::from_millis(since_ms),
        before,
        limit,
    }
}

/// One resolved alert whose fire failed for good after a retry, and a
/// later one still firing whose fire is pending; resolves with their ids.
async fn two_alerts(store: &Store) -> (i64, i64) {
    let first = open_outage(
        store,
        None,
        outage(Health::Down, T0, true),
        routing(vec![target("slack", None)]),
    )
    .await;
    recover(store, first, T0 + 1_000).await;
    let fire = store.outbox().await.unwrap().pending[0].id;
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
    drain(store, T0 + 61_000).await;
    open_outage(
        store,
        None,
        outage(Health::Degraded, T0 + 100_000, true),
        routing(vec![target("slack", None)]),
    )
    .await;
    let alerts = store.alerts().await.unwrap();
    assert_eq!(alerts.len(), 2);
    (alerts[0].id, alerts[1].id)
}

#[tokio::test]
async fn alert_history_lists_each_alert_newest_first_with_how_its_deliveries_went() {
    let Fixture { _dir, store, .. } = open();
    let (resolved, firing) = two_alerts(&store).await;

    let entries = store
        .alert_history(history_page(T0, None, 10), None)
        .await
        .unwrap();
    let ids: Vec<i64> = entries.iter().map(|entry| entry.alert.id).collect();
    assert_eq!(ids, [firing, resolved]);

    let now = &entries[0];
    assert_eq!(now.alert.ended_at_ms, None);
    assert_eq!(now.alert.health, Health::Degraded);
    assert_eq!(now.deliveries.len(), 1);
    assert_eq!(now.deliveries[0].kind, MessageKind::Fire);
    assert_eq!(now.deliveries[0].status, DeliveryStatus::Pending);
    assert_eq!(now.deliveries[0].attempts, 0);
    assert_eq!(now.deliveries[0].finished_at, None);

    let then = &entries[1];
    assert_eq!(then.alert.ended_at_ms, Some(T0 + 1_000));
    let kinds: Vec<_> = then
        .deliveries
        .iter()
        .map(|delivery| (delivery.kind, delivery.status, delivery.attempts))
        .collect();
    assert_eq!(
        kinds,
        [
            (MessageKind::Fire, DeliveryStatus::Failed, 2),
            (MessageKind::Resolve, DeliveryStatus::Delivered, 1),
        ]
    );
    assert_eq!(then.deliveries[0].last_error.as_deref(), Some("410"));
    assert_eq!(
        then.deliveries[0].finished_at,
        Some(Timestamp::from_millis(T0 + 60_000))
    );
    assert_eq!(then.deliveries[1].last_error, None);
}

#[tokio::test]
async fn alert_history_keeps_to_its_window_and_state() {
    let Fixture { _dir, store, .. } = open();
    let (resolved, firing) = two_alerts(&store).await;
    let ids = |entries: Vec<tocsin_store::AlertEntry>| -> Vec<i64> {
        entries.iter().map(|entry| entry.alert.id).collect()
    };

    // An alert that ended at `since` is in the window; one that ended
    // before it is not, while a firing one always is.
    let at_end = store
        .alert_history(history_page(T0 + 1_000, None, 10), None)
        .await
        .unwrap();
    assert_eq!(ids(at_end), [firing, resolved]);
    let after_end = store
        .alert_history(history_page(T0 + 1_001, None, 10), None)
        .await
        .unwrap();
    assert_eq!(ids(after_end), [firing]);

    let only_firing = store
        .alert_history(history_page(T0, None, 10), Some(AlertState::Firing))
        .await
        .unwrap();
    assert_eq!(ids(only_firing), [firing]);
    let only_resolved = store
        .alert_history(history_page(T0, None, 10), Some(AlertState::Resolved))
        .await
        .unwrap();
    assert_eq!(ids(only_resolved), [resolved]);
}

#[tokio::test]
async fn alert_history_pages_by_cursor_and_refuses_a_lost_one() {
    let Fixture { _dir, store, .. } = open();
    let (resolved, firing) = two_alerts(&store).await;

    let first = store
        .alert_history(history_page(T0, None, 1), None)
        .await
        .unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].alert.id, firing);
    let second = store
        .alert_history(history_page(T0, Some(firing), 1), None)
        .await
        .unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].alert.id, resolved);
    let last = store
        .alert_history(history_page(T0, Some(resolved), 1), None)
        .await
        .unwrap();
    assert_eq!(last.len(), 0, "the last page is empty");

    // The cursor only places the page: one whose alert has since left
    // this read's state, as a firing alert does when it resolves between
    // pages, still pages on.
    let resolved_after_firing = store
        .alert_history(
            history_page(T0, Some(firing), 1),
            Some(AlertState::Resolved),
        )
        .await
        .unwrap();
    assert_eq!(resolved_after_firing.len(), 1);
    assert_eq!(resolved_after_firing[0].alert.id, resolved);

    // A cursor naming no alert, or one that ended before the window, is
    // refused rather than read as the last page.
    for (since, before) in [(T0, resolved + firing + 100), (T0 + 1_001, resolved)] {
        let error = store
            .alert_history(history_page(since, Some(before), 1), None)
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Invalid(_)), "{error:?}");
    }
}
