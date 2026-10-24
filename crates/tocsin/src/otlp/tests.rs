// SPDX-License-Identifier: AGPL-3.0-only

//! The OTLP export against a fake collector in process: what it sends,
//! the Slack connections as `/metrics` writes them, that a failed export
//! is counted and logged without its secrets, that
//! probes keep their pace while the collector hangs, and how the settings
//! are taken from the file and the `OTEL_*` variables.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::http::HeaderMap as RequestHeaders;
use axum::routing::post;
use opentelemetry_proto::tonic::collector::metrics::v1::{
    ExportMetricsPartialSuccess, ExportMetricsServiceRequest, ExportMetricsServiceResponse,
};
use opentelemetry_proto::tonic::common::v1::{KeyValue, any_value};
use opentelemetry_proto::tonic::metrics::v1::{Metric, metric, number_data_point};
use prost::Message as _;
use tocsin_core::Health::{Degraded, Down, Up};
use tocsin_core::config::{Config, Environment, OtlpProtocol, Source};
use tocsin_core::state::{ComponentStatus, Confirmed};
use tocsin_engine::ports::BoxFuture;
use tocsin_engine::{CheckSnapshot, ComponentSnapshot, LastResult, MemberKind, Snapshot};
use tocsin_store::SlackCounts;
use tokio::sync::mpsc;

use super::{Exporter, Prepared, Settings, SlackSource};
use crate::metrics::{Metrics, SlackCensus};
use crate::testing::Captured;

const NODE: &str = "node-a";
const TOKEN: &str = "otlp-secret-0123456789";

const CHECKS: &str = r#"
[server]
node_id = "node-a"

[[component]]
id = "api"
name = "API"

[[component]]
id = "jobs"
name = "Jobs"

[[check]]
id = "api-https"
component = "api"
type = "http"
target = "https://api.internal.example.com/health"
interval = "30s"

[[check]]
id = "db"
type = "tcp"
target = "db.internal.example.com:5432"
interval = "1m"

[[heartbeat]]
id = "backup"
component = "jobs"
interval = "1d"
"#;

/// `CHECKS` with `[telemetry.otlp]` sending to `endpoint` with a secret
/// header.
fn exporting(endpoint: &str) -> String {
    format!(
        "{CHECKS}\n[telemetry.otlp]\nendpoint = \"{endpoint}\"\n\
         headers = {{ authorization = \"${{ENV:OTLP_TOKEN}}\" }}\n\
         resource = {{ \"deployment.environment.name\" = \"test\" }}\n"
    )
}

fn load(text: &str, variable: &dyn Fn(&str) -> Option<String>) -> Config {
    tocsin_core::config::load(
        &[Source {
            name: "tocsin.toml".into(),
            text: Ok(text.into()),
        }],
        &Environment {
            hostname: "test".into(),
            variable,
        },
    )
    .unwrap()
}

fn token(name: &str) -> Option<String> {
    (name == "OTLP_TOKEN").then(|| TOKEN.to_owned())
}

/// The API down, `db` degraded but not yet confirmed, the heartbeat up.
fn snapshot() -> Snapshot {
    let check = |id: &str, kind, component: Option<&str>, health, at| CheckSnapshot {
        id: id.into(),
        kind,
        component_id: component.map(Into::into),
        state: (health != Degraded).then_some(Confirmed {
            health,
            since_ms: 1_760_000_000_000,
        }),
        last: Some(LastResult {
            health,
            started_at_ms: at,
        }),
    };
    Snapshot {
        checks: vec![
            check(
                "api-https",
                MemberKind::Check,
                Some("api"),
                Down,
                1_760_000_060_500,
            ),
            check("db", MemberKind::Check, None, Degraded, 1_760_000_061_000),
            check(
                "backup",
                MemberKind::Heartbeat,
                Some("jobs"),
                Up,
                1_760_000_030_000,
            ),
        ],
        components: vec![
            ComponentSnapshot {
                id: "api".into(),
                health: Some(Down),
                status: ComponentStatus::Down,
            },
            ComponentSnapshot {
                id: "jobs".into(),
                health: Some(Up),
                status: ComponentStatus::Operational,
            },
        ],
    }
}

