// SPDX-License-Identifier: AGPL-3.0-only

//! The process's Prometheus metrics, served at `GET /metrics` in the
//! Prometheus text format (version 0.0.4). docs/metrics.md lists them.
//!
//! Two kinds. Counts and the probe-duration histogram accumulate as things
//! happen and live in [`Metrics`]'s registry. Everything that describes
//! the state now (each check's health, each component's status, the
//! database's size) is read fresh by [`Metrics::render`] from the engine's
//! snapshot at each scrape, so a check or component a reload removed is
//! simply not there. The accumulated series of a removed check or notifier
//! are dropped at the same moment, so every label set is bounded by the
//! configuration: a label names a check, heartbeat, component, notifier,
//! node or status, never anything from one result.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::Registry;
use tocsin_alert::AlertMetrics;
use tocsin_core::Health;
use tocsin_core::config::{Config, Id};
use tocsin_core::incident::Timestamp;
use tocsin_core::probe::{CompiledCheck, ProbeOutcome};
use tocsin_core::state::ComponentStatus;
use tocsin_engine::ports::{BoxFuture, Runner};
use tocsin_engine::{MemberKind, Snapshot};
use tocsin_store::SlackCounts;

/// A series' labels, in the order they are written.
type Labels = Vec<(&'static str, String)>;

/// The Slack connections now: how many of each kind are live and dead,
/// and each operator connection's name and whether it is live.
pub type SlackCensus = (SlackCounts, Vec<(String, bool)>);

/// A gauge holding a fraction.
type FloatGauge = Gauge<f64, AtomicU64>;

/// The upper bounds of the probe-duration buckets, in seconds: from a
/// fast local probe to the longest timeout worth having.
const DURATION_BUCKETS: [f64; 13] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
];

/// Every metric the process exports.
#[derive(Debug)]
pub struct Metrics {
    registry: Registry,
    /// Alerting's counters.
    pub alerting: AlertMetrics,
    /// Whether the configuration last reloaded.
    pub config: ConfigMetrics,
    /// How long probes take and when heartbeats last pinged.
    pub checks: CheckMetrics,
}

impl Metrics {
    /// A registry holding every metric, all at zero.
    pub fn new() -> Self {
        let mut registry = Registry::default();
        let alerting = AlertMetrics::register(&mut registry);
        let config = ConfigMetrics::register(&mut registry);
        let checks = CheckMetrics::register(&mut registry);
        let build = Family::<Labels, Gauge>::default();
        build
            .get_or_create(&vec![("version", env!("CARGO_PKG_VERSION").to_owned())])
            .set(1);
        registry.register(
            "tocsin_build_info",
            "Always 1; the version label is the running tocsin's",
            build,
        );
        Metrics {
            registry,
            alerting,
            config,
            checks,
        }
    }

    /// Every metric now, in the Prometheus text format: the registry's,
    /// less the series of whatever `config` no longer has, then the state
    /// in `snapshot`, which must be the engine's for `config`, the
    /// database's size, and the Slack connections when there are any to
    /// describe.
    pub fn render(
        &self,
        snapshot: &Snapshot,
        config: &Config,
        db_size_bytes: u64,
        slack: Option<&SlackCensus>,
    ) -> String {
        let node = config.server.node_id.as_deref().unwrap_or_default();
        // Held until the registry is written, so a probe of a check the
        // configuration dropped cannot bring its series back in between.
        let _durations = self.checks.retain(snapshot, node);
        let pings = self.checks.last_pings(snapshot);
        self.alerting.retain(|notifier| {
            config
                .notifiers
                .iter()
                .any(|configured| configured.id.as_str() == notifier)
        });
        let mut now = state_now(snapshot, node, &pings, db_size_bytes);
        if let Some(census) = slack {
            slack_now(&mut now, census, config);
        }
        let mut text = String::new();
        prometheus_client::encoding::text::encode_registry(&mut text, &self.registry)
            .expect("writing to a String cannot fail");
        prometheus_client::encoding::text::encode_registry(&mut text, &now)
            .expect("writing to a String cannot fail");
        prometheus_text(&text)
    }

    /// The registry alone, in the Prometheus text format.
    #[cfg(test)]
    pub fn encode(&self) -> String {
        let mut text = String::new();
        prometheus_client::encoding::text::encode_registry(&mut text, &self.registry)
            .expect("writing to a String cannot fail");
        prometheus_text(&text)
    }
}

