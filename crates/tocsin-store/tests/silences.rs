// SPDX-License-Identifier: AGPL-3.0-only

//! Silences: stored with who made them, applying until they expire, are
//! ended, or their episode closes, and recording every message they hold
//! back in the outbox instead of losing it.

use std::time::Duration;

use tocsin_core::Health;
use tocsin_core::incident::{Actor, Timestamp};
use tocsin_core::silence::{HeldKind, Matchers, NewSilence, SilenceState};
use tocsin_core::status::CheckKind;
use tocsin_store::{
    AlertRouting, AlertTarget, DeliveryKind, DeliveryOutcome, DeliveryRetention, Error, NewOutage,
    OutageChange, OutageEnd, PendingDelivery, Store, SubjectKind,
};

const T0: i64 = 1_800_000_000_000;
const HOUR: i64 = 3_600_000;

fn open() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join(tocsin_store::DB_FILE)).unwrap();
    (dir, store)
}

fn routing(notifiers: &[&str], ringing: &[&str]) -> AlertRouting {
    AlertRouting {
        targets: notifiers
            .iter()
            .map(|id| AlertTarget {
                notifier_id: (*id).to_owned(),
                route_id: "ops".to_owned(),
                repeat_ms: Some(HOUR),
            })
            .collect(),
        ringing: ringing.iter().map(|id| (*id).to_owned()).collect(),
    }
}

/// Opens an outage of check `api` on `node-a` at `at` (ending `previous`,
/// if any); resolves with its id.
async fn open_outage(
    store: &Store,
    previous: Option<i64>,
    health: Health,
    at: i64,
    routing: AlertRouting,
) -> i64 {
    let change = OutageChange {
        end: previous.map(|id| OutageEnd {
            id,
            ended_at_ms: at,
        }),
        open: Some(NewOutage {
            node_id: "node-a".into(),
            subject_kind: SubjectKind::Check,
            check_id: "api".into(),
            component_id: Some("web".into()),
            health,
            started_at_ms: at,
            alerted: true,
        }),
    };
    store
        .change_outage_alerting(change, Some(routing), at)
        .await
        .unwrap()
        .opened
        .unwrap()
}

async fn recover(store: &Store, id: i64, at: i64) {
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
        .unwrap();
}

fn new_silence(matchers: Matchers, episode: Option<i64>, expires_at: i64) -> NewSilence {
    NewSilence {
        matchers,
        episode,
        expires_at: Timestamp::from_millis(expires_at),
        reason: "deploying the api".to_owned(),
    }
}

fn api() -> Matchers {
    Matchers {
        subject: Some("api".to_owned()),
        ..Matchers::default()
    }
}

async fn silence(store: &Store, matchers: Matchers, episode: Option<i64>, expires_at: i64) -> i64 {
    store
        .create_silence(
            new_silence(matchers, episode, expires_at),
            Actor::token("pager"),
            Timestamp::from_millis(T0),
        )
        .await
        .unwrap()
        .id
}

async fn head(store: &Store, kind: DeliveryKind) -> PendingDelivery {
    let outbox = store.outbox().await.unwrap();
    outbox
        .pending
        .into_iter()
        .find(|head| head.kind == kind)
        .unwrap_or_else(|| panic!("no pending {kind:?}"))
}

async fn hold(store: &Store, delivery: &PendingDelivery, silence: i64, at: i64) -> bool {
    store
        .hold_delivery(delivery.id, silence, Timestamp::from_millis(at))
        .await
        .unwrap()
}

