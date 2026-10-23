// SPDX-License-Identifier: AGPL-3.0-only

//! The whole path: the engine probes a check down and back up, the store
//! queues what the engine decided, and the delivery worker sends it. Real
//! time, like the engine's store tests: the check runs every second.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use common::{Answer, Fake, Fixture, config, monitored};
use tocsin_alert::{
    Alert, AlertEvent, AlertMetrics, Alerter, AlertingHistory, Notifier, Notifiers, Router,
    WorkerSettings,
};
use tocsin_core::Health;
use tocsin_core::probe::{CompiledCheck, ProbeError, ProbeErrorKind, ProbeOutcome};
use tocsin_engine::ports::{BoxFuture, Clock, History, Notices, Runner, Sink, SystemClock};
use tocsin_engine::{CheckChange, Engine, EngineEvent, Notice, Parts, Plan};
use tokio::sync::mpsc;

/// The first probe of each check finds it down; every later one, up.
struct DownThenUp {
    clock: Arc<dyn Clock>,
    runs: std::sync::atomic::AtomicUsize,
}

impl Runner for DownThenUp {
    fn run<'a>(&'a self, check: &'a CompiledCheck) -> BoxFuture<'a, ProbeOutcome> {
        Box::pin(async move {
            let index = self.runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut outcome = ProbeOutcome::failed(
                check.check(),
                self.clock.now_ms(),
                0,
                ProbeError::new(ProbeErrorKind::Connect, "scripted failure"),
            );
            if index > 0 {
                outcome.health = Health::Up;
                outcome.error = None;
            }
            outcome
        })
    }
}

/// The notices the engine sees: fixed.
struct FixedNotices(Vec<Notice>);

impl Notices for FixedNotices {
    fn current(&self) -> BoxFuture<'_, tocsin_store::Result<Vec<Notice>>> {
        Box::pin(async { Ok(self.0.clone()) })
    }
}

/// Hands every check change to a channel.
struct Changes(mpsc::UnboundedSender<CheckChange>);

impl Sink for Changes {
    fn emit(&self, event: EngineEvent) {
        if let EngineEvent::CheckChanged(change) = event {
            let _ = self.0.send(change);
        }
    }
}

/// The engine, the alerter and the channel of check changes.
struct Running {
    engine: Engine,
    alerter: Alerter,
    changes: mpsc::UnboundedReceiver<CheckChange>,
}

async fn run(
    fixture: &Fixture,
    toml: &str,
    notifiers: Vec<(&str, Arc<dyn Notifier>)>,
    notices: Vec<Notice>,
    metrics: &AlertMetrics,
) -> Running {
    let config = config(toml);
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let router = Arc::new(Router::new(&config));
    let notifiers: Notifiers = notifiers
        .into_iter()
        .map(|(id, notifier)| (id.to_owned(), notifier))
        .collect();
    let alerter = Alerter::start(
        Arc::clone(&fixture.store),
        notifiers,
        Arc::clone(&router),
        Arc::clone(&clock),
        metrics.clone(),
        WorkerSettings::default(),
    );
    let history = AlertingHistory::new(
        Arc::clone(&fixture.store),
        router,
        alerter.waker(),
        Arc::clone(&clock),
    );
    let (sender, changes) = mpsc::unbounded_channel();
    let parts = Parts {
        runner: Arc::new(DownThenUp {
            clock: Arc::clone(&clock),
            runs: std::sync::atomic::AtomicUsize::new(0),
        }),
        history: Arc::new(history) as Arc<dyn History>,
        notices: Arc::new(FixedNotices(notices)),
        sink: Arc::new(Changes(sender)),
        clock,
    };
    let plan = Plan::from_config(&config).expect("a runnable plan");
    let engine = Engine::start(plan, parts).await.expect("the engine starts");
    Running {
        engine,
        alerter,
        changes,
    }
}

/// Waits for the check's recovery; ten seconds bounds a broken run.
async fn recovery(
    changes: &mut mpsc::UnboundedReceiver<CheckChange>,
) -> (CheckChange, CheckChange) {
    tokio::time::timeout(Duration::from_secs(10), async {
        let down = changes.recv().await.unwrap();
        let up = changes.recv().await.unwrap();
        (down, up)
    })
    .await
    .expect("the check went down and came back")
}