/// The gauges describing the state now, in a registry built for one
/// scrape: `pings` holds each heartbeat's last ping, unix seconds.
fn state_now(
    snapshot: &Snapshot,
    node: &str,
    pings: &HashMap<String, i64>,
    db_size_bytes: u64,
) -> Registry {
    let mut registry = Registry::default();
    let up = Family::<Labels, FloatGauge>::default();
    let last_run = Family::<Labels, Gauge>::default();
    let last_ping = Family::<Labels, Gauge>::default();
    let component_status = Family::<Labels, Gauge>::default();
    for check in &snapshot.checks {
        let component = check.component_id.clone().unwrap_or_default();
        if let Some(state) = check.state {
            let kind = match check.kind {
                MemberKind::Check => "check",
                MemberKind::Heartbeat => "heartbeat",
            };
            let mut labels = check_labels(&check.id, &component, node);
            labels.push(("kind", kind.to_owned()));
            up.get_or_create(&labels).set(match state.health {
                Health::Up => 1.0,
                Health::Degraded => 0.5,
                Health::Down => 0.0,
            });
        }
        if check.kind == MemberKind::Check
            && let Some(last) = check.last
        {
            last_run
                .get_or_create(&check_labels(&check.id, &component, node))
                .set(last.started_at_ms.div_euclid(1000));
        }
        if let Some(at) = pings.get(&check.id) {
            last_ping
                .get_or_create(&check_labels(&check.id, &component, node))
                .set(*at);
        }
    }
    for component in &snapshot.components {
        for status in ComponentStatus::ALL {
            component_status
                .get_or_create(&vec![
                    ("component", component.id.clone()),
                    ("node", node.to_owned()),
                    ("status", status.as_str().to_owned()),
                ])
                .set(i64::from(status == component.status));
        }
    }
    let db_size = Gauge::<u64, AtomicU64>::default();
    db_size.set(db_size_bytes);
    registry.register(
        "tocsin_check_up",
        "Each check's and heartbeat's confirmed health: 1 up, 0.5 degraded, 0 down; absent until confirmed",
        up,
    );
    registry.register(
        "tocsin_check_last_run_timestamp",
        "When each check's latest probe started, in unix seconds",
        last_run,
    );
    registry.register(
        "tocsin_heartbeat_last_ping_timestamp",
        "When each heartbeat was last pinged, in unix seconds; absent until its first ping since tocsin started",
        last_ping,
    );
    registry.register(
        "tocsin_component_status",
        "1 for the status each component shows on the status page, 0 for the others",
        component_status,
    );
    registry.register(
        "tocsin_db_size_bytes",
        "The database's size on disk, its write-ahead log included",
        db_size,
    );
    registry
}

/// Adds the Slack connections' gauges to `registry`: how many there are
/// of each purpose and state, and whether the connection each notifier
/// names is live. A name not connected is 0, as a dead one is: its
/// alerts fail either way.
fn slack_now(registry: &mut Registry, (counts, operator): &SlackCensus, config: &Config) {
    let connections = Family::<Labels, Gauge>::default();
    for (purpose, state, count) in [
        ("operator", "live", counts.operator_live),
        ("operator", "dead", counts.operator_dead),
        ("subscriber", "live", counts.subscriber_live),
        ("subscriber", "dead", counts.subscriber_dead),
    ] {
        connections
            .get_or_create(&vec![
                ("purpose", purpose.to_owned()),
                ("state", state.to_owned()),
            ])
            .set(count);
    }
    let up = Family::<Labels, Gauge>::default();
    for notifier in &config.notifiers {
        let Some(name) = notifier
            .slack
            .as_ref()
            .and_then(|slack| slack.connection.as_ref())
        else {
            continue;
        };
        let live = operator
            .iter()
            .any(|(known, live)| known == name.as_str() && *live);
        up.get_or_create(&vec![
            ("notifier", notifier.id.as_str().to_owned()),
            ("connection", name.as_str().to_owned()),
        ])
        .set(i64::from(live));
    }
    registry.register(
        "tocsin_slack_connections",
        "Slack channels connected by signing in, by purpose (operator or subscriber) and state (live, or dead because Slack revoked it)",
        connections,
    );
    registry.register(
        "tocsin_slack_connection_up",
        "1 when the Slack connection a notifier names is connected and live, 0 when it is missing or Slack revoked it",
        up,
    );
}

fn check_labels(check: &str, component: &str, node: &str) -> Labels {
    vec![
        ("check", check.to_owned()),
        ("component", component.to_owned()),
        ("node", node.to_owned()),
    ]
}