#[tokio::test]
async fn a_silence_records_who_made_it_and_who_ended_it() {
    let (_dir, store) = open();
    let id = silence(&store, api(), None, T0 + HOUR).await;
    let now = Timestamp::from_millis(T0 + 1);
    let stored = store.silence(id, now).await.unwrap().unwrap();
    assert_eq!(stored.created_by, Actor::token("pager"));
    assert_eq!(stored.created_at, Timestamp::from_millis(T0));
    assert_eq!(stored.expires_at, Timestamp::from_millis(T0 + HOUR));
    assert_eq!(stored.state, SilenceState::Active);
    assert_eq!(stored.matchers, api());
    assert_eq!(
        store.active_silences(now).await.unwrap(),
        [(id, api())],
        "an active silence is offered to the worker"
    );

    let ended = store
        .end_silence(id, Actor::person("ops@example.com"), now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ended.state, SilenceState::Ended);
    assert_eq!(ended.ended_at, Some(now));
    assert_eq!(ended.ended_by, Some(Actor::person("ops@example.com")));
    assert_eq!(store.active_silences(now).await.unwrap(), []);

    let later = Timestamp::from_millis(T0 + 2);
    let again = store
        .end_silence(id, Actor::token("other"), later)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.ended_at, Some(now), "ending twice moves nothing");
    assert!(
        store
            .end_silence(id + 1, Actor::token("x"), later)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_silence_lists_newest_first() {
    let (_dir, store) = open();
    let first = silence(&store, api(), None, T0 + HOUR).await;
    let second = silence(&store, api(), None, T0 + HOUR).await;
    let now = Timestamp::from_millis(T0);
    let ids: Vec<i64> = store
        .silences(None, 10, now)
        .await
        .unwrap()
        .iter()
        .map(|silence| silence.id)
        .collect();
    assert_eq!(ids, [second, first]);
    let page: Vec<i64> = store
        .silences(Some(second), 10, now)
        .await
        .unwrap()
        .iter()
        .map(|silence| silence.id)
        .collect();
    assert_eq!(page, [first]);
}

#[tokio::test]
async fn a_held_message_is_recorded_with_its_silence_not_sent() {
    let (_dir, store) = open();
    open_outage(&store, None, Health::Down, T0, routing(&["pager"], &[])).await;
    let id = silence(&store, api(), None, T0 + HOUR).await;
    let fire = head(&store, DeliveryKind::Fire).await;
    assert!(hold(&store, &fire, id, T0 + 1).await);
    assert!(
        store.outbox().await.unwrap().pending.is_empty(),
        "a held message is finished, not pending"
    );
    assert!(!hold(&store, &fire, id, T0 + 2).await, "held only once");

    let stored = store
        .silence(id, Timestamp::from_millis(T0 + 2))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.held_total, 1);
    let held = &stored.held[0];
    assert_eq!(
        (
            held.delivery_id,
            held.kind,
            held.health,
            held.at,
            held.alert_id,
            held.node.as_str(),
            held.subject_kind,
            held.subject.as_str(),
            held.notifier.as_str(),
        ),
        (
            fire.id,
            HeldKind::Fire,
            Health::Down,
            Timestamp::from_millis(T0 + 1),
            fire.alert.id,
            "node-a",
            CheckKind::Check,
            "api",
            "pager",
        )
    );
    let alert = &store.alerts().await.unwrap()[0];
    assert!(alert.ended_at_ms.is_none(), "the alert still fires");
    assert!(!alert.notified);
}

#[tokio::test]
async fn a_resolve_is_held_only_while_nothing_was_sent() {
    let (_dir, store) = open();
    let first = open_outage(&store, None, Health::Down, T0, routing(&["pager"], &[])).await;
    let id = silence(&store, api(), None, T0 + HOUR).await;
    let fire = head(&store, DeliveryKind::Fire).await;
    assert!(hold(&store, &fire, id, T0 + 1).await);
    recover(&store, first, T0 + 10).await;
    let resolve = head(&store, DeliveryKind::Resolve).await;
    assert!(
        hold(&store, &resolve, id, T0 + 11).await,
        "nobody heard the fire, so the resolve answers nothing"
    );

    let second = open_outage(
        &store,
        None,
        Health::Down,
        T0 + 20,
        routing(&["pager"], &[]),
    )
    .await;
    let fire = head(&store, DeliveryKind::Fire).await;
    store
        .finish_delivery(fire.id, DeliveryOutcome::Delivered, T0 + 21)
        .await
        .unwrap();
    recover(&store, second, T0 + 30).await;
    let resolve = head(&store, DeliveryKind::Resolve).await;
    assert!(resolve.alert.notified);
    assert!(
        !hold(&store, &resolve, id, T0 + 31).await,
        "a resolve for a delivered fire is always sent"
    );
}

