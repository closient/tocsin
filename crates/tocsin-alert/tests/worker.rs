// SPDX-License-Identifier: AGPL-3.0-only

//! The delivery worker against a real store, on a paused clock: retries
//! and their backoff, a resolve that waits for its fire, repeats, failed
//! deliveries, a notifier that hangs, a restart with a delivery still
//! queued, and silences holding messages back.
//!
//! Tests wait on the fakes' channel, never on a tokio timeout: while the
//! store's writer thread works, a paused clock jumps to the next timer, and
//! a test's own timeout would be that timer. [`paused`] bounds each one by
//! the real clock instead.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{
    Answer, Attempt, Fake, Fixture, T0, config, down, failing, monitored, monitored_by, next_from,
    paused, pushover, turns, up,
};
use tocsin_alert::{
    AlertEvent, AlertMetrics, Alerter, AlertingHistory, Backoff, Jitter, Notifier, Notifiers,
    Router, WorkerSettings,
};
use tocsin_core::Health;
use tocsin_core::config::Config;
use tocsin_core::incident::{Actor, Timestamp};
use tocsin_core::silence::{HeldKind, Matchers, NewSilence, SilenceState};
use tocsin_engine::ports::{Clock, History, TokioClock};
use tocsin_store::{DeliveryKind, EndedBy, NewCheckResult, OutageChange, Store, SubjectKind};
use tokio::sync::mpsc;

const HOUR_MS: i64 = 3_600_000;

/// Retries at exactly 5s, 10s, 20s ...; four attempts. The idle wake and the send
/// timeout are an hour, so a clock that jumps while the store works never
/// crosses them; stopping abandons sends after a tenth of a second.
fn settings() -> WorkerSettings {
    WorkerSettings {
        backoff: Backoff {
            base: Duration::from_secs(5),
            cap: Duration::from_secs(60),
            jitter: Jitter::None,
        },
        max_attempts: 4,
        send_timeout: Duration::from_hours(1),
        idle: Duration::from_hours(1),
        stop_grace: Duration::from_millis(100),
        ..WorkerSettings::default()
    }
}

/// An alerter and the engine's history, wired as `serve` wires them.
struct Running {
    alerter: Alerter,
    history: Arc<AlertingHistory>,
}

impl Running {
    /// Applies an outage change as the engine would, with the result that
    /// confirmed it, and resolves with the opened outage's id. The paused
    /// clock stands still meanwhile: run as a blocking task, the store write
    /// no longer looks idle, so the clock cannot leap to the worker's next
    /// wake before the write lands.
    async fn change(&self, change: OutageChange) -> Option<i64> {
        // A recovery is `api`'s (see `common::up`). When the result started
        // does not matter to alerting; now keeps it inside the store's
        // tolerance for the future.
        let (kind, check_id, health) = change
            .open
            .as_ref()
            .map_or((SubjectKind::Check, "api".to_owned(), Health::Up), |open| {
                (open.subject_kind, open.check_id.clone(), open.health)
            });
        let result = NewCheckResult {
            node_id: "node-a".into(),
            check_id,
            started_at_ms: tocsin_store::now_ms(),
            duration_ms: 1,
            health,
            detail: None,
            error: None,
            status_code: None,
        };
        let history = Arc::clone(&self.history);
        let runtime = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            runtime.block_on(history.record(result, kind, Some(change)))
        })
        .await
        .unwrap()
        .unwrap()
    }

    /// Withdraws outage `id` at `at`, as the engine does once a reload
    /// stops watching its check, with the clock standing still as in
    /// [`Running::change`].
    async fn withdraw(&self, id: i64, at: i64) {
        let history = Arc::clone(&self.history);
        let runtime = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || runtime.block_on(history.withdraw(id, at)))
            .await
            .unwrap()
            .unwrap();
    }
}

fn start(
    fixture: &Fixture,
    config: &Config,
    notifiers: &[(&str, Arc<dyn Notifier>)],
    metrics: &AlertMetrics,
) -> Running {
    start_with(fixture, config, notifiers, metrics, settings())
}

fn start_with(
    fixture: &Fixture,
    config: &Config,
    notifiers: &[(&str, Arc<dyn Notifier>)],
    metrics: &AlertMetrics,
    settings: WorkerSettings,
) -> Running {
    let notifiers: Notifiers = notifiers
        .iter()
        .map(|(id, notifier)| ((*id).to_owned(), Arc::clone(notifier)))
        .collect();
    let router = Arc::new(Router::new(config));
    let clock: Arc<dyn Clock> = Arc::new(TokioClock::starting_at(T0));
    let alerter = Alerter::start(
        Arc::clone(&fixture.store),
        notifiers,
        Arc::clone(&router),
        Arc::clone(&clock),
        metrics.clone(),
        settings,
    );
    let history = Arc::new(AlertingHistory::new(
        Arc::clone(&fixture.store),
        router,
        alerter.waker(),
        clock,
    ));
    Running { alerter, history }
}

fn channel() -> (
    mpsc::UnboundedSender<Attempt>,
    mpsc::UnboundedReceiver<Attempt>,
) {
    mpsc::unbounded_channel()
}

#[test]
fn a_resolve_carries_its_fire_s_identity_and_the_outage_s_duration() {
    paused(async {
        let fixture = Fixture::new();
        let config = config(&monitored(&["slack"], ""));
        let (sender, mut receiver) = channel();
        let slack: Arc<dyn Notifier> = Fake::accepting("slack", &sender);
        let running = start(
            &fixture,
            &config,
            &[("slack", slack)],
            &AlertMetrics::default(),
        );

        let id = running.change(down(T0)).await.unwrap();
        let fire = next_from(&mut receiver, "slack").await.alert;
        running.change(up(id, T0 + 90_000)).await;
        let resolve = next_from(&mut receiver, "slack").await.alert;

        assert_eq!(fire.event, AlertEvent::Fire);
        assert_eq!(resolve.event, AlertEvent::Resolve);
        assert_eq!(fire.identity(), "node-a/check/api");
        assert_eq!(resolve.identity(), fire.identity());
        assert_eq!(resolve.started_at_ms, fire.started_at_ms);
        assert_eq!(resolve.duration_ms(), Some(90_000));
        assert_eq!(fire.subject_name, "Public API");
        assert_eq!(fire.component_name.as_deref(), Some("Website"));
        running.alerter.stop().await;
    });
}

/// `monitored`'s check, given `labels` and `annotations`, repeating hourly.
fn labelled(labels: &str, annotations: &str) -> Config {
    config(&monitored(&["slack"], "repeat = \"1h\"").replace(
        "component = \"web\"\n\n[[",
        &format!("component = \"web\"\nlabels = {labels}\nannotations = {annotations}\n\n[["),
    ))
}

/// `toml`, a [`monitored`] configuration, without its check.
fn without_the_check(toml: &str) -> String {
    let start = toml.find("[[check]]").unwrap();
    let end = start + toml[start..].find("\n\n").unwrap() + 2;
    let mut toml = toml.to_owned();
    toml.replace_range(start..end, "");
    toml
}

/// `(name, value)` pairs.
type Pairs = Vec<(String, String)>;

/// The labels and annotations an attempt carried.
fn carried(attempt: &Attempt) -> (Pairs, Pairs) {
    let pairs = |map: &std::collections::BTreeMap<String, String>| {
        map.iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect()
    };
    (
        pairs(&attempt.alert.labels),
        pairs(&attempt.alert.annotations),
    )
}

