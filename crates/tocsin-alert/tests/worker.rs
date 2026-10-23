// SPDX-License-Identifier: AGPL-3.0-only

//! The delivery worker against a real store, on a paused clock: retries
//! and their backoff, a resolve that waits for its fire, repeats, failed
//! deliveries, a notifier that hangs, and a restart with a delivery still
//! queued.
//!
//! Tests wait on the fakes' channel, never on a tokio timeout: while the
//! store's writer thread works, a paused clock jumps to the next timer, and
//! a test's own timeout would be that timer. [`paused`] bounds each one by
//! the real clock instead.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Answer, Attempt, Fake, Fixture, T0, config, down, monitored, next_from, paused, up};
use tocsin_alert::{
    AlertEvent, AlertMetrics, Alerter, AlertingHistory, Backoff, Notifier, Notifiers, Router,
    WorkerSettings,
};
use tocsin_core::config::Config;
use tocsin_engine::ports::{Clock, History, TokioClock};
use tocsin_store::DeliveryKind;
use tokio::sync::mpsc;

const HOUR_MS: i64 = 3_600_000;

/// Retries at 5s, 10s, 20s ...; four attempts. The idle wake and the send
/// timeout are an hour, so a clock that jumps while the store works never
/// crosses them; stopping abandons sends after a tenth of a second.
fn settings() -> WorkerSettings {
    WorkerSettings {
        backoff: Backoff {
            base: Duration::from_secs(5),
            cap: Duration::from_secs(60),
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
    history: AlertingHistory,
}

fn start(
    fixture: &Fixture,
    config: &Config,
    notifiers: &[(&str, Arc<dyn Notifier>)],
    metrics: &AlertMetrics,
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
        settings(),
    );
    let history = AlertingHistory::new(Arc::clone(&fixture.store), router, alerter.waker(), clock);
    Running { alerter, history }
}

fn channel() -> (
    mpsc::UnboundedSender<Attempt>,
    mpsc::UnboundedReceiver<Attempt>,
) {
    mpsc::unbounded_channel()
}

/// Opens the alerting outage; resolves with its id.
async fn open(history: &AlertingHistory, at: i64) -> i64 {
    history.change_outage(down(at)).await.unwrap().unwrap()
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

        let id = open(&running.history, T0).await;
        let fire = next_from(&mut receiver, "slack").await.alert;
        running
            .history
            .change_outage(up(id, T0 + 90_000))
            .await
            .unwrap();
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

        let id = open(&running.history, T0).await;
        let first = next_from(&mut receiver, "slack").await;
        // The recovery comes while the fire is still failing.
        running
            .history
            .change_outage(up(id, T0 + 1_000))
            .await
            .unwrap();
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

        let id = open(&running.history, T0).await;
        running
            .history
            .change_outage(up(id, T0 + 1_000))
            .await
            .unwrap();
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

        let id = open(&running.history, T0).await;
        let mut seen = Vec::new();
        for _ in 0..3 {
            let attempt = next_from(&mut receiver, "slack").await;
            seen.push((attempt.event(), attempt.alert.sent_at_ms - T0));
        }
        running
            .history
            .change_outage(up(id, T0 + 2 * HOUR_MS + 1_000))
            .await
            .unwrap();
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

    let id = open(&running.history, T0).await;
    running
        .history
        .change_outage(up(id, T0 + 1_000))
        .await
        .unwrap();
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

        let id = open(&running.history, T0).await;
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

        running
            .history
            .change_outage(up(id, T0 + 1_000))
            .await
            .unwrap();
        let resolve = next_from(&mut receiver, "slack").await.alert;
        assert_eq!(resolve.event, AlertEvent::Resolve);
        assert_eq!(resolve.identity(), fire.identity());
        running.alerter.stop().await;
    });
}