/// `open_metrics`, prometheus-client's `OpenMetrics` text, rewritten in
/// the Prometheus text format 0.0.4, which every scraper and `promtool`
/// parse: a counter family is named with its `_total` suffix, and there is
/// no `# EOF`. Nothing else differs for the types used here (counters,
/// gauges and classic histograms, no units).
fn prometheus_text(open_metrics: &str) -> String {
    let counters: HashSet<&str> = open_metrics
        .lines()
        .filter_map(|line| line.strip_prefix("# TYPE ")?.strip_suffix(" counter"))
        .collect();
    let mut text = String::with_capacity(open_metrics.len());
    for line in open_metrics.lines() {
        if line == "# EOF" {
            continue;
        }
        let counter = ["# HELP ", "# TYPE "].into_iter().find_map(|tag| {
            let (name, rest) = line.strip_prefix(tag)?.split_once(' ')?;
            counters.contains(name).then_some((tag, name, rest))
        });
        match counter {
            Some((tag, name, rest)) => writeln!(text, "{tag}{name}_total {rest}"),
            None => writeln!(text, "{line}"),
        }
        .expect("writing to a String cannot fail");
    }
    text
}

/// How configuration reloads went:
///
/// - `tocsin_config_reload_success`, 1 while the configuration last read
///   is the one running, 0 after a reload failed and the last good one was
///   kept. It starts at 1: serve does not start on a bad configuration.
/// - `tocsin_config_last_error_timestamp`, when a reload last failed, in
///   unix seconds; 0 if none has.
#[derive(Debug, Clone)]
pub struct ConfigMetrics {
    reload_success: Gauge,
    last_error: Gauge,
}

impl ConfigMetrics {
    fn register(registry: &mut Registry) -> Self {
        let metrics = ConfigMetrics {
            reload_success: Gauge::default(),
            last_error: Gauge::default(),
        };
        metrics.reload_success.set(1);
        registry.register(
            "tocsin_config_reload_success",
            "1 if the last configuration reload succeeded, 0 if it failed and the last good configuration is still running",
            metrics.reload_success.clone(),
        );
        registry.register(
            "tocsin_config_last_error_timestamp",
            "When a configuration reload last failed, in unix seconds; 0 if none has",
            metrics.last_error.clone(),
        );
        metrics
    }

    /// A reload put its configuration in place.
    pub fn succeeded(&self) {
        self.reload_success.set(1);
    }

    /// A reload at `at` failed.
    pub fn failed(&self, at: Timestamp) {
        self.reload_success.set(0);
        self.last_error.set(at.as_millis().div_euclid(1000));
    }
}

/// What the checks and heartbeats do, as it happens:
///
/// - `tocsin_check_duration_seconds{check,component,node}`, a histogram
///   of how long each check's probes take;
/// - `tocsin_heartbeat_last_ping_timestamp{check,component,node}`, when
///   each heartbeat was last pinged, in unix seconds; absent until its
///   first ping since tocsin started.
///
/// Cheap to clone; clones share the series.
#[derive(Debug, Clone)]
pub struct CheckMetrics {
    durations: Family<Labels, Histogram, fn() -> Histogram>,
    /// The label sets `durations` has series for, which a family cannot
    /// list, so [`CheckMetrics::retain`] can find the stale ones.
    observed: Arc<Mutex<HashSet<Labels>>>,
    /// Each heartbeat's last ping, unix seconds, by heartbeat id.
    pings: Arc<Mutex<HashMap<String, i64>>>,
}

impl CheckMetrics {
    fn register(registry: &mut Registry) -> Self {
        let metrics = CheckMetrics {
            durations: Family::new_with_constructor(|| Histogram::new(DURATION_BUCKETS)),
            observed: Arc::default(),
            pings: Arc::default(),
        };
        registry.register(
            "tocsin_check_duration_seconds",
            "How long each check's probes take",
            metrics.durations.clone(),
        );
        metrics
    }

    /// Records one probe of `check` on `node` taking `duration`.
    pub fn observe(&self, check: &str, component: Option<&str>, node: &str, duration: Duration) {
        let labels = check_labels(check, component.unwrap_or_default(), node);
        // Under the lock, so `retain` cannot forget the label set while
        // its series is created.
        let mut observed = lock(&self.observed);
        self.durations
            .get_or_create(&labels)
            .observe(duration.as_secs_f64());
        observed.insert(labels);
    }

    /// Records a ping of heartbeat `id` at `at_ms`, unix milliseconds.
    pub fn pinged(&self, id: &str, at_ms: i64) {
        lock(&self.pings).insert(id.to_owned(), at_ms.div_euclid(1000));
    }