#[test]
fn an_alert_keeps_the_labels_it_opened_with_through_a_reload_until_it_resolves() {
    paused(async {
        let fixture = Fixture::new();
        let (sender, mut receiver) = channel();
        let slack: Arc<dyn Notifier> = Fake::accepting("slack", &sender);
        let running = start(
            &fixture,
            &labelled(r#"{ page_class = "page" }"#, r#"{ ticket = "OPS-1" }"#),
            &[("slack", Arc::clone(&slack))],
            &AlertMetrics::default(),
        );
        let opened = (
            vec![("page_class".to_owned(), "page".to_owned())],
            vec![("ticket".to_owned(), "OPS-1".to_owned())],
        );

        let id = running.change(down(T0)).await.unwrap();
        let fire = next_from(&mut receiver, "slack").await;
        assert_eq!(fire.event(), AlertEvent::Fire);
        assert_eq!(carried(&fire), opened);

        // A reload changes the check's labels while its alert fires: the
        // alert keeps the identity Alertmanager already knows it by.
        reload(
            &fixture,
            &running,
            &labelled(r#"{ page_class = "ticket" }"#, r#"{ ticket = "OPS-2" }"#),
            &[("slack", Arc::clone(&slack))],
        )
        .await;
        let repeat = next_from(&mut receiver, "slack").await;
        assert_eq!(repeat.event(), AlertEvent::Repeat);
        assert_eq!(repeat.alert.identity(), fire.alert.identity());
        assert_eq!(carried(&repeat), opened);

        running.change(up(id, T0 + HOUR_MS + 1_000)).await;
        let resolve = next_from(&mut receiver, "slack").await;
        assert_eq!(resolve.event(), AlertEvent::Resolve);
        assert_eq!(carried(&resolve), opened);

        // The change takes effect from the next episode.
        running.change(down(T0 + 2 * HOUR_MS)).await;
        let next = next_from(&mut receiver, "slack").await;
        assert_eq!(next.event(), AlertEvent::Fire);
        assert_eq!(
            carried(&next),
            (
                vec![("page_class".to_owned(), "ticket".to_owned())],
                vec![("ticket".to_owned(), "OPS-2".to_owned())],
            )
        );
        running.alerter.stop().await;
    });
}

#[test]
fn an_alert_that_re_fires_after_a_reload_keeps_the_labels_it_opened_with() {
    paused(async {
        let fixture = Fixture::new();
        let (sender, mut receiver) = channel();
        let slack: Arc<dyn Notifier> = Fake::accepting("slack", &sender);
        let running = start(
            &fixture,
            &labelled(r#"{ page_class = "page" }"#, r#"{ ticket = "OPS-1" }"#),
            &[("slack", Arc::clone(&slack))],
            &AlertMetrics::default(),
        );
        let opened = (
            vec![("page_class".to_owned(), "page".to_owned())],
            vec![("ticket".to_owned(), "OPS-1".to_owned())],
        );

        let id = running.change(down(T0)).await.unwrap();
        let fire = next_from(&mut receiver, "slack").await;
        assert_eq!(carried(&fire), opened);
        reload(
            &fixture,
            &running,
            &labelled(r#"{ page_class = "ticket" }"#, r#"{ ticket = "OPS-2" }"#),
            &[("slack", Arc::clone(&slack))],
        )
        .await;

        // The health changes, so the open alert fires again rather than
        // opening a second one.
        running
            .change(turns(id, Health::Degraded, T0 + 1_000))
            .await
            .unwrap();
        let again = next_from(&mut receiver, "slack").await;
        assert_eq!(again.event(), AlertEvent::Fire);
        assert_eq!(again.alert.identity(), fire.alert.identity());
        assert_eq!(carried(&again), opened);

        let stored = fixture.store.alerts().await.unwrap();
        assert_eq!(stored.len(), 1, "{stored:?}");
        let pairs = |map: &std::collections::BTreeMap<String, String>| -> Pairs {
            map.iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect()
        };
        assert_eq!(
            (pairs(&stored[0].labels), pairs(&stored[0].annotations)),
            opened
        );
        running.alerter.stop().await;
    });
}

/// The resolve of an outage that recovered just before a reload removed
/// its check, still queued when the reload landed, goes out after it with
/// the labels it fired with.
#[test]
fn a_resolve_queued_before_a_reload_removes_its_check_keeps_its_labels() {
    paused(async {
        let fixture = Fixture::new();
        let (sender, mut receiver) = channel();
        // The resolve's first attempt fails, so it is still queued when the
        // reload lands and its retry goes out after.
        let slack: Arc<dyn Notifier> = Fake::new("slack", &sender, |alert, attempt| {
            if alert.event == AlertEvent::Resolve && attempt == 1 {
                Answer::Fail
            } else {
                Answer::Accept
            }
        });
        let running = start(
            &fixture,
            &labelled(r#"{ page_class = "page" }"#, "{}"),
            &[("slack", Arc::clone(&slack))],
            &AlertMetrics::default(),
        );
        let id = running.change(down(T0)).await.unwrap();
        let fire = next_from(&mut receiver, "slack").await;
        running.change(up(id, T0 + 1_000)).await;
        let failed = next_from(&mut receiver, "slack").await;
        assert_eq!(
            (failed.event(), failed.answer),
            (AlertEvent::Resolve, Answer::Fail)
        );

        // The reload removes the check: the router knows nothing of it.
        reload(
            &fixture,
            &running,
            &config(&without_the_check(&monitored(&["slack"], ""))),
            &[("slack", Arc::clone(&slack))],
        )
        .await;
        let resolve = next_from(&mut receiver, "slack").await;
        assert_eq!(
            (resolve.event(), resolve.answer),
            (AlertEvent::Resolve, Answer::Accept)
        );
        assert_eq!(carried(&resolve), carried(&fire));
        assert_eq!(resolve.alert.labels.len(), 1);
        running.alerter.stop().await;
    });
}

#[test]
fn failures_back_off_and_the_resolve_waits_for_its_fire() {
    paused(async {
        let fixture = Fixture::new();
        let config = config(&monitored(&["slack"], ""));
        let (sender, mut receiver) = channel();
        // The fire fails three times; everything after is accepted.
        let slack: Arc<dyn Notifier> = Fake::new("slack", &sender, |_, attempt| {
            if attempt < 3 {
                Answer::Fail
            } else {
                Answer::Accept
            }
        });
        let metrics = AlertMetrics::default();
        let running = start(&fixture, &config, &[("slack", slack)], &metrics);

        let id = running.change(down(T0)).await.unwrap();
        let first = next_from(&mut receiver, "slack").await;
        // The recovery comes while the fire is still failing.
        running.change(up(id, T0 + 1_000)).await;
        let mut attempts = vec![first];
        while attempts.last().unwrap().event() != AlertEvent::Resolve {
            attempts.push(next_from(&mut receiver, "slack").await);
        }

        let seen: Vec<(AlertEvent, i64, Answer)> = attempts
            .iter()
            .map(|attempt| {
                (
                    attempt.event(),
                    attempt.alert.sent_at_ms - T0,
                    attempt.answer,
                )
            })
            .collect();
        assert_eq!(
            seen,
            [
                (AlertEvent::Fire, 0, Answer::Fail),
                (AlertEvent::Fire, 5_000, Answer::Fail),
                (AlertEvent::Fire, 15_000, Answer::Fail),
                (AlertEvent::Fire, 35_000, Answer::Accept),
                (AlertEvent::Resolve, 35_000, Answer::Accept),
            ]
        );
        assert_eq!(
            (
                metrics.attempt_failures("slack"),
                metrics.abandonments("slack")
            ),
            (3, 0)
        );
        running.alerter.stop().await;
        assert_eq!(fixture.store.failed_deliveries(10).await.unwrap(), []);
    });
}

/// Every failed delivery as (notifier, kind, attempts, error), sorted.
async fn failed_deliveries(fixture: &Fixture) -> Vec<(String, &'static str, u32, Option<String>)> {
    let mut failed: Vec<_> = fixture
        .store
        .failed_deliveries(10)
        .await
        .unwrap()
        .into_iter()
        .map(|failed| {
            (
                failed.alert.notifier_id,
                failed.kind.as_str(),
                failed.attempts,
                failed.error,
            )
        })
        .collect();
    failed.sort();
    failed
}

#[test]
fn a_delivery_that_keeps_failing_is_recorded_and_counted() {
    paused(async {
        let fixture = Fixture::new();
        let config = config(&monitored(&["flaky", "strict", "gone"], ""));
        let (sender, mut receiver) = channel();
        let flaky: Arc<dyn Notifier> = Fake::new("flaky", &sender, |alert, _| {
            if alert.event == AlertEvent::Fire {
                Answer::Fail
            } else {
                Answer::Accept
            }
        });
        let strict: Arc<dyn Notifier> = Fake::new("strict", &sender, |_, _| Answer::Refuse);
        // `gone` is routed to but has no notifier, as after a bad reload.
        let metrics = AlertMetrics::default();
        let running = start(
            &fixture,
            &config,
            &[("flaky", flaky), ("strict", strict)],
            &metrics,
        );

        let id = running.change(down(T0)).await.unwrap();
        running.change(up(id, T0 + 1_000)).await;
        // Four failed fires, then the resolve: a fire that failed for good no
        // longer holds its resolve back.
        let mut flaky_seen = Vec::new();
        loop {
            let attempt = next_from(&mut receiver, "flaky").await;
            flaky_seen.push((attempt.event(), attempt.alert.sent_at_ms - T0));
            if attempt.event() == AlertEvent::Resolve {
                break;
            }
        }
        assert_eq!(
            flaky_seen,
            [
                (AlertEvent::Fire, 0),
                (AlertEvent::Fire, 5_000),
                (AlertEvent::Fire, 15_000),
                (AlertEvent::Fire, 35_000),
                (AlertEvent::Resolve, 35_000),
            ]
        );
        running.alerter.stop().await;

        assert_eq!(
            failed_deliveries(&fixture).await,
            [
                (
                    "flaky".into(),
                    "fire",
                    4,
                    Some("HTTP 503: try later".into())
                ),
                (
                    "gone".into(),
                    "fire",
                    1,
                    Some("notifier `gone` is no longer configured".into())
                ),
                (
                    "gone".into(),
                    "resolve",
                    1,
                    Some("notifier `gone` is no longer configured".into())
                ),
                (
                    "strict".into(),
                    "fire",
                    1,
                    Some("HTTP 400: bad request".into())
                ),
                (
                    "strict".into(),
                    "resolve",
                    1,
                    Some("HTTP 400: bad request".into())
                ),
            ]
        );
        // Every failed attempt counts; only the messages given up on count
        // as abandoned: flaky's fire after four tries, and each of the
        // others' fire and resolve at the first.
        let counted: Vec<_> = ["flaky", "strict", "gone"]
            .into_iter()
            .map(|notifier| {
                (
                    notifier,
                    metrics.attempt_failures(notifier),
                    metrics.abandonments(notifier),
                )
            })
            .collect();
        assert_eq!(counted, [("flaky", 4, 1), ("strict", 2, 2), ("gone", 2, 2)]);
        // Two alerts ended unheard: strict refused its resolve, and gone
        // could not be sent one.
        assert_eq!(metrics.alerts_abandoned(), 2);
    });
}

#[test]
fn a_firing_alert_repeats_on_its_route_s_interval_until_it_resolves() {
    paused(async {
        let fixture = Fixture::new();
        let config = config(&monitored(&["slack"], "repeat = \"1h\""));
        let (sender, mut receiver) = channel();
        let slack = Fake::accepting("slack", &sender);
        let running = start(
            &fixture,
            &config,
            &[("slack", Arc::clone(&slack) as Arc<dyn Notifier>)],
            &AlertMetrics::default(),
        );

        let id = running.change(down(T0)).await.unwrap();
        let mut seen = Vec::new();
        for _ in 0..3 {
            let attempt = next_from(&mut receiver, "slack").await;
            seen.push((attempt.event(), attempt.alert.sent_at_ms - T0));
        }
        running.change(up(id, T0 + 2 * HOUR_MS + 1_000)).await;
        let resolve = next_from(&mut receiver, "slack").await;
        seen.push((resolve.event(), resolve.alert.sent_at_ms - T0));
        // A day later, nothing more has been sent.
        tokio::time::sleep(Duration::from_hours(24)).await;
        running.alerter.stop().await;

        assert_eq!(
            seen,
            [
                (AlertEvent::Fire, 0),
                (AlertEvent::Repeat, HOUR_MS),
                (AlertEvent::Repeat, 2 * HOUR_MS),
                (AlertEvent::Resolve, 2 * HOUR_MS),
            ]
        );
        assert_eq!(slack.seen().len(), 4, "{:?}", slack.seen());
        assert!(receiver.try_recv().is_err());
    });
}

// Real time: on a paused clock, every store write would let the clock jump
// to the stuck send's timeout, and Slack would look held up when it is not.
#[tokio::test]
async fn a_hanging_notifier_holds_up_only_its_own_alerts() {
    let fixture = Fixture::new();
    let config = config(&monitored(&["stuck", "slack"], ""));
    let (sender, mut receiver) = channel();
    let stuck: Arc<dyn Notifier> = Fake::new("stuck", &sender, |_, _| Answer::Hang);
    let slack: Arc<dyn Notifier> = Fake::accepting("slack", &sender);
    let running = start(
        &fixture,
        &config,
        &[("stuck", stuck), ("slack", slack)],
        &AlertMetrics::default(),
    );

    let id = running.change(down(T0)).await.unwrap();
    running.change(up(id, T0 + 1_000)).await;
    // The stuck send is in flight under its hour-long send timeout; Slack
    // answers at once, so ten seconds only bounds a broken run.
    let slack_seen = tokio::time::timeout(Duration::from_secs(10), async {
        let fire = next_from(&mut receiver, "slack").await;
        let resolve = next_from(&mut receiver, "slack").await;
        (fire.event(), resolve.event())
    })
    .await
    .expect("Slack is held up by the stuck notifier");
    assert_eq!(slack_seen, (AlertEvent::Fire, AlertEvent::Resolve));
    running.alerter.stop().await;

    // Abandoned at stop, the stuck fire is still queued, untried.
    let outbox = fixture.store.outbox().await.unwrap();
    let pending: Vec<_> = outbox
        .pending
        .iter()
        .map(|delivery| {
            (
                delivery.alert.notifier_id.as_str(),
                delivery.kind,
                delivery.attempts,
            )
        })
        .collect();
    assert_eq!(pending, [("stuck", DeliveryKind::Fire, 0)]);
}

#[test]
fn a_delivery_pending_at_shutdown_is_sent_after_the_restart() {
    paused(async {
        let fixture = Fixture::new();
        let config = config(&monitored(&["slack"], ""));
        let (sender, mut receiver) = channel();
        let hanging: Arc<dyn Notifier> = Fake::new("slack", &sender, |_, _| Answer::Hang);
        let running = start(
            &fixture,
            &config,
            &[("slack", hanging)],
            &AlertMetrics::default(),
        );

        let id = running.change(down(T0)).await.unwrap();
        let abandoned = next_from(&mut receiver, "slack").await.alert;
        let Running { alerter, history } = running;
        alerter.stop().await;
        drop(history);
        let fixture = fixture.restart().await;

        let slack: Arc<dyn Notifier> = Fake::accepting("slack", &sender);
        let running = start(
            &fixture,
            &config,
            &[("slack", slack)],
            &AlertMetrics::default(),
        );
        let fire = next_from(&mut receiver, "slack").await.alert;
        assert_eq!(fire.event, AlertEvent::Fire);
        assert_eq!(fire.delivery_id, abandoned.delivery_id);
        assert_eq!(fire.identity(), abandoned.identity());

        running.change(up(id, T0 + 1_000)).await;
        let resolve = next_from(&mut receiver, "slack").await.alert;
        assert_eq!(resolve.event, AlertEvent::Resolve);
        assert_eq!(resolve.identity(), fire.identity());
        running.alerter.stop().await;
    });
}

/// Swaps the alerter's notifiers and the history's routes for `config`'s,
/// and ends the alerts of notifiers it no longer declares, waking the
/// worker to send their resolves, as a reload does.
async fn reload(
    fixture: &Fixture,
    running: &Running,
    config: &Config,
    notifiers: &[(&str, Arc<dyn Notifier>)],
) -> usize {
    let router = Arc::new(Router::new(config));
    let notifiers: Notifiers = notifiers
        .iter()
        .map(|(id, notifier)| ((*id).to_owned(), Arc::clone(notifier)))
        .collect();
    let ids = notifiers.keys().cloned().collect();
    running.alerter.reconfigure(notifiers, Arc::clone(&router));
    running.history.set_router(router);
    let ended = fixture
        .store
        .end_alerts_without_notifier(ids, tocsin_store::now_ms())
        .await
        .unwrap();
    if ended > 0 {
        running.alerter.waker().notify_one();
    }
    ended
}

#[test]
fn a_reload_sends_new_outages_on_the_new_routes_and_open_alerts_on_their_own() {
    paused(async {
        let fixture = Fixture::new();
        let (sender, mut receiver) = channel();
        let old = Fake::accepting("old", &sender);
        let kept: Arc<dyn Notifier> = Fake::accepting("kept", &sender);
        let metrics = AlertMetrics::default();
        let running = start(
            &fixture,
            &config(&monitored(&["old", "kept"], "")),
            &[("old", old.clone()), ("kept", Arc::clone(&kept))],
            &metrics,
        );
        let id = running.change(down(T0)).await.unwrap();
        let fire = next_from(&mut receiver, "old").await;
        next_from(&mut receiver, "kept").await;

        let new: Arc<dyn Notifier> = Fake::accepting("new", &sender);
        let ended = reload(
            &fixture,
            &running,
            &config(&monitored(&["kept", "new"], "")),
            &[("kept", kept), ("new", new)],
        )
        .await;
        assert_eq!(ended, 1, "only the removed notifier's alert ends");
        // The removed notifier is kept for one last send: the resolve of
        // the alert it fired, saying the configuration ended it.
        let ended = next_from(&mut receiver, "old").await;
        assert_eq!(
            (ended.event(), ended.cancel, ended.answer),
            (AlertEvent::Resolve, false, Answer::Accept)
        );
        assert_eq!(ended.alert.identity(), fire.alert.identity());
        assert_eq!(ended.alert.ended_by, Some(EndedBy::Config));

        // The open outage recovers: its alert on `kept` resolves, the
        // removed notifier, whose alert already ended, hears nothing more,
        // and `new`, which never fired for it, hears nothing.
        running.change(up(id, T0 + 1_000)).await;
        let resolve = next_from(&mut receiver, "kept").await;
        assert_eq!(resolve.event(), AlertEvent::Resolve);

        // A new outage fires on the new routes.
        running.change(down(T0 + 2_000)).await;
        let mut fires = Vec::new();
        for _ in 0..2 {
            let attempt = receiver.recv().await.unwrap();
            fires.push((attempt.notifier.clone(), attempt.event()));
        }
        fires.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            fires,
            [
                ("kept".to_owned(), AlertEvent::Fire),
                ("new".to_owned(), AlertEvent::Fire)
            ]
        );

        running.alerter.stop().await;
        let old_seen: Vec<AlertEvent> = old.seen().iter().map(Attempt::event).collect();
        assert_eq!(old_seen, [AlertEvent::Fire, AlertEvent::Resolve]);
        assert_eq!(metrics.alerts_abandoned(), 0);
        // Nothing for the removed notifier is left pending, or failed as no
        // longer configured.
        let outbox = fixture.store.outbox().await.unwrap();
        assert!(
            outbox
                .pending
                .iter()
                .all(|delivery| delivery.alert.notifier_id != "old"),
            "{outbox:?}"
        );
        assert_eq!(failed_deliveries(&fixture).await, []);
    });
}

/// A reload that stops watching a firing check: the engine withdraws its
/// outage, and every notifier that heard the fire hears it end, with the
/// fire's identity, labels and annotations from the stored alert (the
/// router no longer knows the check), marked as ended by the configuration.
#[test]
fn a_check_a_reload_removes_resolves_everywhere_it_fired_as_ended_by_the_configuration() {
    paused(async {
        let fixture = Fixture::new();
        let (sender, mut receiver) = channel();
        let slack: Arc<dyn Notifier> = Fake::accepting("slack", &sender);
        let hook: Arc<dyn Notifier> = Fake::accepting("hook", &sender);
        let notifiers = [("slack", Arc::clone(&slack)), ("hook", Arc::clone(&hook))];
        let toml = monitored(&["slack", "hook"], "").replace(
            "component = \"web\"\n\n[[",
            "component = \"web\"\nlabels = { page_class = \"page\" }\n\
             annotations = { runbook = \"https://runbooks.example/api\" }\n\n[[",
        );
        let metrics = AlertMetrics::default();
        let running = start(&fixture, &config(&toml), &notifiers, &metrics);
        let id = running.change(down(T0)).await.unwrap();
        let fires = [
            next_from(&mut receiver, "slack").await,
            next_from(&mut receiver, "hook").await,
        ];

        let ended = reload(
            &fixture,
            &running,
            &config(&without_the_check(&toml)),
            &notifiers,
        )
        .await;
        assert_eq!(ended, 0, "both notifiers are still configured");
        running.withdraw(id, T0 + 90_000).await;

        for fire in fires {
            let resolve = next_from(&mut receiver, &fire.notifier).await;
            assert_eq!(fire.event(), AlertEvent::Fire);
            assert_eq!(
                (resolve.event(), resolve.cancel, resolve.answer),
                (AlertEvent::Resolve, false, Answer::Accept),
                "{}",
                fire.notifier
            );
            assert_eq!(resolve.alert.identity(), fire.alert.identity());
            assert_eq!(resolve.alert.started_at_ms, fire.alert.started_at_ms);
            assert_eq!(resolve.alert.duration_ms(), Some(90_000));
            assert_eq!(resolve.alert.component_id, fire.alert.component_id);
            assert_eq!(carried(&resolve), carried(&fire));
            assert_eq!(resolve.alert.labels.len(), 1);
            assert_eq!(resolve.alert.ended_by, Some(EndedBy::Config));
            let mut annotations = fire.alert.annotations();
            assert_eq!(annotations.len(), 1);
            annotations.insert("ended_by".into(), "config".into());
            assert_eq!(resolve.alert.annotations(), annotations);
        }
        running.alerter.stop().await;
        assert_eq!(failed_deliveries(&fixture).await, []);
        assert_eq!(metrics.alerts_abandoned(), 0);
        assert_eq!(fixture.store.outbox().await.unwrap().pending.len(), 0);
    });
}

/// A notifier removed while tocsin was stopped is gone for good: its open
/// alert ends at startup, and its resolve, which nothing can send, fails,
/// counted as an alert abandoned rather than dropped without a trace.
#[test]
fn a_notifier_removed_across_a_restart_abandons_its_alert_and_counts_it() {
    paused(async {
        let fixture = Fixture::new();
        let (sender, mut receiver) = channel();
        let old = Fake::accepting("old", &sender);
        let kept: Arc<dyn Notifier> = Fake::accepting("kept", &sender);
        let running = start(
            &fixture,
            &config(&monitored(&["old", "kept"], "")),
            &[("old", old.clone()), ("kept", Arc::clone(&kept))],
            &AlertMetrics::default(),
        );
        let id = running.change(down(T0)).await.unwrap();
        next_from(&mut receiver, "old").await;
        next_from(&mut receiver, "kept").await;
        // A notifier has the fire before the worker records it sent; stop
        // before that and the fire stays queued and goes again after the
        // restart. Wait until both are recorded.
        while !fixture.store.outbox().await.unwrap().pending.is_empty() {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let Running { alerter, history } = running;
        alerter.stop().await;
        drop(history);
        let fixture = fixture.restart().await;

        // Started without `old`, as `serve` does once the configuration
        // drops it.
        let metrics = AlertMetrics::default();
        let running = start(
            &fixture,
            &config(&monitored(&["kept"], "")),
            &[("kept", Arc::clone(&kept))],
            &metrics,
        );
        let ended = fixture
            .store
            .end_alerts_without_notifier(vec!["kept".into()], tocsin_store::now_ms())
            .await
            .unwrap();
        assert_eq!(ended, 1);
        running.alerter.waker().notify_one();
        // The outage recovers; once `kept` hears it, the worker has been
        // through `old`'s resolve too, which was queued first.
        running.change(up(id, T0 + 1_000)).await;
        let resolve = next_from(&mut receiver, "kept").await;
        assert_eq!(resolve.event(), AlertEvent::Resolve);
        running.alerter.stop().await;

        let old_seen: Vec<AlertEvent> = old.seen().iter().map(Attempt::event).collect();
        assert_eq!(old_seen, [AlertEvent::Fire]);
        assert_eq!(
            failed_deliveries(&fixture).await,
            [(
                "old".into(),
                "resolve",
                1,
                Some("notifier `old` is no longer configured".into())
            )]
        );
        assert_eq!(metrics.abandonments("old"), 1);
        assert_eq!(metrics.alerts_abandoned(), 1);
    });
}

/// Whether an emergency is still ringing after `attempts`, in order: a
/// fire or repeat while down rings (the `pager` fixture's priority 2), and
/// a cancel stops it.
fn ringing_after(attempts: &[Attempt]) -> bool {
    attempts.iter().fold(false, |ringing, attempt| {
        if attempt.cancel {
            false
        } else if attempt.event() == AlertEvent::Resolve {
            ringing
        } else {
            attempt.alert.health == Health::Down
        }
    })
}

#[test]
fn an_emergency_is_cancelled_when_its_alert_drops_below_emergency() {
    paused(async {
        let fixture = Fixture::new();
        let config = config(&monitored_by(&pushover("pager"), &["pager"], ""));
        let (sender, mut receiver) = channel();
        let pager = Fake::accepting("pager", &sender);
        let running = start(
            &fixture,
            &config,
            &[("pager", Arc::clone(&pager) as Arc<dyn Notifier>)],
            &AlertMetrics::default(),
        );

        let down = running.change(down(T0)).await.unwrap();
        let fire = next_from(&mut receiver, "pager").await;
        assert_eq!((fire.event(), fire.cancel), (AlertEvent::Fire, false));
        assert!(ringing_after(&pager.seen()));

        // Down (priority 2) to degraded (priority 0): the emergency is
        // cancelled now, beside the re-fire, not left ringing until the
        // resolve.
        let degraded = running
            .change(turns(down, Health::Degraded, T0 + 60_000))
            .await
            .unwrap();
        let mut downgrade = [
            next_from(&mut receiver, "pager").await,
            next_from(&mut receiver, "pager").await,
        ];
        downgrade.sort_by_key(|attempt| attempt.cancel);
        let [refire, cancel] = downgrade;
        assert_eq!(
            (refire.event(), refire.alert.health, refire.cancel),
            (AlertEvent::Fire, Health::Degraded, false)
        );
        assert!(cancel.cancel);
        // The cancel names the message that rang.
        assert_eq!(cancel.alert.health, Health::Down);
        assert_eq!(cancel.alert.identity(), fire.alert.identity());
        assert!(!ringing_after(&pager.seen()));

        running.change(up(degraded, T0 + 120_000)).await;
        let resolve = next_from(&mut receiver, "pager").await;
        assert_eq!(
            (resolve.event(), resolve.cancel),
            (AlertEvent::Resolve, false)
        );
        tokio::time::sleep(Duration::from_hours(1)).await;
        running.alerter.stop().await;

        // One cancel, at the downgrade; nothing was ringing at the resolve
        // and nothing rings now.
        let seen = pager.seen();
        assert_eq!(seen.iter().filter(|attempt| attempt.cancel).count(), 1);
        assert!(!ringing_after(&seen));
        assert!(receiver.try_recv().is_err());
    });
}

#[test]
fn a_failing_cancel_never_holds_up_the_resolve() {
    paused(async {
        let fixture = Fixture::new();
        let config = config(&monitored_by(&pushover("pager"), &["pager"], ""));
        let (sender, mut receiver) = channel();
        // The fire is the first attempt; every later fire-shaped attempt
        // is the cancel, and it always fails.
        let pager = Fake::new("pager", &sender, |alert, index| {
            if index > 0 && alert.event == AlertEvent::Fire {
                Answer::Fail
            } else {
                Answer::Accept
            }
        });
        let metrics = AlertMetrics::default();
        let running = start(
            &fixture,
            &config,
            &[("pager", Arc::clone(&pager) as Arc<dyn Notifier>)],
            &metrics,
        );

        let id = running.change(down(T0)).await.unwrap();
        next_from(&mut receiver, "pager").await;
        running.change(up(id, T0 + 60_000)).await;
        let mut resolve = None;
        let mut cancels = Vec::new();
        while resolve.is_none() || cancels.len() < 4 {
            let attempt = next_from(&mut receiver, "pager").await;
            if attempt.cancel {
                cancels.push(attempt.alert.sent_at_ms - T0);
            } else {
                resolve = Some(attempt);
            }
        }
        running.alerter.stop().await;

        let resolve = resolve.unwrap();
        assert_eq!(resolve.event(), AlertEvent::Resolve);
        // Sent at once, beside the first cancel, not after the cancel gave
        // up 35 seconds later.
        assert_eq!(resolve.alert.sent_at_ms - T0, 0);
        // The cancel retries on its own backoff and then fails for good.
        assert_eq!(cancels, [0, 5_000, 15_000, 35_000]);
        assert_eq!(metrics.attempt_failures("pager"), 4);
        assert_eq!(metrics.abandonments("pager"), 1);
    });
}

#[test]
fn a_storm_of_alerts_queues_behind_the_notifier_s_cap_and_its_retry_after() {
    paused(async {
        const ALERTS: usize = 100;
        const LATENCY_MS: i64 = 1_000;
        const RETRY_AFTER_MS: i64 = 30_000;
        let fixture = Fixture::new();
        // The default cap: two sends at once.
        let config = config(&monitored(&["slack"], ""));
        let (sender, mut receiver) = channel();
        // Answers in a second; the first three attempts get a 429 asking
        // for thirty seconds' peace.
        let slack = Fake::slow(
            "slack",
            &sender,
            Duration::from_millis(LATENCY_MS.unsigned_abs()),
            |_, index| {
                if index < 3 {
                    Answer::Busy(Duration::from_millis(RETRY_AFTER_MS.unsigned_abs()))
                } else {
                    Answer::Accept
                }
            },
        );
        let metrics = AlertMetrics::default();
        let running = start(
            &fixture,
            &config,
            &[("slack", Arc::clone(&slack) as Arc<dyn Notifier>)],
            &metrics,
        );

        for n in 0..ALERTS {
            running
                .change(failing(&format!("api-{n}"), Health::Down, T0))
                .await
                .unwrap();
        }
        let mut delivered = std::collections::HashSet::new();
        while delivered.len() < ALERTS {
            let attempt = next_from(&mut receiver, "slack").await;
            if attempt.answer == Answer::Accept {
                assert!(delivered.insert(attempt.alert.subject_id.clone()));
            }
        }
        running.alerter.stop().await;

        let seen = slack.seen();
        assert_eq!(slack.peak(), 2, "the cap holds, and is reached");
        assert_eq!(metrics.attempt_failures("slack"), 3);
        assert_eq!(metrics.abandonments("slack"), 0);
        // Once a 429 is answered, nothing more goes to the notifier until
        // its Retry-After has passed.
        for busy in seen
            .iter()
            .filter(|attempt| attempt.answer != Answer::Accept)
        {
            let answered = busy.alert.sent_at_ms + LATENCY_MS;
            let early: Vec<i64> = seen
                .iter()
                .map(|attempt| attempt.alert.sent_at_ms)
                .filter(|&at| at > answered && at < answered + RETRY_AFTER_MS)
                .map(|at| at - T0)
                .collect();
            assert!(
                early.is_empty(),
                "sent at {early:?} ms, inside the Retry-After from {} ms",
                answered - T0
            );
        }
        assert_eq!(seen.len(), ALERTS + 3);
    });
}

#[test]
fn a_send_that_panics_is_retried_and_never_freezes_its_alert() {
    paused(async {
        let fixture = Fixture::new();
        let config = config(&monitored(&["slack"], ""));
        let (sender, mut receiver) = channel();
        let slack = Fake::new("slack", &sender, |_, index| {
            if index == 0 {
                Answer::Panic
            } else {
                Answer::Accept
            }
        });
        let metrics = AlertMetrics::default();
        let running = start(
            &fixture,
            &config,
            &[("slack", Arc::clone(&slack) as Arc<dyn Notifier>)],
            &metrics,
        );

        let id = running.change(down(T0)).await.unwrap();
        let panicked = next_from(&mut receiver, "slack").await;
        let retried = next_from(&mut receiver, "slack").await;
        running.change(up(id, T0 + 60_000)).await;
        let resolve = next_from(&mut receiver, "slack").await;
        running.alerter.stop().await;

        assert_eq!(panicked.answer, Answer::Panic);
        // Retried after the first backoff, like any failed attempt.
        assert_eq!(
            (retried.event(), retried.alert.sent_at_ms - T0),
            (AlertEvent::Fire, 5_000)
        );
        assert_eq!(resolve.event(), AlertEvent::Resolve);
        assert_eq!(metrics.attempt_failures("slack"), 1);
        assert_eq!(metrics.abandonments("slack"), 0);
    });
}

#[test]
fn an_idle_worker_writes_nothing_until_a_repeat_is_due() {
    paused(async {
        let fixture = Fixture::new();
        let config = config(&monitored(&["slack"], "repeat = \"1d\""));
        let (sender, mut receiver) = channel();
        let slack: Arc<dyn Notifier> = Fake::accepting("slack", &sender);
        let running = start_with(
            &fixture,
            &config,
            &[("slack", slack)],
            &AlertMetrics::default(),
            WorkerSettings {
                prune_every: Duration::from_hours(1000),
                ..settings()
            },
        );

        running.change(down(T0)).await.unwrap();
        next_from(&mut receiver, "slack").await;
        tokio::time::sleep(Duration::from_mins(1)).await;
        // The worker's last write after the fire runs on the store's writer
        // thread, which the paused clock cannot see: left alone, the clock
        // would race through both sleeps while the worker waits on it, and
        // no idle wake would land inside the window. A blocking task holds
        // the clock while the worker finishes and goes back to sleep.
        tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(50)))
            .await
            .unwrap();
        let settled = fixture.store.writes_queued();
        // Ten idle wakes, an hour apart, with an alert open but no repeat
        // due: reads only.
        tokio::time::sleep(Duration::from_hours(10)).await;
        assert_eq!(fixture.store.writes_queued(), settled);

        let repeat = next_from(&mut receiver, "slack").await;
        assert_eq!(
            (repeat.event(), repeat.alert.sent_at_ms - T0),
            (AlertEvent::Repeat, 24 * HOUR_MS)
        );
        assert!(fixture.store.writes_queued() > settled);
        running.alerter.stop().await;
    });
}

/// Runs `write` against the store with the paused clock standing still,
/// as [`Running::change`] does, so no retry or repeat comes due meanwhile.
async fn still<T: Send + 'static>(
    fixture: &Fixture,
    write: impl AsyncFnOnce(&Store) -> T + Send + 'static,
) -> T {
    let store = Arc::clone(&fixture.store);
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || runtime.block_on(write(&store)))
        .await
        .unwrap()
}

/// Silences check `api` on `node-a` (with `episode`, only during that
/// outage) from `at` until `expires_at`; resolves with the silence's id.
async fn silence(fixture: &Fixture, episode: Option<i64>, at: i64, expires_at: i64) -> i64 {
    let new = NewSilence {
        matchers: Matchers {
            node: Some("node-a".to_owned()),
            subject: Some("api".to_owned()),
            ..Matchers::default()
        },
        episode,
        expires_at: Some(Timestamp::from_millis(expires_at)),
        expires_in: None,
        reason: "deploying the api".to_owned(),
    };
    still(fixture, async move |store: &Store| {
        store
            .create_silence(
                new,
                Timestamp::from_millis(expires_at),
                Actor::token("pager"),
                Timestamp::from_millis(at),
            )
            .await
            .unwrap()
            .id
    })
    .await
}

/// What silence `id` has held back, oldest first, as (kind, ms after T0),
/// once it has held `count`. Waits on the paused clock, a second at a time,
/// for the worker to get there.
async fn held(fixture: &Fixture, id: i64, count: usize) -> Vec<(HeldKind, i64)> {
    loop {
        let silence = fixture
            .store
            .silence(id, Timestamp::from_millis(T0))
            .await
            .unwrap()
            .unwrap();
        if silence.held.len() >= count {
            return silence
                .held
                .iter()
                .rev()
                .map(|held| (held.kind, held.at.as_millis() - T0))
                .collect();
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[test]
fn a_silenced_alert_is_recorded_and_never_sent() {
    paused(async {
        let fixture = Fixture::new();
        let config = config(&monitored(&["slack"], ""));
        let (sender, mut receiver) = channel();
        let slack = Fake::accepting("slack", &sender);
        let running = start(
            &fixture,
            &config,
            &[("slack", Arc::clone(&slack) as Arc<dyn Notifier>)],
            &AlertMetrics::default(),
        );
        let id = silence(&fixture, None, T0, T0 + 24 * HOUR_MS).await;

        let outage = running.change(down(T0)).await.unwrap();
        assert_eq!(held(&fixture, id, 1).await, [(HeldKind::Fire, 0)]);
        // Nothing was ever sent, so the resolve has nothing to answer.
        running.change(up(outage, T0 + 90_000)).await;
        let seen = held(&fixture, id, 2).await;
        running.alerter.stop().await;

        assert_eq!(seen[0], (HeldKind::Fire, 0));
        assert_eq!(seen[1].0, HeldKind::Resolve);
        assert!(slack.seen().is_empty(), "{:?}", slack.seen());
        assert!(receiver.try_recv().is_err());
        // Held back, not failed: nothing for anyone to retry.
        assert_eq!(fixture.store.failed_deliveries(10).await.unwrap(), []);
    });
}

/// Every message `notifier` was sent, cancels apart, as (event, ms after
/// T0).
fn sent(fake: &Fake) -> Vec<(AlertEvent, bool, i64)> {
    fake.seen()
        .iter()
        .map(|attempt| {
            (
                attempt.event(),
                attempt.cancel,
                attempt.alert.sent_at_ms - T0,
            )
        })
        .collect()
}

#[test]
fn an_outage_that_outlives_its_silence_fires_when_the_silence_expires() {
    paused(async {
        let fixture = Fixture::new();
        let config = config(&monitored(&["slack"], ""));
        let (sender, mut receiver) = channel();
        let slack = Fake::accepting("slack", &sender);
        let running = start(
            &fixture,
            &config,
            &[("slack", Arc::clone(&slack) as Arc<dyn Notifier>)],
            &AlertMetrics::default(),
        );
        // An hour's silence; five minutes in, the api goes down.
        let id = silence(&fixture, None, T0, T0 + HOUR_MS).await;
        tokio::time::sleep(Duration::from_mins(5)).await;
        let outage = running.change(down(T0 + 5 * 60_000)).await.unwrap();
        assert_eq!(held(&fixture, id, 1).await[0].0, HeldKind::Fire);

        // Still down when the silence expires: somebody is told then.
        let fire = next_from(&mut receiver, "slack").await;
        assert_eq!(
            (fire.event(), fire.alert.sent_at_ms - T0),
            (AlertEvent::Fire, HOUR_MS)
        );
        running.change(up(outage, T0 + 2 * HOUR_MS)).await;
        let resolve = next_from(&mut receiver, "slack").await;
        running.alerter.stop().await;

        assert_eq!(resolve.event(), AlertEvent::Resolve);
        assert_eq!(sent(&slack).len(), 2, "{:?}", sent(&slack));
        assert_eq!(held(&fixture, id, 1).await.len(), 1);
    });
}

#[test]
fn an_outage_that_outlives_a_silence_ended_early_fires_when_it_ends() {
    paused(async {
        let fixture = Fixture::new();
        let config = config(&monitored(&["slack"], ""));
        let (sender, mut receiver) = channel();
        let slack = Fake::accepting("slack", &sender);
        let running = start(
            &fixture,
            &config,
            &[("slack", Arc::clone(&slack) as Arc<dyn Notifier>)],
            &AlertMetrics::default(),
        );
        let id = silence(&fixture, None, T0, T0 + 24 * HOUR_MS).await;
        let outage = running.change(down(T0)).await.unwrap();
        assert_eq!(held(&fixture, id, 1).await[0].0, HeldKind::Fire);
        still(&fixture, async move |store: &Store| {
            store
                .end_silence(
                    id,
                    Actor::token("pager"),
                    Timestamp::from_millis(T0 + 60_000),
                )
                .await
                .unwrap()
        })
        .await;
        // What `POST /api/v1/silences/{id}/expire` does after ending it.
        let ended_at = tokio::time::Instant::now();
        running.alerter.waker().notify_one();

        // At once, not at the worker's idle wake an hour away.
        let fire = next_from(&mut receiver, "slack").await;
        assert_eq!(fire.event(), AlertEvent::Fire);
        assert!(
            ended_at.elapsed() < Duration::from_secs(1),
            "the fire waited {:?} for the idle wake",
            ended_at.elapsed()
        );
        running.change(up(outage, T0 + 2 * HOUR_MS)).await;
        let resolve = next_from(&mut receiver, "slack").await;
        running.alerter.stop().await;

        assert_eq!(resolve.event(), AlertEvent::Resolve);
        assert_eq!(sent(&slack).len(), 2, "{:?}", sent(&slack));
    });
}

#[test]
fn a_fire_whose_send_failed_before_an_episode_silence_still_gets_its_resolve() {
    paused(async {
        let fixture = Fixture::new();
        let config = config(&monitored(&["slack"], ""));
        let (sender, mut receiver) = channel();
        // The fire fails once, maybe after the receiver saw it.
        let slack = Fake::new("slack", &sender, |_, attempt| {
            if attempt == 0 {
                Answer::Fail
            } else {
                Answer::Accept
            }
        });
        let running = start(
            &fixture,
            &config,
            &[("slack", Arc::clone(&slack) as Arc<dyn Notifier>)],
            &AlertMetrics::default(),
        );
        let outage = running.change(down(T0)).await.unwrap();
        let first = next_from(&mut receiver, "slack").await;
        assert_eq!(
            (first.event(), first.answer),
            (AlertEvent::Fire, Answer::Fail)
        );
        // Acknowledged for this outage: the retry is held back, even
        // before it is due.
        let id = silence(&fixture, Some(outage), T0, T0 + 24 * HOUR_MS).await;
        assert_eq!(held(&fixture, id, 1).await[0].0, HeldKind::Fire);

        running.change(up(outage, T0 + HOUR_MS)).await;
        let resolve = next_from(&mut receiver, "slack").await;
        running.alerter.stop().await;

        assert_eq!(
            (resolve.event(), resolve.answer),
            (AlertEvent::Resolve, Answer::Accept)
        );
        assert_eq!(sent(&slack).len(), 2, "{:?}", sent(&slack));
        assert_eq!(
            held(&fixture, id, 1).await.len(),
            1,
            "the resolve is not held"
        );
    });
}

#[test]
fn a_fire_whose_answer_timed_out_before_a_silence_still_gets_its_resolve() {
    paused(async {
        let fixture = Fixture::new();
        let config = config(&monitored(&["slack"], ""));
        let (sender, mut receiver) = channel();
        // The fire lands but its answer never comes; later sends answer.
        let slack = Fake::new("slack", &sender, |_, attempt| {
            if attempt == 0 {
                Answer::Hang
            } else {
                Answer::Accept
            }
        });
        let running = start_with(
            &fixture,
            &config,
            &[("slack", Arc::clone(&slack) as Arc<dyn Notifier>)],
            &AlertMetrics::default(),
            WorkerSettings {
                send_timeout: Duration::from_secs(10),
                ..settings()
            },
        );
        let outage = running.change(down(T0)).await.unwrap();
        let first = next_from(&mut receiver, "slack").await;
        assert_eq!(
            (first.event(), first.answer),
            (AlertEvent::Fire, Answer::Hang)
        );
        let id = silence(&fixture, None, T0, T0 + 24 * HOUR_MS).await;
        // Timed out at 10s, the retry is held back. When exactly the worker
        // recorded it depends on how far the paused clock ran meanwhile,
        // but it is after the timeout and before the 15s backoff.
        let seen = held(&fixture, id, 1).await;
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, HeldKind::Fire);
        assert!((10_000..15_000).contains(&seen[0].1), "{seen:?}");

        running.change(up(outage, T0 + HOUR_MS)).await;
        let resolve = next_from(&mut receiver, "slack").await;
        running.alerter.stop().await;

        assert_eq!(resolve.event(), AlertEvent::Resolve);
        assert_eq!(sent(&slack).len(), 2, "{:?}", sent(&slack));
        assert_eq!(
            held(&fixture, id, 1).await.len(),
            1,
            "the resolve is not held"
        );
    });
}

#[test]
fn an_emergency_that_never_rang_is_never_cancelled() {
    paused(async {
        let fixture = Fixture::new();
        let config = config(&monitored_by(&pushover("pager"), &["pager"], ""));
        let (sender, _receiver) = channel();
        let pager = Fake::accepting("pager", &sender);
        let running = start(
            &fixture,
            &config,
            &[("pager", Arc::clone(&pager) as Arc<dyn Notifier>)],
            &AlertMetrics::default(),
        );
        let id = silence(&fixture, None, T0, T0 + 24 * HOUR_MS).await;
        let outage = running.change(down(T0)).await.unwrap();
        assert_eq!(held(&fixture, id, 1).await[0].0, HeldKind::Fire);
        running.change(up(outage, T0 + 60_000)).await;
        let seen = held(&fixture, id, 2).await;
        // The resolve is held and the cancel dropped in one pass; wait for
        // the outbox to drain.
        while !fixture.store.outbox().await.unwrap().pending.is_empty() {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        running.alerter.stop().await;

        assert_eq!(seen[1].0, HeldKind::Resolve);
        assert_eq!(sent(&pager), [], "nothing rang, so nothing is cancelled");
        assert_eq!(fixture.store.failed_deliveries(10).await.unwrap(), []);
    });
}

#[test]
fn an_episode_silence_ends_with_its_outage_and_the_re_fire_notifies() {
    paused(async {
        let fixture = Fixture::new();
        let config = config(&monitored(
            &["slack"],
            "repeat = \"1h\"\nnotify_on = \"degraded\"",
        ));
        let (sender, mut receiver) = channel();
        let slack: Arc<dyn Notifier> = Fake::accepting("slack", &sender);
        let running = start(
            &fixture,
            &config,
            &[("slack", slack)],
            &AlertMetrics::default(),
        );

        let degraded = running
            .change(failing("api", Health::Degraded, T0))
            .await
            .unwrap();
        let fire = next_from(&mut receiver, "slack").await;
        assert_eq!(fire.event(), AlertEvent::Fire);
        // Acknowledged for this outage only, for a day.
        let id = silence(&fixture, Some(degraded), T0, T0 + 24 * HOUR_MS).await;
        // The repeat due an hour in is held back. When exactly the worker
        // got to it depends on how far the paused clock ran meanwhile.
        tokio::time::sleep(Duration::from_mins(90)).await;
        let seen = held(&fixture, id, 1).await;
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, HeldKind::Repeat);
        assert!(seen[0].1 >= HOUR_MS, "{seen:?}");

        // Going down ends the degraded outage and opens another: the
        // silence is over, and the re-fire is sent.
        let down = running
            .change(turns(degraded, Health::Down, T0 + 2 * HOUR_MS))
            .await
            .unwrap();
        let refire = next_from(&mut receiver, "slack").await;
        running.change(up(down, T0 + 3 * HOUR_MS)).await;
        let resolve = next_from(&mut receiver, "slack").await;
        running.alerter.stop().await;

        assert_eq!(
            (refire.event(), refire.alert.health),
            (AlertEvent::Fire, Health::Down)
        );
        assert_eq!(resolve.event(), AlertEvent::Resolve);
        let silence = fixture
            .store
            .silence(id, Timestamp::from_millis(T0 + 3 * HOUR_MS))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(silence.state, SilenceState::EpisodeEnded);
        assert_eq!(silence.held_total, 1);
    });
}

#[test]
fn an_escalation_a_silence_held_fires_when_the_silence_expires() {
    paused(async {
        let fixture = Fixture::new();
        let config = config(&monitored(&["slack"], "notify_on = \"degraded\""));
        let (sender, mut receiver) = channel();
        let slack = Fake::accepting("slack", &sender);
        let running = start(
            &fixture,
            &config,
            &[("slack", Arc::clone(&slack) as Arc<dyn Notifier>)],
            &AlertMetrics::default(),
        );
        let degraded = running
            .change(failing("api", Health::Degraded, T0))
            .await
            .unwrap();
        let fire = next_from(&mut receiver, "slack").await;
        assert_eq!(fire.event(), AlertEvent::Fire);

        // Heard as degraded, then silenced for an hour; it goes down
        // meanwhile, and that fire is held.
        tokio::time::sleep(Duration::from_mins(1)).await;
        let id = silence(&fixture, None, T0 + 60_000, T0 + 60_000 + HOUR_MS).await;
        tokio::time::sleep(Duration::from_mins(1)).await;
        let down = running
            .change(turns(degraded, Health::Down, T0 + 120_000))
            .await
            .unwrap();
        assert_eq!(held(&fixture, id, 1).await[0].0, HeldKind::Fire);

        // Still down when the silence expires: the escalation is heard then.
        let escalation = next_from(&mut receiver, "slack").await;
        assert_eq!(
            (
                escalation.event(),
                escalation.alert.health,
                escalation.alert.sent_at_ms - T0
            ),
            (AlertEvent::Fire, Health::Down, 60_000 + HOUR_MS)
        );
        running.change(up(down, T0 + 2 * HOUR_MS)).await;
        let resolve = next_from(&mut receiver, "slack").await;
        running.alerter.stop().await;

        assert_eq!(resolve.event(), AlertEvent::Resolve);
        assert_eq!(sent(&slack).len(), 3, "{:?}", sent(&slack));
    });
}

#[test]
fn a_silence_never_holds_back_the_resolve_of_an_alert_already_sent() {
    paused(async {
        let fixture = Fixture::new();
        let config = config(&monitored(&["slack"], ""));
        let (sender, mut receiver) = channel();
        let slack: Arc<dyn Notifier> = Fake::accepting("slack", &sender);
        let running = start(
            &fixture,
            &config,
            &[("slack", slack)],
            &AlertMetrics::default(),
        );

        let outage = running.change(down(T0)).await.unwrap();
        let fire = next_from(&mut receiver, "slack").await;
        let id = silence(&fixture, None, T0, T0 + 24 * HOUR_MS).await;
        running.change(up(outage, T0 + 60_000)).await;
        let resolve = next_from(&mut receiver, "slack").await;
        running.alerter.stop().await;

        assert_eq!(fire.event(), AlertEvent::Fire);
        assert_eq!(resolve.event(), AlertEvent::Resolve);
        let silence = fixture
            .store
            .silence(id, Timestamp::from_millis(T0 + 60_000))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(silence.held_total, 0);
    });
}