/// Metrics for `config` after a probe of each check, a ping and the
/// results they stored.
fn metrics(config: &Config) -> Arc<Metrics> {
    let metrics = Arc::new(Metrics::new());
    metrics.configured(config);
    for millis in [120, 480, 2_600] {
        metrics.checks.observe(
            "api-https",
            Some("api"),
            NODE,
            Duration::from_millis(millis),
        );
    }
    metrics
        .checks
        .observe("db", None, NODE, Duration::from_millis(4));
    metrics.checks.pinged("backup", 1_760_000_030_250);
    metrics
        .checks
        .recorded(MemberKind::Check, "api-https", Down);
    metrics.checks.recorded(MemberKind::Check, "api-https", Up);
    metrics.checks.recorded(MemberKind::Heartbeat, "backup", Up);
    metrics
}

/// A [`SlackSource`] answering every read with what it holds.
struct FakeSlack(Result<SlackCensus, String>);

impl SlackSource for FakeSlack {
    fn census(&self) -> BoxFuture<'_, Result<SlackCensus, String>> {
        Box::pin(async move { self.0.clone() })
    }
}

/// Slack for a configuration with none to describe: a read fails the
/// export, so a test of one that reads it anyway fails.
fn no_slack() -> Arc<FakeSlack> {
    Arc::new(FakeSlack(Err("read with no Slack to describe".to_owned())))
}

/// The export `config` turns on, sending every `interval` instead of the
/// file's, which may not go below a second.
fn start(config: &Config, metrics: &Arc<Metrics>, interval: Duration) -> Exporter {
    start_reading(config, metrics, interval, no_slack())
}

/// [`start`], reading the Slack connections from `slack`.
fn start_reading(
    config: &Config,
    metrics: &Arc<Metrics>,
    interval: Duration,
    slack: Arc<FakeSlack>,
) -> Exporter {
    let mut prepared = Prepared::new(config, &|_| None).unwrap().unwrap();
    prepared.settings.interval = interval;
    prepared.settings.timeout = interval.min(prepared.settings.timeout);
    prepared.start(Arc::clone(metrics), Arc::new(snapshot()), slack)
}

/// One request a [`Collector`] received.
struct Received {
    headers: RequestHeaders,
    request: ExportMetricsServiceRequest,
}

/// A fake OTLP/HTTP collector: it decodes each export and answers with
/// `response`.
struct Collector {
    address: std::net::SocketAddr,
    received: mpsc::UnboundedReceiver<Received>,
}

