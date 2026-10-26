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
        ..AlertRouting::default()
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

fn new_silence(matchers: Matchers, episode: Option<i64>) -> NewSilence {
    NewSilence {
        matchers,
        episode,
        expires_at: None,
        expires_in: None,
        reason: "deploying the api".to_owned(),
    }
}

fn api() -> Matchers {
    Matchers {
        subject: Some("api".to_owned()),
        ..Matchers::default()
    }
}

/// What an episode silence of `api` on `node-a` matches, as `scope_to`
/// sets it.
fn api_on_node_a() -> Matchers {
    Matchers {
        node: Some("node-a".to_owned()),
        ..api()
    }
}

async fn silence(store: &Store, matchers: Matchers, episode: Option<i64>, expires_at: i64) -> i64 {
    store
        .create_silence(
            new_silence(matchers, episode),
            Timestamp::from_millis(expires_at),
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

async fn hold_resolve(store: &Store, delivery: &PendingDelivery, at: i64) -> Option<i64> {
    store
        .hold_resolve(delivery.id, Timestamp::from_millis(at))
        .await
        .unwrap()
}

/// The ids of the silences that apply at `at`.
async fn active(store: &Store, at: i64) -> Vec<i64> {
    store
        .active_silences(Timestamp::from_millis(at))
        .await
        .unwrap()
        .iter()
        .map(|silence| silence.id)
        .collect()
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
    let offered = store.active_silences(now).await.unwrap();
    assert_eq!(
        offered
            .iter()
            .map(|silence| (
                silence.id,
                &silence.matchers,
                silence.episode,
                silence.expires_at
            ))
            .collect::<Vec<_>>(),
        [(id, &api(), None, Timestamp::from_millis(T0 + HOUR))],
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
    assert_eq!(active(&store, T0 + 1).await, [0_i64; 0]);

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
async fn a_resolve_is_held_only_while_no_send_was_ever_attempted() {
    let (_dir, store) = open();
    let first = open_outage(&store, None, Health::Down, T0, routing(&["pager"], &[])).await;
    let id = silence(&store, api(), None, T0 + 5).await;
    let fire = head(&store, DeliveryKind::Fire).await;
    assert!(hold(&store, &fire, id, T0 + 1).await);
    recover(&store, first, T0 + 10).await;
    let resolve = head(&store, DeliveryKind::Resolve).await;
    assert!(!resolve.alert.attempted);
    assert_eq!(
        hold_resolve(&store, &resolve, T0 + 11).await,
        Some(id),
        "nobody can have heard of the fire, so the resolve answers nothing, \
         under the fire's silence though it has expired"
    );
    assert_eq!(
        hold_resolve(&store, &resolve, T0 + 12).await,
        None,
        "held once"
    );

    // A send attempted, even one never delivered, may have reached the
    // receiver: the resolve goes, silence or not.
    let second = open_outage(
        &store,
        None,
        Health::Down,
        T0 + 20,
        routing(&["pager"], &[]),
    )
    .await;
    let id = silence(&store, api(), None, T0 + HOUR).await;
    let fire = head(&store, DeliveryKind::Fire).await;
    store.attempt_alert(fire.alert.id).await.unwrap();
    store
        .finish_delivery(
            fire.id,
            DeliveryOutcome::Retry {
                at_ms: T0 + 25,
                error: "no answer within 10s".to_owned(),
            },
            T0 + 21,
        )
        .await
        .unwrap();
    let retry = head(&store, DeliveryKind::Fire).await;
    assert!(hold(&store, &retry, id, T0 + 22).await);
    recover(&store, second, T0 + 30).await;
    let resolve = head(&store, DeliveryKind::Resolve).await;
    assert!(resolve.alert.attempted && !resolve.alert.notified);
    assert_eq!(hold_resolve(&store, &resolve, T0 + 31).await, None);
    store
        .finish_delivery(resolve.id, DeliveryOutcome::Delivered, T0 + 32)
        .await
        .unwrap();

    // Nothing held back at all: the resolve goes.
    let third = open_outage(
        &store,
        None,
        Health::Down,
        T0 + 40,
        routing(&["pager"], &[]),
    )
    .await;
    let fire = head(&store, DeliveryKind::Fire).await;
    store
        .finish_delivery(fire.id, DeliveryOutcome::Delivered, T0 + 41)
        .await
        .unwrap();
    recover(&store, third, T0 + 50).await;
    let resolve = head(&store, DeliveryKind::Resolve).await;
    assert!(resolve.alert.notified && resolve.alert.attempted);
    assert_eq!(hold_resolve(&store, &resolve, T0 + 51).await, None);
}

#[tokio::test]
async fn a_cancel_for_an_alert_never_attempted_is_dropped() {
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
    assert!(hold(&store, &fire, id, T0 + 1).await);
    recover(&store, first, T0 + 10).await;
    let cancel = head(&store, DeliveryKind::Cancel).await;
    assert!(store.drop_cancel(cancel.id).await.unwrap(), "nothing rang");
    assert!(
        store
            .outbox()
            .await
            .unwrap()
            .pending
            .iter()
            .all(|delivery| delivery.kind != DeliveryKind::Cancel)
    );

    let second = open_outage(
        &store,
        None,
        Health::Down,
        T0 + 20,
        routing(&["pushover"], &["pushover"]),
    )
    .await;
    let fire = head(&store, DeliveryKind::Fire).await;
    store.attempt_alert(fire.alert.id).await.unwrap();
    store
        .finish_delivery(
            fire.id,
            DeliveryOutcome::Failed {
                error: "HTTP 400: bad request".to_owned(),
            },
            T0 + 21,
        )
        .await
        .unwrap();
    recover(&store, second, T0 + 30).await;
    let cancel = head(&store, DeliveryKind::Cancel).await;
    assert!(
        !store.drop_cancel(cancel.id).await.unwrap(),
        "a send was attempted, so it may be ringing"
    );
    assert_eq!(head(&store, DeliveryKind::Cancel).await.id, cancel.id);
}

#[tokio::test]
async fn an_alert_a_silence_kept_quiet_is_stranded_until_it_fires_again() {
    let (_dir, store) = open();
    open_outage(&store, None, Health::Down, T0, routing(&["pager"], &[])).await;
    assert_eq!(
        store.stranded_alerts().await.unwrap(),
        [],
        "its fire is pending"
    );
    let id = silence(&store, api(), None, T0 + HOUR).await;
    let fire = head(&store, DeliveryKind::Fire).await;
    assert!(hold(&store, &fire, id, T0 + 1).await);
    let stranded = store.stranded_alerts().await.unwrap();
    assert_eq!(
        stranded.iter().map(|alert| alert.id).collect::<Vec<_>>(),
        [fire.alert.id]
    );

    assert!(store.refire(fire.alert.id, T0 + HOUR).await.unwrap());
    assert!(
        !store.refire(fire.alert.id, T0 + HOUR).await.unwrap(),
        "its fire is pending again"
    );
    assert_eq!(store.stranded_alerts().await.unwrap(), []);
    let refire = head(&store, DeliveryKind::Fire).await;
    assert_ne!(refire.id, fire.id);
    assert_eq!(refire.health, Health::Down);

    // Delivered once, an alert is never fired afresh: its repeats carry it.
    store
        .finish_delivery(refire.id, DeliveryOutcome::Delivered, T0 + HOUR + 1)
        .await
        .unwrap();
    assert_eq!(
        store.outbox().await.unwrap().next_repeat_at_ms,
        Some(T0 + 2 * HOUR),
        "repeats count from the fresh fire"
    );
    let id = silence(&store, api(), None, T0 + 3 * HOUR).await;
    store.queue_repeats(T0 + 2 * HOUR).await.unwrap();
    let repeat = head(&store, DeliveryKind::Repeat).await;
    assert!(hold(&store, &repeat, id, T0 + 2 * HOUR).await);
    assert_eq!(store.stranded_alerts().await.unwrap(), []);
    assert!(!store.refire(fire.alert.id, T0 + 3 * HOUR).await.unwrap());
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
    let resolve = head(&store, DeliveryKind::Resolve).await;
    assert!(
        !hold(&store, &resolve, id, T0 + 11).await,
        "a resolve is held only by its own test"
    );
}

#[tokio::test]
async fn a_silence_stops_applying_when_it_expires() {
    let (_dir, store) = open();
    open_outage(&store, None, Health::Down, T0, routing(&["pager"], &[])).await;
    let id = silence(&store, api(), None, T0 + HOUR).await;
    let expiry = Timestamp::from_millis(T0 + HOUR);
    assert_eq!(active(&store, T0 + HOUR).await, [0_i64; 0]);
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
    let id = silence(&store, api_on_node_a(), Some(degraded), T0 + HOUR).await;
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
    assert_eq!(active(&store, T0 + 11).await, [0_i64; 0]);
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
                new_silence(api_on_node_a(), Some(episode)),
                Timestamp::from_millis(T0 + HOUR),
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

#[tokio::test]
async fn an_episode_silence_holds_only_its_own_outage_s_messages() {
    let (_dir, store) = open();
    let api_outage = open_outage(&store, None, Health::Down, T0, routing(&["pager"], &[])).await;
    // An outage of another check, open beside it.
    let db_outage = store
        .change_outage_alerting(
            OutageChange {
                end: None,
                open: Some(NewOutage {
                    node_id: "node-a".into(),
                    subject_kind: SubjectKind::Check,
                    check_id: "db".into(),
                    component_id: None,
                    health: Health::Down,
                    started_at_ms: T0,
                    alerted: true,
                }),
            },
            Some(routing(&["pager"], &[])),
            T0,
        )
        .await
        .unwrap()
        .opened
        .unwrap();
    // Scoped to the db's outage, yet matching api: it holds nothing of
    // api's, whose alert fired for another outage.
    let id = silence(&store, api_on_node_a(), Some(db_outage), T0 + HOUR).await;
    let fire = store
        .outbox()
        .await
        .unwrap()
        .pending
        .into_iter()
        .find(|delivery| delivery.alert.outage_id == Some(api_outage))
        .unwrap();
    assert!(!hold(&store, &fire, id, T0 + 1).await);

    let own = silence(&store, api_on_node_a(), Some(api_outage), T0 + HOUR).await;
    assert!(hold(&store, &fire, own, T0 + 1).await);
}

#[tokio::test]
async fn an_episode_silence_always_names_its_node_and_subject() {
    let (_dir, store) = open();
    let outage = open_outage(&store, None, Health::Down, T0, routing(&["pager"], &[])).await;
    for matchers in [Matchers::default(), api()] {
        let refused = store
            .create_silence(
                new_silence(matchers.clone(), Some(outage)),
                Timestamp::from_millis(T0 + HOUR),
                Actor::token("pager"),
                Timestamp::from_millis(T0),
            )
            .await;
        assert!(refused.is_err(), "{matchers:?}: {refused:?}");
    }
    assert_eq!(
        store
            .silences(None, 10, Timestamp::from_millis(T0))
            .await
            .unwrap(),
        []
    );
}