    /// Drops the duration series of every check `snapshot` does not have,
    /// or has under another component, and holds off [`Self::observe`]
    /// until the guard it returns is dropped.
    fn retain(&self, snapshot: &Snapshot, node: &str) -> MutexGuard<'_, HashSet<Labels>> {
        let wanted: HashSet<Labels> = snapshot
            .checks
            .iter()
            .filter(|check| check.kind == MemberKind::Check)
            .map(|check| {
                check_labels(
                    &check.id,
                    check.component_id.as_deref().unwrap_or_default(),
                    node,
                )
            })
            .collect();
        let mut observed = lock(&self.observed);
        observed.retain(|labels| {
            let keep = wanted.contains(labels);
            if !keep {
                self.durations.remove(labels);
            }
            keep
        });
        observed
    }

    /// Each heartbeat's last ping, unix seconds, by heartbeat id, less any
    /// `snapshot` does not have, which are forgotten.
    fn last_pings(&self, snapshot: &Snapshot) -> HashMap<String, i64> {
        let mut pings = lock(&self.pings);
        pings.retain(|id, _| {
            snapshot
                .checks
                .iter()
                .any(|check| check.kind == MemberKind::Heartbeat && check.id == *id)
        });
        pings.clone()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Nothing done under these locks panics.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A [`Runner`] that records how long each probe took in `metrics`.
pub(crate) struct Metered<R> {
    inner: R,
    metrics: CheckMetrics,
    node: String,
}

impl<R> Metered<R> {
    /// Runs probes with `inner`, recording them as made on `node`.
    pub(crate) fn new(inner: R, metrics: CheckMetrics, node: String) -> Self {
        Metered {
            inner,
            metrics,
            node,
        }
    }
}

impl<R: Runner> Runner for Metered<R> {
    fn run<'a>(&'a self, check: &'a CompiledCheck) -> BoxFuture<'a, ProbeOutcome> {
        Box::pin(async move {
            let outcome = self.inner.run(check).await;
            let config = check.check();
            self.metrics.observe(
                config.id.as_str(),
                config.component.as_ref().map(Id::as_str),
                &self.node,
                Duration::from_millis(outcome.duration_ms),
            );
            outcome
        })
    }
}

#[cfg(test)]
mod tests {
    use tocsin_engine::{CheckSnapshot, MemberKind, Snapshot};

    use super::{Metrics, prometheus_text};

    #[test]
    fn a_heartbeat_dropped_and_added_back_has_no_ping_until_its_next() {
        let metrics = Metrics::new();
        let nightly = Snapshot {
            checks: vec![CheckSnapshot {
                id: "nightly".into(),
                kind: MemberKind::Heartbeat,
                component_id: None,
                state: None,
                last: None,
            }],
            components: Vec::new(),
        };
        metrics.checks.pinged("nightly", 1_760_000_000_500);
        assert_eq!(
            metrics.checks.last_pings(&nightly).get("nightly"),
            Some(&1_760_000_000)
        );
        // A reload drops it, and a later one adds it back.
        assert!(metrics.checks.last_pings(&Snapshot::default()).is_empty());
        assert!(metrics.checks.last_pings(&nightly).is_empty());
    }

    #[test]
    fn failed_attempts_and_abandoned_notifications_are_exported_per_notifier() {
        let metrics = Metrics::new();
        metrics.alerting.attempt_failed("pager");
        metrics.alerting.attempt_failed("pager");
        metrics.alerting.abandoned("pager");
        let text = metrics.encode();
        for line in [
            "# TYPE tocsin_notification_failures_total counter\n",
            "tocsin_notification_failures_total{notifier=\"pager\"} 2\n",
            "# HELP tocsin_notifications_abandoned_total ",
            "tocsin_notifications_abandoned_total{notifier=\"pager\"} 1\n",
        ] {
            assert!(text.contains(line), "{line} missing from {text}");
        }
    }

    #[test]
    fn counters_are_named_with_their_suffix_and_nothing_else_changes() {
        let open_metrics = "# HELP a_b Counted.\n# TYPE a_b counter\na_b_total 2\n\
                            # HELP c Gauged.\n# TYPE c gauge\nc 1\n# EOF\n";
        assert_eq!(
            prometheus_text(open_metrics),
            "# HELP a_b_total Counted.\n# TYPE a_b_total counter\na_b_total 2\n\
             # HELP c Gauged.\n# TYPE c gauge\nc 1\n"
        );
    }
}