impl Collector {
    async fn start(response: ExportMetricsServiceResponse) -> Self {
        let (sender, received) = mpsc::unbounded_channel();
        let app = Router::new().route(
            "/v1/metrics",
            post(move |headers: RequestHeaders, body: Bytes| {
                let sender = sender.clone();
                let response = response.clone();
                async move {
                    let request = ExportMetricsServiceRequest::decode(body).unwrap();
                    let _ = sender.send(Received { headers, request });
                    (
                        [(axum::http::header::CONTENT_TYPE, "application/x-protobuf")],
                        response.encode_to_vec(),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Collector { address, received }
    }

    fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// The next export, within five seconds.
    async fn next(&mut self) -> Received {
        tokio::time::timeout(Duration::from_secs(5), self.received.recv())
            .await
            .expect("no export reached the collector within 5s")
            .unwrap()
    }
}

fn text(attributes: &[KeyValue]) -> BTreeMap<&str, &str> {
    attributes
        .iter()
        .map(|attribute| {
            let Some(any_value::Value::StringValue(value)) = attribute
                .value
                .as_ref()
                .and_then(|value| value.value.as_ref())
            else {
                panic!("{} is not a string", attribute.key);
            };
            (attribute.key.as_str(), value.as_str())
        })
        .collect()
}

fn named<'a>(request: &'a ExportMetricsServiceRequest, name: &str) -> &'a Metric {
    request.resource_metrics[0].scope_metrics[0]
        .metrics
        .iter()
        .find(|metric| metric.name == name)
        .unwrap_or_else(|| panic!("no {name}"))
}

/// The attribute sets of a gauge's or sum's points, with their values.
fn numbers(metric: &Metric) -> Vec<(BTreeMap<&str, &str>, f64)> {
    let points = match &metric.data {
        Some(metric::Data::Gauge(gauge)) => &gauge.data_points,
        Some(metric::Data::Sum(sum)) => &sum.data_points,
        other => panic!("{} is {other:?}", metric.name),
    };
    points
        .iter()
        .map(|point| {
            #[allow(clippy::cast_precision_loss)]
            let value = match point.value {
                Some(number_data_point::Value::AsDouble(value)) => value,
                Some(number_data_point::Value::AsInt(value)) => value as f64,
                None => panic!("a point of {} has no value", metric.name),
            };
            (text(&point.attributes), value)
        })
        .collect()
}

fn attributes<'a>(pairs: &[(&'a str, &'a str)]) -> BTreeMap<&'a str, &'a str> {
    pairs.iter().copied().collect()
}

/// Each export carries the resource, every check's and heartbeat's
/// attributes and the probe-duration histogram; without the exporter
/// nothing arrives and this fails.
#[tokio::test]
async fn exports_reach_the_collector_with_their_attributes_and_resource() {
    let mut collector = Collector::start(ExportMetricsServiceResponse::default()).await;
    let config = load(&exporting(&collector.url()), &token);
    let metrics = metrics(&config);
    let exporter = start(&config, &metrics, Duration::from_millis(50));
    let Received { headers, request } = collector.next().await;
    let second = collector.next().await;
    exporter.stop().await;

    assert_eq!(headers["authorization"], TOKEN);
    assert_eq!(headers["content-type"], "application/x-protobuf");
    let resource = request.resource_metrics[0].resource.as_ref().unwrap();
    assert_eq!(
        text(&resource.attributes),
        attributes(&[
            ("deployment.environment.name", "test"),
            ("service.instance.id", NODE),
            ("service.name", "tocsin"),
            ("service.version", env!("CARGO_PKG_VERSION")),
        ])
    );
    let scope = request.resource_metrics[0].scope_metrics[0]
        .scope
        .as_ref()
        .unwrap();
    assert_eq!(
        (scope.name.as_str(), scope.version.as_str()),
        ("tocsin", env!("CARGO_PKG_VERSION"))
    );

    let api = [
        ("tocsin.check.id", "api-https"),
        ("tocsin.check.kind", "check"),
        ("tocsin.check.type", "http"),
        ("tocsin.component.id", "api"),
        ("tocsin.node.id", NODE),
    ];
    let backup = [
        ("tocsin.check.id", "backup"),
        ("tocsin.check.kind", "heartbeat"),
        ("tocsin.component.id", "jobs"),
        ("tocsin.node.id", NODE),
    ];
    // `db` is not confirmed yet, so it has no health.
    assert_eq!(
        numbers(named(&request, "tocsin.check.up")),
        vec![(attributes(&api), 0.0), (attributes(&backup), 1.0)]
    );
    let db = [
        ("tocsin.check.id", "db"),
        ("tocsin.check.kind", "check"),
        ("tocsin.check.type", "tcp"),
        ("tocsin.node.id", NODE),
    ];
    assert_eq!(
        numbers(named(&request, "tocsin.check.last_run")),
        vec![
            (attributes(&api), 1_760_000_060.0),
            (attributes(&db), 1_760_000_061.0)
        ]
    );
    assert_eq!(
        numbers(named(&request, "tocsin.heartbeat.last_ping")),
        vec![(attributes(&backup), 1_760_000_030.0)]
    );
    let results = numbers(named(&request, "tocsin.check.results"));
    let with = |base: &[(&'static str, &'static str)], health| {
        let mut set = attributes(base);
        set.insert("tocsin.check.health", health);
        set
    };
    assert!(results.contains(&(with(&api, "down"), 1.0)), "{results:?}");
    assert!(results.contains(&(with(&api, "up"), 1.0)), "{results:?}");
    assert!(results.contains(&(with(&backup, "up"), 1.0)), "{results:?}");
    assert!(
        results.contains(&(with(&backup, "down"), 0.0)),
        "{results:?}"
    );
    let statuses = numbers(named(&request, "tocsin.component.status"));
    assert!(statuses.contains(&(
        attributes(&[
            ("tocsin.component.id", "api"),
            ("tocsin.component.status", "down"),
            ("tocsin.node.id", NODE),
        ]),
        1.0
    )));

    assert_duration(&request, &second.request, &attributes(&api));
    // No `[slack]` and no notifier naming a connection: no Slack metrics,
    // and the connections were never read, or the export would have
    // failed.
    let names: Vec<&str> = request.resource_metrics[0].scope_metrics[0]
        .metrics
        .iter()
        .map(|metric| metric.name.as_str())
        .collect();
    assert!(
        !names.iter().any(|name| name.starts_with("tocsin.slack.")),
        "{names:?}"
    );
    assert_eq!(metrics.export.failures(), 0);
}

/// Three notifiers naming Slack connections: one live, one dead and one
/// never connected.
const NAMING: &str = "[[notifier]]\nid = \"ops-slack\"\ntype = \"slack\"\n\n\
                      [notifier.slack]\nconnection = \"ops-alerts\"\n\n\
                      [[notifier]]\nid = \"old-slack\"\ntype = \"slack\"\n\n\
                      [notifier.slack]\nconnection = \"old\"\n\n\
                      [[notifier]]\nid = \"lost-slack\"\ntype = \"slack\"\n\n\
                      [notifier.slack]\nconnection = \"never-connected\"\n";

/// One live and one dead operator connection, two live subscriber ones.
fn census() -> SlackCensus {
    (
        SlackCounts {
            operator_live: 1,
            operator_dead: 1,
            subscriber_live: 2,
            subscriber_dead: 0,
        },
        vec![("ops-alerts".to_owned(), true), ("old".to_owned(), false)],
    )
}

/// With notifiers naming Slack connections, each export carries the
/// connections, with the same values `/metrics` writes from the same
/// census.
#[tokio::test]
async fn slack_connections_are_exported_as_metrics_writes_them() {
    let mut collector = Collector::start(ExportMetricsServiceResponse::default()).await;
    let config = load(
        &format!("{}\n{NAMING}", exporting(&collector.url())),
        &token,
    );
    let metrics = metrics(&config);
    let slack = Arc::new(FakeSlack(Ok(census())));
    let exporter = start_reading(&config, &metrics, Duration::from_millis(50), slack);
    let Received { request, .. } = collector.next().await;
    exporter.stop().await;

    let connections = named(&request, "tocsin.slack.connections");
    assert_eq!(connections.unit, "{connection}");
    let connections = numbers(connections);
    let up = numbers(named(&request, "tocsin.slack.connection.up"));
    let text = metrics.render(&snapshot(), &config, 0, Some(&census()));
    let mut written = 0;
    for (purpose, state, value) in [
        ("operator", "live", 1.0),
        ("operator", "dead", 1.0),
        ("subscriber", "live", 2.0),
        ("subscriber", "dead", 0.0),
    ] {
        let set = attributes(&[
            ("tocsin.slack.purpose", purpose),
            ("tocsin.slack.state", state),
        ]);
        assert!(connections.contains(&(set, value)), "{connections:?}");
        let line = format!(
            "tocsin_slack_connections{{purpose=\"{purpose}\",state=\"{state}\"}} {value}\n"
        );
        assert!(text.contains(&line), "{line} in\n{text}");
        written += 1;
    }
    assert_eq!(connections.len(), written, "{connections:?}");
    assert_eq!(
        up,
        vec![
            (
                attributes(&[
                    ("tocsin.notifier.id", "ops-slack"),
                    ("tocsin.slack.connection", "ops-alerts"),
                ]),
                1.0
            ),
            (
                attributes(&[
                    ("tocsin.notifier.id", "old-slack"),
                    ("tocsin.slack.connection", "old"),
                ]),
                0.0
            ),
            (
                attributes(&[
                    ("tocsin.notifier.id", "lost-slack"),
                    ("tocsin.slack.connection", "never-connected"),
                ]),
                0.0
            ),
        ]
    );
    for line in [
        "tocsin_slack_connection_up{notifier=\"ops-slack\",connection=\"ops-alerts\"} 1\n",
        "tocsin_slack_connection_up{notifier=\"old-slack\",connection=\"old\"} 0\n",
        "tocsin_slack_connection_up{notifier=\"lost-slack\",connection=\"never-connected\"} 0\n",
    ] {
        assert!(text.contains(line), "{line} in\n{text}");
    }
    assert_eq!(metrics.export.failures(), 0);
}

/// Connections it cannot read fail the export whole, as they fail a
/// scrape of `/metrics`: nothing is sent without them, and the failure
/// is counted and logged with why.
#[tokio::test]
async fn unreadable_slack_connections_fail_the_export() {
    let mut collector = Collector::start(ExportMetricsServiceResponse::default()).await;
    let config = load(
        &format!("{}\n{NAMING}", exporting(&collector.url())),
        &token,
    );
    let metrics = metrics(&config);
    let log = Captured::default();
    let _capturing = log.capture();
    let slack = Arc::new(FakeSlack(Err("disk I/O error".to_owned())));
    let exporter = start_reading(&config, &metrics, Duration::from_millis(50), slack);
    let deadline = Instant::now() + Duration::from_secs(5);
    while metrics.export.failures() < 2 {
        assert!(Instant::now() < deadline, "no failure counted within 5s");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    exporter.stop().await;

    assert!(collector.received.try_recv().is_err(), "an export was sent");
    let log = log.text();
    assert!(
        log.contains("cannot read the Slack connections: disk I/O error"),
        "{log}"
    );
}

/// `tocsin.check.duration` holds one cumulative histogram per probed
/// check, `api`'s with its three probes, and `second`, the next export,
/// carries it from the same start.
fn assert_duration(
    first: &ExportMetricsServiceRequest,
    second: &ExportMetricsServiceRequest,
    api: &BTreeMap<&str, &str>,
) {
    let duration = named(first, "tocsin.check.duration");
    assert_eq!(duration.unit, "s");
    let Some(metric::Data::Histogram(histogram)) = &duration.data else {
        panic!("tocsin.check.duration is {:?}", duration.data);
    };
    assert_eq!(histogram.data_points.len(), 2, "one per probed check");
    let point = histogram
        .data_points
        .iter()
        .find(|point| text(&point.attributes) == *api)
        .unwrap();
    assert_eq!(point.count, 3);
    assert_eq!(point.bucket_counts.iter().sum::<u64>(), point.count);
    assert_eq!(point.bucket_counts.len(), point.explicit_bounds.len() + 1);
    assert!((point.sum.unwrap() - 3.2).abs() < 1e-9);
    assert!(point.start_time_unix_nano > 0 && point.start_time_unix_nano <= point.time_unix_nano);

    // Cumulative: the next export carries the same start.
    let next = named(second, "tocsin.check.duration");
    let Some(metric::Data::Histogram(next)) = &next.data else {
        panic!();
    };
    assert!(next.data_points.iter().any(|later| {
        later.start_time_unix_nano == point.start_time_unix_nano && later.count == 3
    }));
}

/// An unreachable collector: every export fails, is counted and logged,
/// and the log names the endpoint but never the header's secret.
#[tokio::test]
async fn a_failed_export_is_counted_and_logged_without_its_secrets() {
    // Bound and never listening: refused, and no other test can take it.
    let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
    socket
        .bind(&std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0)).into())
        .unwrap();
    let closed = socket.local_addr().unwrap().as_socket().unwrap();
    let endpoint = format!("http://user:{TOKEN}@{closed}");
    let config = load(&exporting(&endpoint), &token);
    let metrics = metrics(&config);
    let log = Captured::default();
    let _capturing = log.capture();

    let exporter = start(&config, &metrics, Duration::from_millis(50));
    let deadline = Instant::now() + Duration::from_secs(5);
    while metrics.export.failures() < 2 {
        assert!(Instant::now() < deadline, "no failure counted within 5s");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    exporter.stop().await;

    let log = log.text();
    assert!(
        log.contains("OTLP metric export failed, counted in tocsin_otlp_export_failures_total"),
        "{log}"
    );
    assert!(
        log.contains(&format!("http://{closed}/v1/metrics")),
        "{log}"
    );
    assert!(log.contains("headers=authorization"), "{log}");
    assert!(!log.contains(TOKEN), "{log}");
}

/// A collector that takes part of an export and rejects the rest has
/// not taken it: that is a failure too.
#[tokio::test]
async fn a_partly_rejected_export_is_a_failure() {
    let mut collector = Collector::start(ExportMetricsServiceResponse {
        partial_success: Some(ExportMetricsPartialSuccess {
            rejected_data_points: 3,
            error_message: "too many attributes".into(),
        }),
    })
    .await;
    let config = load(&exporting(&collector.url()), &token);
    let metrics = metrics(&config);
    let log = Captured::default();
    let _capturing = log.capture();
    let exporter = start(&config, &metrics, Duration::from_millis(50));
    collector.next().await;
    let deadline = Instant::now() + Duration::from_secs(5);
    while metrics.export.failures() < 1 {
        assert!(Instant::now() < deadline, "no failure counted within 5s");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    exporter.stop().await;
    assert!(log.text().contains("too many attributes"), "{}", log.text());
}

/// A collector that takes the connection and never answers: each export
/// waits out its timeout and is counted, while probes, which record into
/// the same metrics on the same thread, keep their pace.
#[tokio::test]
async fn a_hanging_collector_does_not_slow_the_probes() {
    // Listening and never accepting: the connection is made and nothing
    // ever answers.
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", silent.local_addr().unwrap());
    let config = load(&exporting(&endpoint), &token);
    let metrics = metrics(&config);
    let mut prepared = Prepared::new(&config, &|_| None).unwrap().unwrap();
    prepared.settings.interval = Duration::from_millis(200);
    prepared.settings.timeout = Duration::from_millis(150);
    let exporter = prepared.start(Arc::clone(&metrics), Arc::new(snapshot()), no_slack());

    // Stand-ins for probes: each records a duration and a result, then
    // waits for its next run, as the engine's do.
    let pace = Duration::from_millis(10);
    let runs = 100;
    let started = Instant::now();
    let mut slowest = Duration::ZERO;
    for _ in 0..runs {
        let run = Instant::now();
        metrics
            .checks
            .observe("db", None, NODE, Duration::from_millis(4));
        metrics.checks.recorded(MemberKind::Check, "db", Up);
        tokio::time::sleep(pace).await;
        slowest = slowest.max(run.elapsed());
    }
    let took = started.elapsed();
    let failures = metrics.export.failures();
    exporter.stop().await;

    assert!(failures >= 3, "{failures} timeouts counted in {took:?}");
    // Each run is a 10ms sleep; a run that waited on an export would take
    // the export's 150ms timeout.
    assert!(
        slowest < Duration::from_millis(100),
        "a run took {slowest:?}"
    );
    assert!(took < Duration::from_secs(4), "{runs} runs took {took:?}");
}

/// Without `[telemetry.otlp]` there is no exporter, whatever the
/// environment says: the variables only fill in the table.
#[test]
fn without_the_table_nothing_is_exported() {
    let environment = |name: &str| match name {
        "OTEL_EXPORTER_OTLP_ENDPOINT" => Some("http://127.0.0.1:4318".to_owned()),
        "OTEL_EXPORTER_OTLP_PROTOCOL" => Some("http/protobuf".to_owned()),
        "OTEL_METRIC_EXPORT_INTERVAL" => Some("1000".to_owned()),
        _ => None,
    };
    let config = load(CHECKS, &environment);
    assert!(Prepared::new(&config, &environment).unwrap().is_none());
    assert_eq!(Settings::resolve(&config, &environment), Ok(None));
    assert_eq!(super::check(&config, &environment), Ok(()));
}

fn resolved(table: &str, environment: &[(&str, &str)]) -> Result<Settings, String> {
    let variable = |name: &str| {
        environment
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| (*value).to_owned())
            .or_else(|| token(name))
    };
    let config = load(&format!("{CHECKS}\n[telemetry.otlp]\n{table}\n"), &variable);
    Settings::resolve(&config, &variable).map(Option::unwrap)
}

#[test]
fn an_empty_table_takes_the_defaults() {
    let settings = resolved("", &[]).unwrap();
    assert_eq!(
        settings.endpoint.as_str(),
        "http://localhost:4318/v1/metrics"
    );
    assert_eq!(settings.protocol, OtlpProtocol::HttpProtobuf);
    assert_eq!(settings.interval, Duration::from_secs(60));
    assert_eq!(settings.timeout, Duration::from_secs(10));
    assert!(settings.headers.is_empty());
    assert_eq!(settings.resource["service.name"], "tocsin");
}

#[test]
fn the_variables_fill_in_what_the_table_leaves_out() {
    let settings = resolved(
        "",
        &[
            (
                "OTEL_EXPORTER_OTLP_ENDPOINT",
                "https://collector.example.com:4318/otlp/",
            ),
            ("OTEL_EXPORTER_OTLP_HEADERS", "x-team=ops,api-key=generic"),
            ("OTEL_EXPORTER_OTLP_METRICS_HEADERS", "api-key=per%20signal"),
            ("OTEL_METRIC_EXPORT_INTERVAL", "15000"),
            ("OTEL_EXPORTER_OTLP_TIMEOUT", "3000"),
            ("OTEL_EXPORTER_OTLP_METRICS_TIMEOUT", "2000"),
            (
                "OTEL_RESOURCE_ATTRIBUTES",
                "service.name=from-attributes,host.name=box,service.instance.id=other",
            ),
            ("OTEL_SERVICE_NAME", "from-service-name"),
        ],
    )
    .unwrap();
    // The generic endpoint is a base: the signal's path is added.
    assert_eq!(
        settings.endpoint.as_str(),
        "https://collector.example.com:4318/otlp/v1/metrics"
    );
    assert_eq!(settings.headers["x-team"], "ops");
    assert_eq!(settings.headers["api-key"], "per signal");
    assert_eq!(settings.interval, Duration::from_secs(15));
    assert_eq!(settings.timeout, Duration::from_secs(2));
    assert_eq!(settings.resource["service.name"], "from-service-name");
    assert_eq!(settings.resource["host.name"], "box");
    // tocsin's own win over the variables.
    assert_eq!(settings.resource["service.instance.id"], NODE);
    assert_eq!(
        settings.resource["service.version"],
        env!("CARGO_PKG_VERSION")
    );

    // The per-signal endpoint is the whole URL, used as is.
    let settings = resolved(
        "",
        &[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://ignored:4318"),
            (
                "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
                "http://collector:9000/custom",
            ),
        ],
    )
    .unwrap();
    assert_eq!(settings.endpoint.as_str(), "http://collector:9000/custom");
}

#[test]
fn the_table_wins_over_the_variables() {
    let settings = resolved(
        "endpoint = \"http://file:4318\"\nprotocol = \"http/protobuf\"\n\
         interval = \"30s\"\ntimeout = \"5s\"\n\
         headers = { api-key = \"${ENV:OTLP_TOKEN}\" }\n\
         resource = { \"service.name\" = \"from-file\" }",
        &[
            (
                "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
                "http://variable:4318/v1/metrics",
            ),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
            ("OTEL_EXPORTER_OTLP_HEADERS", "api-key=from-variable"),
            ("OTEL_METRIC_EXPORT_INTERVAL", "1000"),
            ("OTEL_EXPORTER_OTLP_TIMEOUT", "1000"),
            ("OTEL_SERVICE_NAME", "from-variable"),
        ],
    )
    .unwrap();
    assert_eq!(settings.endpoint.as_str(), "http://file:4318/v1/metrics");
    assert_eq!(settings.protocol, OtlpProtocol::HttpProtobuf);
    assert_eq!(settings.interval, Duration::from_secs(30));
    assert_eq!(settings.timeout, Duration::from_secs(5));
    assert_eq!(settings.headers["api-key"], TOKEN);
    assert_eq!(settings.resource["service.name"], "from-file");
}

#[test]
fn bad_variables_are_named_without_their_secrets() {
    for (variable, value, expected) in [
        (
            "OTEL_EXPORTER_OTLP_PROTOCOL",
            "http/json",
            "OTEL_EXPORTER_OTLP_PROTOCOL",
        ),
        (
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            "collector:4318",
            "OTEL_EXPORTER_OTLP_ENDPOINT",
        ),
        ("OTEL_METRIC_EXPORT_INTERVAL", "500", "at least 1000"),
        (
            "OTEL_EXPORTER_OTLP_TIMEOUT",
            "soon",
            "OTEL_EXPORTER_OTLP_TIMEOUT",
        ),
        (
            "OTEL_EXPORTER_OTLP_HEADERS",
            "api-key=otlp-secret-0123456789%0A",
            "the value of header `api-key` in OTEL_EXPORTER_OTLP_HEADERS",
        ),
    ] {
        let error = resolved("", &[(variable, value)]).unwrap_err();
        assert!(error.contains(expected), "{variable}: {error}");
        assert!(!error.contains(TOKEN), "{variable}: {error}");
    }
    let error = resolved(
        "interval = \"5s\"",
        &[("OTEL_EXPORTER_OTLP_TIMEOUT", "9000")],
    )
    .unwrap_err();
    assert!(
        error.contains("must not be longer than the interval"),
        "{error}"
    );
}

#[cfg(not(feature = "grpc"))]
#[test]
fn grpc_needs_the_feature() {
    let error = resolved("protocol = \"grpc\"", &[]).unwrap_err();
    assert!(error.contains("`grpc` feature"), "{error}");
}

#[cfg(feature = "grpc")]
#[test]
fn grpc_sends_to_the_base_url() {
    let settings = resolved("protocol = \"grpc\"", &[]).unwrap();
    assert_eq!(settings.endpoint.as_str(), "http://localhost:4317/");
}

/// The settings' `Debug`, as a panic or a debug log would show them,
/// redacts the header values.
#[test]
fn the_settings_never_show_a_header_value() {
    let settings = resolved("headers = { api-key = \"${ENV:OTLP_TOKEN}\" }", &[]).unwrap();
    let shown = format!("{settings:?}");
    assert!(shown.contains("api-key"), "{shown}");
    assert!(!shown.contains(TOKEN), "{shown}");
}

/// Over gRPC, an export reaches a collector as a `MetricsService/Export`
/// call carrying the headers as metadata.
#[cfg(feature = "grpc")]
#[tokio::test]
async fn grpc_exports_reach_the_collector() {
    use opentelemetry_proto::tonic::collector::metrics::v1::metrics_service_server::{
        MetricsService, MetricsServiceServer,
    };

    struct Fake(mpsc::UnboundedSender<(String, ExportMetricsServiceRequest)>);

    #[tonic::async_trait]
    impl MetricsService for Fake {
        async fn export(
            &self,
            request: tonic::Request<ExportMetricsServiceRequest>,
        ) -> Result<tonic::Response<ExportMetricsServiceResponse>, tonic::Status> {
            let authorization = request
                .metadata()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            let _ = self.0.send((authorization, request.into_inner()));
            Ok(tonic::Response::new(ExportMetricsServiceResponse::default()))
        }
    }

    let (sender, mut received) = mpsc::unbounded_channel();
    let app =
        tonic::service::Routes::new(MetricsServiceServer::new(Fake(sender))).into_axum_router();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let config = load(
        &format!(
            "{}protocol = \"grpc\"\n",
            exporting(&format!("http://{address}"))
        ),
        &token,
    );
    let metrics = metrics(&config);
    let exporter = start(&config, &metrics, Duration::from_millis(50));
    let (authorization, request) = tokio::time::timeout(Duration::from_secs(5), received.recv())
        .await
        .expect("no export reached the collector within 5s")
        .unwrap();
    exporter.stop().await;
    assert_eq!(authorization, TOKEN);
    let resource = request.resource_metrics[0].resource.as_ref().unwrap();
    assert_eq!(text(&resource.attributes)["service.instance.id"], NODE);
    assert!(named(&request, "tocsin.check.duration").data.is_some());
    assert_eq!(metrics.export.failures(), 0);
}