#[tokio::test]
async fn a_check_going_down_and_up_fires_and_resolves_past_a_failing_notifier() {
    let fixture = Fixture::new();
    let (sender, mut attempts) = mpsc::unbounded_channel();
    let broken = Fake::new("broken", &sender, |_, _| Answer::Fail);
    let metrics = AlertMetrics::default();
    let mut running = run(
        &fixture,
        &monitored(&["slack", "ntfy", "broken"], ""),
        vec![
            ("slack", Fake::accepting("slack", &sender)),
            ("ntfy", Fake::accepting("ntfy", &sender)),
            ("broken", Arc::clone(&broken) as Arc<dyn Notifier>),
        ],
        Vec::new(),
        &metrics,
    )
    .await;

    let (down, up) = recovery(&mut running.changes).await;
    assert_eq!((down.to, up.to), (Health::Down, Health::Up));
    let sent = tokio::time::timeout(Duration::from_secs(10), async {
        let mut by_notifier: HashMap<String, Vec<Alert>> = HashMap::new();
        let done = |sent: &HashMap<String, Vec<Alert>>| {
            ["slack", "ntfy"]
                .iter()
                .all(|id| sent.get(*id).is_some_and(|alerts| alerts.len() >= 2))
        };
        while !done(&by_notifier) {
            let attempt = attempts.recv().await.unwrap();
            by_notifier
                .entry(attempt.notifier)
                .or_default()
                .push(attempt.alert);
        }
        ["slack", "ntfy"].map(|id| {
            let alerts = &by_notifier[id];
            (id, alerts[0].clone(), alerts[1].clone())
        })
    })
    .await
    .expect("both working notifiers fire and resolve");
    running.engine.shutdown().await;
    running.alerter.stop().await;

    for (notifier, fire, resolve) in sent {
        assert_eq!(
            (fire.event, resolve.event),
            (AlertEvent::Fire, AlertEvent::Resolve),
            "{notifier}"
        );
        assert_eq!(fire.identity(), "node-a/check/api", "{notifier}");
        assert_eq!(resolve.identity(), fire.identity(), "{notifier}");
        assert_eq!(resolve.started_at_ms, down.since_ms, "{notifier}");
        assert_eq!(resolve.ended_at_ms, Some(up.since_ms), "{notifier}");
    }
    // The broken notifier failed its fire, and so has not been sent the
    // resolve: that waits for the fire to be delivered or given up.
    let broken_seen = broken.seen();
    assert!(!broken_seen.is_empty());
    assert!(
        broken_seen
            .iter()
            .all(|attempt| attempt.event() == AlertEvent::Fire)
    );
    assert!(metrics.attempt_failures("broken") >= 1);
    assert_eq!(
        metrics.attempt_failures("slack") + metrics.attempt_failures("ntfy"),
        0
    );
}

#[tokio::test]
async fn an_outage_inside_a_maintenance_window_alerts_nobody() {
    let fixture = Fixture::new();
    let (sender, _attempts) = mpsc::unbounded_channel();
    let slack = Fake::accepting("slack", &sender);
    let now = SystemClock.now_ms();
    let mut running = run(
        &fixture,
        &monitored(&["slack"], ""),
        vec![("slack", Arc::clone(&slack) as Arc<dyn Notifier>)],
        vec![Notice::Maintenance {
            components: vec!["web".into()],
            starts_at_ms: now - 3_600_000,
            ends_at_ms: now + 3_600_000,
        }],
        &AlertMetrics::default(),
    )
    .await;

    let (down, up) = recovery(&mut running.changes).await;
    running.engine.shutdown().await;
    running.alerter.stop().await;

    assert_eq!((down.to, up.to), (Health::Down, Health::Up));
    assert!(down.maintenance);
    // The outage was recorded, and ended, but never alerted: nothing was
    // queued, so nothing could be sent.
    assert!(down.opened_outage_id.is_some());
    assert_eq!(up.ended_outage_id, down.opened_outage_id);
    assert_eq!(fixture.store.alerts().await.unwrap().len(), 0);
    assert_eq!(fixture.store.outbox().await.unwrap().pending.len(), 0);
    assert_eq!(slack.seen().len(), 0);
}