#[tokio::test]
async fn a_cancel_is_never_held() {
    let (_dir, store) = open();
    let first = open_outage(
        &store,
        None,
        Health::Down,
        T0,
        routing(&["pushover"], &["pushover"]),
    )
    .await;
    let id = silence(&store, api(), None, T0 + HOUR).await;
    let fire = head(&store, DeliveryKind::Fire).await;
    store
        .finish_delivery(fire.id, DeliveryOutcome::Delivered, T0 + 1)
        .await
        .unwrap();
    recover(&store, first, T0 + 10).await;
    let cancel = head(&store, DeliveryKind::Cancel).await;
    assert!(!hold(&store, &cancel, id, T0 + 11).await);
}

#[tokio::test]
async fn a_silence_stops_applying_when_it_expires() {
    let (_dir, store) = open();
    open_outage(&store, None, Health::Down, T0, routing(&["pager"], &[])).await;
    let id = silence(&store, api(), None, T0 + HOUR).await;
    let expiry = Timestamp::from_millis(T0 + HOUR);
    assert_eq!(store.active_silences(expiry).await.unwrap(), []);
    let fire = head(&store, DeliveryKind::Fire).await;
    assert!(
        !hold(&store, &fire, id, T0 + HOUR).await,
        "an expired silence holds nothing back"
    );
    let stored = store.silence(id, expiry).await.unwrap().unwrap();
    assert_eq!(stored.state, SilenceState::Expired);
}

#[tokio::test]
async fn an_episode_silence_ends_with_its_outage() {
    let (_dir, store) = open();
    let degraded = open_outage(&store, None, Health::Degraded, T0, routing(&["pager"], &[])).await;
    let id = silence(&store, Matchers::default(), Some(degraded), T0 + HOUR).await;
    let fire = head(&store, DeliveryKind::Fire).await;
    assert!(hold(&store, &fire, id, T0 + 1).await);

    // Going down ends the degraded outage and opens another: the episode
    // is over, so the re-fire is sent.
    open_outage(
        &store,
        Some(degraded),
        Health::Down,
        T0 + 10,
        routing(&["pager"], &[]),
    )
    .await;
    let now = Timestamp::from_millis(T0 + 11);
    assert_eq!(store.active_silences(now).await.unwrap(), []);
    let refire = head(&store, DeliveryKind::Fire).await;
    assert_eq!(refire.health, Health::Down);
    assert!(!hold(&store, &refire, id, T0 + 11).await);
    let stored = store.silence(id, now).await.unwrap().unwrap();
    assert_eq!(stored.state, SilenceState::EpisodeEnded);
    assert_eq!(
        stored.episode_ended_at,
        Some(Timestamp::from_millis(T0 + 10))
    );
}

#[tokio::test]
async fn a_closed_or_missing_episode_is_refused() {
    let (_dir, store) = open();
    let outage = open_outage(&store, None, Health::Down, T0, routing(&["pager"], &[])).await;
    recover(&store, outage, T0 + 1).await;
    for episode in [outage, outage + 1] {
        let refused = store
            .create_silence(
                new_silence(Matchers::default(), Some(episode), T0 + HOUR),
                Actor::token("pager"),
                Timestamp::from_millis(T0 + 2),
            )
            .await
            .unwrap_err();
        let Error::Invalid(errors) = refused else {
            panic!("{refused}")
        };
        assert_eq!(errors[0].path, "episode");
    }
}

#[tokio::test]
async fn held_messages_are_pruned_with_failed_ones() {
    let (_dir, store) = open();
    open_outage(&store, None, Health::Down, T0, routing(&["pager"], &[])).await;
    let id = silence(&store, api(), None, T0 + HOUR).await;
    let fire = head(&store, DeliveryKind::Fire).await;
    assert!(hold(&store, &fire, id, T0).await);
    let retention = DeliveryRetention {
        delivered_ms: 10,
        failed_ms: 1_000,
        chunk_rows: 100,
        pause: Duration::ZERO,
    };
    let kept = store.prune_deliveries(retention, T0 + 500).await.unwrap();
    assert_eq!(kept.deliveries, 0, "a held message outlives delivered ones");
    let pruned = store.prune_deliveries(retention, T0 + 2_000).await.unwrap();
    assert_eq!(pruned.deliveries, 1);
    let stored = store
        .silence(id, Timestamp::from_millis(T0 + 2_000))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.held_total, 0);
}
