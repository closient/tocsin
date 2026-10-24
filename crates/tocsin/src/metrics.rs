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
//!
//! [`Metrics::read`] takes the same state as one [`Reading`], which the
//! OpenTelemetry exporter (`crate::otlp`) sends over OTLP; `/metrics` is
//! written from a reading too, so the two cannot disagree.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use prometheus_client::encoding::{EncodeMetric, MetricEncoder, NoLabelSet};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::{MetricType, TypedMetric};
use prometheus_client::registry::Registry;
use tocsin_alert::AlertMetrics;
use tocsin_core::Health;
use tocsin_core::config::{CheckType, Config, Id};
use tocsin_core::incident::Timestamp;
use tocsin_core::probe::{CompiledCheck, ProbeOutcome};
use tocsin_core::state::ComponentStatus;
use tocsin_engine::ports::{BoxFuture, History, Runner};
use tocsin_engine::{MemberKind, Snapshot};
use tocsin_store::{
    LastUp, LatestResult, NewCheckResult, Outage, OutageChange, Result as StoreResult, SlackCounts,
    SubjectKind,
};

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
    /// How the OpenTelemetry export goes.
    pub export: ExportMetrics,
    /// The Slack connections the running configuration's notifiers name,
    /// as [`slack_names`] gives them, for [`Metrics::read`].
    slack: Mutex<Option<Vec<(String, String)>>>,
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
        // Last, and empty until an exporter starts, so with no
        // `[telemetry]` the text is what it was before there was one.
        let export = ExportMetrics::register(&mut registry);
        Metrics {
            registry,
            alerting,
            config,
            checks,
            export,
            slack: Mutex::default(),
        }
    }

    /// Takes what the metrics describe from `config`, the configuration
    /// running now.
    pub fn configured(&self, config: &Config) {
        self.checks.configured(config);
        *lock(&self.slack) = slack_names(config);
    }

    /// Whether a [`Metrics::read`] describes the Slack connections, so
    /// needs them read.
    pub fn reads_slack(&self) -> bool {
        lock(&self.slack).is_some()
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
        let (mut reading, _durations) = self.checks.reading(snapshot, node);
        self.alerting.retain(|notifier| {
            config
                .notifiers
                .iter()
                .any(|configured| configured.id.as_str() == notifier)
        });
        reading.slack =
            slack.map(|census| SlackReading::of(census, &slack_names(config).unwrap_or_default()));
        let now = state_now(&reading, node, db_size_bytes);
        let mut text = String::new();
        prometheus_client::encoding::text::encode_registry(&mut text, &self.registry)
            .expect("writing to a String cannot fail");
        prometheus_client::encoding::text::encode_registry(&mut text, &now)
            .expect("writing to a String cannot fail");
        prometheus_text(&text)
    }

    /// The checks' and heartbeats' state in `snapshot`, which must be the
    /// engine's, as probed on `node`, less the series of whatever
    /// `snapshot` no longer has, and the Slack connections in `slack`
    /// when [`Metrics::reads_slack`]: what [`Metrics::render`] writes,
    /// for the OpenTelemetry exporter.
    pub fn read(&self, snapshot: &Snapshot, node: &str, slack: Option<&SlackCensus>) -> Reading {
        let mut reading = self.checks.reading(snapshot, node).0;
        if let Some(names) = &*lock(&self.slack) {
            reading.slack = slack.map(|census| SlackReading::of(census, names));
        }
        reading
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

/// The gauges describing the state now in `reading`, and the database's
/// size, in a registry built for one scrape.
fn state_now(reading: &Reading, node: &str, db_size_bytes: u64) -> Registry {
    let mut registry = Registry::default();
    let up = Family::<Labels, FloatGauge>::default();
    let last_run = Family::<Labels, Gauge>::default();
    let last_ping = Family::<Labels, Gauge>::default();
    let component_status = Family::<Labels, Gauge>::default();
    for member in &reading.members {
        if let Some(health) = member.health {
            let mut labels = check_labels(&member.id, &member.component, node);
            labels.push(("kind", kind_name(member.kind).to_owned()));
            up.get_or_create(&labels).set(up_value(health));
        }
        if let Some(at) = member.last_run {
            last_run
                .get_or_create(&check_labels(&member.id, &member.component, node))
                .set(at);
        }
        if let Some(at) = member.last_ping {
            last_ping
                .get_or_create(&check_labels(&member.id, &member.component, node))
                .set(at);
        }
    }
    for (component, status) in &reading.components {
        for each in ComponentStatus::ALL {
            component_status
                .get_or_create(&vec![
                    ("component", component.clone()),
                    ("node", node.to_owned()),
                    ("status", each.as_str().to_owned()),
                ])
                .set(i64::from(each == *status));
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
    if let Some(slack) = &reading.slack {
        slack_now(&mut registry, slack);
    }
    registry
}

/// Adds `slack`'s gauges to `registry`: how many connections there are
/// of each purpose and state, and whether the connection each notifier
/// names is live.
fn slack_now(registry: &mut Registry, slack: &SlackReading) {
    let connections = Family::<Labels, Gauge>::default();
    for (purpose, state, count) in slack.counted() {
        connections
            .get_or_create(&vec![
                ("purpose", purpose.to_owned()),
                ("state", state.to_owned()),
            ])
            .set(count);
    }
    let up = Family::<Labels, Gauge>::default();
    for named in &slack.named {
        up.get_or_create(&vec![
            ("notifier", named.notifier.clone()),
            ("connection", named.connection.clone()),
        ])
        .set(i64::from(named.live));
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

/// `tocsin_check_up`'s value for `health`.
pub(crate) fn up_value(health: Health) -> f64 {
    match health {
        Health::Up => 1.0,
        Health::Degraded => 0.5,
        Health::Down => 0.0,
    }
}

/// The `kind` label's value.
pub(crate) fn kind_name(kind: MemberKind) -> &'static str {
    match kind {
        MemberKind::Check => "check",
        MemberKind::Heartbeat => "heartbeat",
    }
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
///   first ping since tocsin started;
/// - how many results each check and heartbeat recorded, by health, which
///   only the OpenTelemetry export carries (docs/metrics.md says why).
///
/// Cheap to clone; clones share the series.
#[derive(Debug, Clone)]
pub struct CheckMetrics {
    durations: Family<Labels, DurationHistogram, fn() -> DurationHistogram>,
    /// The label sets `durations` has series for, which a family cannot
    /// list, so [`CheckMetrics::retain`] can find the stale ones.
    observed: Arc<Mutex<HashSet<Labels>>>,
    /// Each configured check's type, by id, as last configured.
    types: Arc<Mutex<HashMap<String, CheckType>>>,
    /// Each heartbeat's last ping, unix seconds, by heartbeat id.
    pings: Arc<Mutex<HashMap<String, i64>>>,
    /// The results each check and heartbeat recorded since tocsin started
    /// or the configuration added it.
    results: Arc<Mutex<HashMap<(MemberKind, String), ResultCounts>>>,
}

impl CheckMetrics {
    fn register(registry: &mut Registry) -> Self {
        let metrics = CheckMetrics {
            durations: Family::new_with_constructor(DurationHistogram::new),
            observed: Arc::default(),
            types: Arc::default(),
            pings: Arc::default(),
            results: Arc::default(),
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

    /// Takes each check's type from `config`, the configuration running
    /// now.
    pub fn configured(&self, config: &Config) {
        *lock(&self.types) = config
            .checks
            .iter()
            .map(|check| (check.id.as_str().to_owned(), check.check_type))
            .collect();
    }

    /// Records a ping of heartbeat `id` at `at_ms`, unix milliseconds.
    pub fn pinged(&self, id: &str, at_ms: i64) {
        lock(&self.pings).insert(id.to_owned(), at_ms.div_euclid(1000));
    }

    /// Records a result of `health` stored for the check or heartbeat `id`.
    pub fn recorded(&self, kind: MemberKind, id: &str, health: Health) {
        lock(&self.results)
            .entry((kind, id.to_owned()))
            .or_insert_with(ResultCounts::new)
            .count(health);
    }

    /// The state in `snapshot` as probed on `node`, less what `snapshot`
    /// does not have, which is forgotten; and the guard of
    /// [`Self::retain`], holding off [`Self::observe`] until dropped.
    fn reading(
        &self,
        snapshot: &Snapshot,
        node: &str,
    ) -> (Reading, MutexGuard<'_, HashSet<Labels>>) {
        let observed = self.retain(snapshot, node);
        let types = lock(&self.types).clone();
        let pings = self.last_pings(snapshot);
        let results = self.last_results(snapshot);
        let members = snapshot
            .checks
            .iter()
            .map(|check| {
                let component = check.component_id.clone().unwrap_or_default();
                let labels = check_labels(&check.id, &component, node);
                let is_check = check.kind == MemberKind::Check;
                let duration = (is_check && observed.contains(&labels))
                    .then(|| self.durations.get(&labels))
                    .flatten()
                    .map(|histogram| histogram.snapshot());
                MemberReading {
                    kind: check.kind,
                    check_type: is_check.then(|| types.get(&check.id).copied()).flatten(),
                    health: check.state.map(|state| state.health),
                    last_run: (check.kind == MemberKind::Check)
                        .then_some(check.last)
                        .flatten()
                        .map(|last| last.started_at_ms.div_euclid(1000)),
                    // By id alone, as `/metrics` always has.
                    last_ping: pings.get(&check.id).copied(),
                    duration,
                    results: results.get(&(check.kind, check.id.clone())).cloned(),
                    id: check.id.clone(),
                    component,
                }
            })
            .collect();
        let components = snapshot
            .components
            .iter()
            .map(|component| (component.id.clone(), component.status))
            .collect();
        (
            Reading {
                members,
                components,
                slack: None,
            },
            observed,
        )
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

    /// Each check's and heartbeat's result counts, less any `snapshot`
    /// does not have, which are forgotten.
    fn last_results(&self, snapshot: &Snapshot) -> HashMap<(MemberKind, String), ResultCounts> {
        let mut results = lock(&self.results);
        results.retain(|(kind, id), _| {
            snapshot
                .checks
                .iter()
                .any(|check| check.kind == *kind && check.id == *id)
        });
        results.clone()
    }
}

/// The checks' and heartbeats' state at one moment, as `/metrics` and the
/// OpenTelemetry export both report it.
#[derive(Debug, Clone, Default)]
pub struct Reading {
    /// Every check and heartbeat configured, in the engine's order.
    pub members: Vec<MemberReading>,
    /// Every component, with the status the page shows for it.
    pub components: Vec<(String, ComponentStatus)>,
    /// The Slack connections, when there are any to describe.
    pub slack: Option<SlackReading>,
}

/// The Slack connections' part of a [`Reading`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackReading {
    /// How many connections there are of each purpose and state.
    pub counts: SlackCounts,
    /// Each notifier that names an operator connection, in the
    /// configuration's order.
    pub named: Vec<NamedConnection>,
}

/// A notifier's Slack connection, by name, in a [`SlackReading`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedConnection {
    /// The notifier's id.
    pub notifier: String,
    /// The connection's name.
    pub connection: String,
    /// Whether that connection is connected and live. One not connected
    /// is not, as a dead one is not: its alerts fail either way.
    pub live: bool,
}

impl SlackReading {
    /// `census` as the notifiers `names` lists see it, each with the
    /// connection it names.
    fn of((counts, operator): &SlackCensus, names: &[(String, String)]) -> Self {
        SlackReading {
            counts: *counts,
            named: names
                .iter()
                .map(|(notifier, connection)| NamedConnection {
                    notifier: notifier.clone(),
                    connection: connection.clone(),
                    live: operator
                        .iter()
                        .any(|(known, live)| known == connection && *live),
                })
                .collect(),
        }
    }

    /// How many connections there are, as `(purpose, state, count)`: one
    /// for every purpose and state, none left out.
    pub fn counted(&self) -> [(&'static str, &'static str, i64); 4] {
        let counts = &self.counts;
        [
            ("operator", "live", counts.operator_live),
            ("operator", "dead", counts.operator_dead),
            ("subscriber", "live", counts.subscriber_live),
            ("subscriber", "dead", counts.subscriber_dead),
        ]
    }
}

/// Each notifier in `config` that names a Slack connection, with the
/// name; `None` when there is no Slack to describe: no `[slack]` and no
/// notifier naming a connection. With `Some`, the Slack connections are
/// read for every [`Reading`].
pub fn slack_names(config: &Config) -> Option<Vec<(String, String)>> {
    let names: Vec<(String, String)> = config
        .notifiers
        .iter()
        .filter_map(|notifier| {
            let name = notifier.slack.as_ref()?.connection.as_ref()?;
            Some((notifier.id.as_str().to_owned(), name.as_str().to_owned()))
        })
        .collect();
    (config.slack.is_some() || !names.is_empty()).then_some(names)
}

/// One check's or heartbeat's part of a [`Reading`].
#[derive(Debug, Clone)]
pub struct MemberReading {
    /// Its id.
    pub id: String,
    /// A check or a heartbeat.
    pub kind: MemberKind,
    /// Its component's id, or empty.
    pub component: String,
    /// A check's type, as configured; `None` for a heartbeat.
    pub check_type: Option<CheckType>,
    /// Its confirmed health; `None` until confirmed.
    pub health: Option<Health>,
    /// When a check's latest probe started, unix seconds.
    pub last_run: Option<i64>,
    /// When the heartbeat with its id was last pinged, unix seconds.
    pub last_ping: Option<i64>,
    /// How long a check's probes took.
    pub duration: Option<HistogramSnapshot>,
    /// The results it recorded.
    pub results: Option<ResultCounts>,
}

/// The results one check or heartbeat recorded, by health.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultCounts {
    /// Results that were up, degraded and down, in [`Health::ALL`]'s order.
    pub counts: [u64; 3],
    /// When counting began.
    pub since: SystemTime,
}

impl ResultCounts {
    fn new() -> Self {
        ResultCounts {
            counts: [0; 3],
            since: SystemTime::now(),
        }
    }

    fn count(&mut self, health: Health) {
        let index = Health::ALL
            .iter()
            .position(|each| *each == health)
            .expect("ALL holds every health");
        self.counts[index] += 1;
    }
}

/// A histogram of how long probes took, in seconds: prometheus-client's
/// classic `Histogram`, observed and written to `/metrics` the same way,
/// but whose buckets can be read, which the OpenTelemetry export needs and
/// prometheus-client keeps private.
#[derive(Debug)]
pub struct DurationHistogram {
    inner: Mutex<HistogramSnapshot>,
}

impl DurationHistogram {
    /// An empty histogram over [`DURATION_BUCKETS`], starting now.
    fn new() -> Self {
        DurationHistogram {
            inner: Mutex::new(HistogramSnapshot {
                sum: 0.0,
                count: 0,
                buckets: DURATION_BUCKETS
                    .into_iter()
                    .chain(std::iter::once(f64::MAX))
                    .map(|upper| (upper, 0))
                    .collect(),
                since: SystemTime::now(),
            }),
        }
    }

    /// Counts `value` in the first bucket that holds it; NaN in the last.
    fn observe(&self, value: f64) {
        let mut inner = lock(&self.inner);
        inner.sum += value;
        inner.count += 1;
        let bucket = if value.is_nan() {
            inner.buckets.last_mut()
        } else {
            inner.buckets.iter_mut().find(|(upper, _)| *upper >= value)
        };
        if let Some((_, count)) = bucket {
            *count += 1;
        }
    }

    /// Its state now.
    fn snapshot(&self) -> HistogramSnapshot {
        lock(&self.inner).clone()
    }
}

impl TypedMetric for DurationHistogram {
    const TYPE: MetricType = MetricType::Histogram;
}

impl EncodeMetric for DurationHistogram {
    fn encode(&self, mut encoder: MetricEncoder) -> std::fmt::Result {
        let snapshot = self.snapshot();
        encoder.encode_histogram::<NoLabelSet>(
            snapshot.sum,
            snapshot.count,
            &snapshot.buckets,
            None,
        )
    }

    fn metric_type(&self) -> MetricType {
        Self::TYPE
    }
}

/// A [`DurationHistogram`]'s state at one moment.
#[derive(Debug, Clone, PartialEq)]
pub struct HistogramSnapshot {
    /// The sum of every value observed.
    pub sum: f64,
    /// How many values were observed.
    pub count: u64,
    /// Each bucket's upper bound and how many values fell in it, not
    /// counting those in the buckets below: [`DURATION_BUCKETS`], then
    /// `f64::MAX` for everything above them.
    pub buckets: Vec<(f64, u64)>,
    /// When the histogram began counting.
    pub since: SystemTime,
}

/// How the OpenTelemetry export goes:
///
/// - `tocsin_otlp_export_failures_total`, OTLP metric exports that failed:
///   the collector could not be reached, did not answer within the
///   timeout, or answered with an error. Absent unless `[telemetry.otlp]`
///   is on; 0 from the moment its exporter starts.
///
/// Cheap to clone; clones share the series.
#[derive(Debug, Clone, Default)]
pub struct ExportMetrics {
    failures: StartedCounter,
}

impl ExportMetrics {
    fn register(registry: &mut Registry) -> Self {
        let metrics = ExportMetrics::default();
        registry.register(
            "tocsin_otlp_export_failures",
            "OTLP metric exports that failed: the collector was unreachable, timed out or answered with an error",
            metrics.failures.clone(),
        );
        metrics
    }

    /// An exporter started: the count is written from now on, at 0.
    pub fn started(&self) {
        self.failures.started.store(true, Ordering::Relaxed);
    }

    /// An export failed.
    pub fn failed(&self) {
        self.failures.counter.inc();
    }

    /// How many exports failed.
    #[cfg(test)]
    pub fn failures(&self) -> u64 {
        self.failures.counter.get()
    }
}

/// A counter with no labels that is not written, not even its `# HELP`,
/// until it is started.
#[derive(Debug, Clone, Default)]
struct StartedCounter {
    started: Arc<AtomicBool>,
    counter: Counter,
}

impl TypedMetric for StartedCounter {
    const TYPE: MetricType = MetricType::Counter;
}

impl EncodeMetric for StartedCounter {
    fn encode(&self, encoder: MetricEncoder) -> std::fmt::Result {
        self.counter.encode(encoder)
    }

    fn metric_type(&self) -> MetricType {
        Self::TYPE
    }

    fn is_empty(&self) -> bool {
        !self.started.load(Ordering::Relaxed)
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

/// A [`History`] that counts each result `inner` stores in `metrics`, by
/// health: every probe's, every ping's and every missed heartbeat
/// deadline's, since they all are stored through it.
pub(crate) struct Counted {
    inner: Arc<dyn History>,
    metrics: CheckMetrics,
}

impl Counted {
    /// Stores through `inner`, counting in `metrics`.
    pub(crate) fn new(inner: Arc<dyn History>, metrics: CheckMetrics) -> Self {
        Counted { inner, metrics }
    }
}

impl History for Counted {
    fn record(
        &self,
        result: NewCheckResult,
        kind: SubjectKind,
        change: Option<OutageChange>,
    ) -> BoxFuture<'_, StoreResult<Option<i64>>> {
        Box::pin(async move {
            let member = match kind {
                SubjectKind::Check => MemberKind::Check,
                SubjectKind::Heartbeat => MemberKind::Heartbeat,
            };
            let (id, health) = (result.check_id.clone(), result.health);
            let stored = self.inner.record(result, kind, change).await?;
            self.metrics.recorded(member, &id, health);
            Ok(stored)
        })
    }

    fn open_outages<'a>(&'a self, node_id: &'a str) -> BoxFuture<'a, StoreResult<Vec<Outage>>> {
        self.inner.open_outages(node_id)
    }

    fn last_up<'a>(
        &'a self,
        node_id: &'a str,
        check_ids: Vec<String>,
    ) -> BoxFuture<'a, StoreResult<Vec<LastUp>>> {
        self.inner.last_up(node_id, check_ids)
    }

    fn latest_results<'a>(
        &'a self,
        node_id: &'a str,
        check_ids: Vec<String>,
    ) -> BoxFuture<'a, StoreResult<Vec<LatestResult>>> {
        self.inner.latest_results(node_id, check_ids)
    }

    fn withdraw(&self, outage: i64, at_ms: i64) -> BoxFuture<'_, StoreResult<()>> {
        self.inner.withdraw(outage, at_ms)
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
