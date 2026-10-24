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
use axum::http::StatusCode;
use axum::routing::post;
use opentelemetry_proto::tonic::collector::metrics::v1::{
    ExportMetricsPartialSuccess, ExportMetricsServiceRequest, ExportMetricsServiceResponse,
};
use opentelemetry_proto::tonic::common::v1::{KeyValue, any_value};
use opentelemetry_proto::tonic::metrics::v1::{Metric, metric, number_data_point};
use prost::Message as _;
use tocsin_alert::{Backoff, Jitter};
use tocsin_core::Health::{Degraded, Down, Up};
use tocsin_core::config::{Config, Environment, OtlpProtocol, Source};
use tocsin_core::state::{ComponentStatus, Confirmed};
use tocsin_engine::ports::BoxFuture;
use tocsin_engine::{CheckSnapshot, ComponentSnapshot, LastResult, MemberKind, Snapshot};
use tocsin_store::SlackCounts;
use tokio::sync::{mpsc, oneshot};

use super::{Exporter, Failed, Prepared, Retry, Settings, SlackSource};
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

/// At shutdown the readings since the last export are sent once more:
/// with an hour between exports, the only one is the last, and it
/// carries a result recorded after the exporter started.
#[tokio::test]
async fn the_last_readings_are_sent_at_shutdown() {
    let mut collector = Collector::start(ExportMetricsServiceResponse::default()).await;
    let config = load(&exporting(&collector.url()), &token);
    let metrics = metrics(&config);
    let exporter = start(&config, &metrics, Duration::from_secs(3600));
    metrics
        .checks
        .recorded(MemberKind::Check, "api-https", Down);
    exporter.stop().await;

    let Received { request, .. } = collector.next().await;
    let down = attributes(&[
        ("tocsin.check.health", "down"),
        ("tocsin.check.id", "api-https"),
        ("tocsin.check.kind", "check"),
        ("tocsin.check.type", "http"),
        ("tocsin.component.id", "api"),
        ("tocsin.node.id", NODE),
    ]);
    let results = numbers(named(&request, "tocsin.check.results"));
    assert!(results.contains(&(down, 2.0)), "{results:?}");
    assert!(collector.received.try_recv().is_err(), "a second export");
    assert_eq!(metrics.export.failures(), 0);
}

/// The last export waits on a silent collector no longer than the
/// timeout, and its failure is counted like any other.
#[tokio::test]
async fn the_last_export_ends_within_the_timeout() {
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let config = load(
        &exporting(&format!("http://{}", silent.local_addr().unwrap())),
        &token,
    );
    let metrics = metrics(&config);
    let mut prepared = Prepared::new(&config, &|_| None).unwrap().unwrap();
    prepared.settings.interval = Duration::from_secs(3600);
    prepared.settings.timeout = Duration::from_millis(200);
    let exporter = prepared.start(Arc::clone(&metrics), Arc::new(snapshot()), no_slack());
    let log = Captured::default();
    let _capturing = log.capture();

    tokio::time::timeout(Duration::from_secs(5), exporter.stop())
        .await
        .expect("the last export outlasted its timeout");
    assert_eq!(metrics.export.failures(), 1);
    assert!(
        log.text().contains("it was the last, at shutdown"),
        "{}",
        log.text()
    );
}

/// An exported request held by a [`Gate`] until the test answers it,
/// with a status and headers.
type Held = (
    ExportMetricsServiceRequest,
    oneshot::Sender<(StatusCode, RequestHeaders)>,
);

/// `status`, with no headers but the content type.
fn plain(status: StatusCode) -> (StatusCode, RequestHeaders) {
    (status, RequestHeaders::new())
}

/// A fake OTLP/HTTP collector that hands each export to the test and
/// answers with the status and headers the test sends back.
struct Gate {
    address: std::net::SocketAddr,
    held: mpsc::UnboundedReceiver<Held>,
}

impl Gate {
    async fn start() -> Self {
        let (sender, held) = mpsc::unbounded_channel();
        let app = Router::new().route(
            "/v1/metrics",
            post(move |body: Bytes| {
                let sender = sender.clone();
                async move {
                    let request = ExportMetricsServiceRequest::decode(body).unwrap();
                    let (answer, answered) = oneshot::channel();
                    let _ = sender.send((request, answer));
                    let (status, headers) = answered
                        .await
                        .unwrap_or_else(|_| plain(StatusCode::INTERNAL_SERVER_ERROR));
                    (
                        status,
                        headers,
                        [(axum::http::header::CONTENT_TYPE, "application/x-protobuf")],
                        ExportMetricsServiceResponse::default().encode_to_vec(),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Gate { address, held }
    }

    /// The next export, within five seconds.
    async fn next(&mut self) -> Held {
        tokio::time::timeout(Duration::from_secs(5), self.held.recv())
            .await
            .expect("no export reached the collector within 5s")
            .unwrap()
    }
}

/// Stand-ins for probes, as in [`a_hanging_collector_does_not_slow_the_probes`]:
/// the slowest of `runs` runs, each recording into `metrics` and sleeping
/// 10ms.
async fn probe(metrics: &Metrics, runs: usize) -> Duration {
    let mut slowest = Duration::ZERO;
    for _ in 0..runs {
        let run = Instant::now();
        metrics
            .checks
            .observe("db", None, NODE, Duration::from_millis(4));
        metrics.checks.recorded(MemberKind::Check, "db", Up);
        tokio::time::sleep(Duration::from_millis(10)).await;
        slowest = slowest.max(run.elapsed());
    }
    slowest
}

/// A collector answering 503 and then 200 gets the same export twice and
/// takes it once; the refused attempt is counted, and probes keep their
/// pace while each attempt is held.
#[tokio::test]
async fn an_unavailable_collector_is_retried_and_counted() {
    let mut gate = Gate::start().await;
    let config = load(&exporting(&format!("http://{}", gate.address)), &token);
    let metrics = metrics(&config);
    let mut prepared = Prepared::new(&config, &|_| None).unwrap().unwrap();
    prepared.settings.interval = Duration::from_secs(1);
    prepared.settings.timeout = Duration::from_millis(500);
    prepared.settings.backoff = Backoff {
        base: Duration::from_millis(1),
        cap: Duration::from_millis(1),
        jitter: Jitter::None,
    };
    let exporter = prepared.start(Arc::clone(&metrics), Arc::new(snapshot()), no_slack());

    let (first, answer) = gate.next().await;
    let mut slowest = probe(&metrics, 5).await;
    answer.send(plain(StatusCode::SERVICE_UNAVAILABLE)).unwrap();
    let (retried, answer) = gate.next().await;
    slowest = slowest.max(probe(&metrics, 5).await);
    answer.send(plain(StatusCode::OK)).unwrap();
    assert_eq!(retried, first, "the retry sent a different export");

    // Everything after is answered 200: the last export, at shutdown.
    let mut stopping = tokio::spawn(exporter.stop());
    let mut later = Vec::new();
    loop {
        tokio::select! {
            stopped = &mut stopping => break stopped.unwrap(),
            Some((request, answer)) = gate.held.recv() => {
                let _ = answer.send(plain(StatusCode::OK));
                later.push(request);
            }
        }
    }
    assert_ne!(later.len(), 0, "nothing was sent at shutdown");
    assert!(later.iter().all(|request| *request != first));
    assert_eq!(metrics.export.failures(), 1);
    assert!(
        slowest < Duration::from_millis(100),
        "a probe run took {slowest:?}"
    );
}

/// A 503 with `Retry-After` is retried after the wait the collector
/// asked for, not the backoff.
#[tokio::test]
async fn retry_after_is_honoured() {
    let mut gate = Gate::start().await;
    let config = load(&exporting(&format!("http://{}", gate.address)), &token);
    let metrics = metrics(&config);
    let mut prepared = Prepared::new(&config, &|_| None).unwrap().unwrap();
    prepared.settings.interval = Duration::from_secs(2);
    prepared.settings.timeout = Duration::from_millis(500);
    prepared.settings.backoff = Backoff {
        base: Duration::from_millis(1),
        cap: Duration::from_millis(1),
        jitter: Jitter::None,
    };
    let log = Captured::default();
    let _capturing = log.capture();
    let exporter = prepared.start(Arc::clone(&metrics), Arc::new(snapshot()), no_slack());

    let (first, answer) = gate.next().await;
    let mut headers = RequestHeaders::new();
    headers.insert(axum::http::header::RETRY_AFTER, "1".parse().unwrap());
    answer
        .send((StatusCode::SERVICE_UNAVAILABLE, headers))
        .unwrap();
    let refused = Instant::now();
    let (retried, answer) = gate.next().await;
    let waited = refused.elapsed();
    answer.send(plain(StatusCode::OK)).unwrap();
    assert_eq!(retried, first);

    let mut stopping = tokio::spawn(exporter.stop());
    loop {
        tokio::select! {
            stopped = &mut stopping => break stopped.unwrap(),
            Some((_, answer)) = gate.held.recv() => {
                let _ = answer.send(plain(StatusCode::OK));
            }
        }
    }
    assert!(
        waited >= Duration::from_millis(900),
        "retried after {waited:?}"
    );
    assert!(log.text().contains("; retrying in 1s"), "{}", log.text());
    assert_eq!(metrics.export.failures(), 1);
}

/// Settings for [`delivered`]: an export every minute, attempts timing
/// out after 10s, and retries 1s, 2s, 4s... apart, without jitter.
fn retrying() -> Settings {
    let mut settings = resolved("interval = \"60s\"\ntimeout = \"10s\"", &[]).unwrap();
    settings.backoff = Backoff {
        base: Duration::from_secs(1),
        cap: settings.interval,
        jitter: Jitter::None,
    };
    settings
}

/// Delivers through attempts that fail as `failing` says until the
/// `succeeds`th, if any, on a paused clock: when each attempt was made,
/// from the start, and what was counted.
async fn delivered(
    settings: &Settings,
    failing: &Failed,
    succeeds: Option<usize>,
) -> (Vec<Duration>, Vec<String>) {
    let start = tokio::time::Instant::now();
    let mut made = Vec::new();
    let mut counted = Vec::new();
    super::deliver(
        settings,
        start,
        || {
            made.push(start.elapsed());
            let outcome = if Some(made.len()) == succeeds {
                Ok(())
            } else {
                Err(failing.clone())
            };
            std::future::ready(outcome)
        },
        |message| counted.push(message.to_owned()),
    )
    .await;
    (made, counted)
}

fn unavailable(retry: Retry) -> Failed {
    Failed {
        reason: "the collector answered 503 Service Unavailable".into(),
        retry,
    }
}

/// Retries back off, doubling, and stop once the next could not end,
/// even timing out, before the next export is due; each failed attempt
/// is counted.
#[tokio::test(start_paused = true)]
async fn retries_back_off_within_the_interval() {
    let seconds = |list: &[u64]| {
        list.iter()
            .map(|s| Duration::from_secs(*s))
            .collect::<Vec<_>>()
    };
    let (made, counted) = delivered(&retrying(), &unavailable(Retry::Backoff), None).await;
    // The next, 32s after the sixth, would end at 73s, past 60s.
    assert_eq!(made, seconds(&[0, 1, 3, 7, 15, 31]));
    assert_eq!(counted.len(), 6, "{counted:?}");
    assert!(counted[0].ends_with("; retrying in 1s"), "{counted:?}");
    assert!(
        counted[5].ends_with(
            "; not retried, as the retry 32s from now could not end before the next export"
        ),
        "{counted:?}"
    );

    // The collector's wait is taken over the backoff.
    let after = unavailable(Retry::After(Duration::from_secs(20)));
    let (made, counted) = delivered(&retrying(), &after, None).await;
    assert_eq!(made, seconds(&[0, 20, 40]));
    assert_eq!(counted.len(), 3);
    for wait in [55, u64::MAX] {
        let after = unavailable(Retry::After(Duration::from_secs(wait)));
        let (made, counted) = delivered(&retrying(), &after, None).await;
        assert_eq!(made, seconds(&[0]), "Retry-After {wait}s");
        assert_eq!(counted.len(), 1);
    }

    // A success ends it.
    let (made, counted) = delivered(&retrying(), &unavailable(Retry::Backoff), Some(3)).await;
    assert_eq!(made, seconds(&[0, 1, 3]));
    assert_eq!(counted.len(), 2);

    // What a retry would not mend is not retried.
    let (made, counted) = delivered(&retrying(), &unavailable(Retry::Never), None).await;
    assert_eq!(made, seconds(&[0]));
    assert_eq!(counted, ["the collector answered 503 Service Unavailable"]);
}

/// An attempt that times out is counted and not retried.
#[tokio::test(start_paused = true)]
async fn a_timed_out_attempt_is_not_retried() {
    let settings = retrying();
    let start = tokio::time::Instant::now();
    let mut attempts = 0;
    let mut counted = Vec::new();
    super::deliver(
        &settings,
        start,
        || {
            attempts += 1;
            std::future::pending::<Result<(), Failed>>()
        },
        |message| counted.push(message.to_owned()),
    )
    .await;
    assert_eq!(attempts, 1);
    assert_eq!(counted, ["no answer within 10s"]);
    assert_eq!(start.elapsed(), Duration::from_secs(10));
}

/// Reading the state and the first attempt share one timeout, so an
/// export whose reading was slow still ends within it; a retry gets a
/// timeout of its own.
#[tokio::test(start_paused = true)]
async fn reading_and_sending_share_one_timeout() {
    let settings = retrying();
    let start = tokio::time::Instant::now();
    // The reading took 7s of the 10s.
    tokio::time::sleep(Duration::from_secs(7)).await;
    let mut counted = Vec::new();
    super::deliver(
        &settings,
        start,
        std::future::pending::<Result<(), Failed>>,
        |message| counted.push(message.to_owned()),
    )
    .await;
    assert_eq!(start.elapsed(), Duration::from_secs(10));
    assert_eq!(counted, ["no answer within 10s"]);

    let start = tokio::time::Instant::now();
    tokio::time::sleep(Duration::from_secs(7)).await;
    let mut attempts = 0;
    super::deliver(
        &settings,
        start,
        || {
            attempts += 1;
            let refused = attempts == 1;
            async move {
                if refused {
                    Err(unavailable(Retry::Backoff))
                } else {
                    std::future::pending().await
                }
            }
        },
        |_| {},
    )
    .await;
    // Refused at 7s, retried at 8s, which waits out its own 10s.
    assert_eq!(attempts, 2);
    assert_eq!(start.elapsed(), Duration::from_secs(18));
}

/// Exports `[telemetry.otlp]` with `extra` to `endpoint`, which takes no
/// export, an attempt timing out after 200ms and retries 20ms, 40ms,
/// 80ms... apart: the attempts counted and the log once the first export
/// is given up on, an interval of 1s from its start.
async fn given_up(endpoint: &str, extra: &str) -> (u64, String) {
    let config = load(
        &format!(
            "{}interval = \"1s\"\ntimeout = \"200ms\"\n{extra}",
            exporting(endpoint)
        ),
        &token,
    );
    let metrics = metrics(&config);
    let log = Captured::default();
    let _capturing = log.capture();
    let mut prepared = Prepared::new(&config, &|_| None).unwrap().unwrap();
    prepared.settings.backoff = Backoff {
        base: Duration::from_millis(20),
        cap: Duration::from_secs(1),
        jitter: Jitter::None,
    };
    let exporter = prepared.start(Arc::clone(&metrics), Arc::new(snapshot()), no_slack());
    let deadline = Instant::now() + Duration::from_secs(5);
    while !log.text().contains("could not end before the next export") {
        assert!(
            Instant::now() < deadline,
            "no export given up on within 5s: {}",
            log.text()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let failures = metrics.export.failures();
    let text = log.text();
    exporter.stop().await;
    (failures, text)
}

/// The attempts `given_up` logged: each failed one, retried and then not.
fn assert_retried(failures: u64, log: &str) {
    let retried = log.matches("; retrying in ").count() as u64;
    // Attempts at 0, 20, 60, 140, 300 and 620ms; the next, 640ms after,
    // could time out past 1s. Slow runners may fit fewer.
    assert!(retried >= 2, "{retried} retries: {log}");
    assert_eq!(failures, retried + 1, "{log}");
}

/// A collector nothing listens for: the connection is refused, and each
/// attempt is retried with backoff and counted, within the interval.
#[tokio::test]
async fn a_refused_connection_is_retried_and_counted() {
    let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
    socket
        .bind(&std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0)).into())
        .unwrap();
    let closed = socket.local_addr().unwrap().as_socket().unwrap();
    let (failures, log) = given_up(&format!("http://{closed}"), "").await;
    assert!(log.contains("Connection refused"), "{log}");
    assert_retried(failures, &log);
}

/// A collector that takes the connection and closes it unanswered: the
/// export is retried like a refused one.
#[tokio::test]
async fn a_dropped_connection_is_retried_and_counted() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            drop(stream);
        }
    });
    let (failures, log) = given_up(&format!("http://{address}"), "").await;
    assert_retried(failures, &log);
}

/// A collector whose connection is never made, its listen queue full:
/// each attempt gives up connecting after half the timeout and is
/// retried like a refused one.
#[tokio::test]
async fn a_connection_not_made_in_time_is_retried_and_counted() {
    let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
    socket
        .bind(&std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0)).into())
        .unwrap();
    socket.listen(0).unwrap();
    let full = socket.local_addr().unwrap().as_socket().unwrap();
    // Connect until one hangs: the queue is full, and the kernel drops
    // the next connection's SYN rather than refusing it.
    let mut queued = Vec::new();
    loop {
        match tokio::time::timeout(
            Duration::from_millis(100),
            tokio::net::TcpStream::connect(full),
        )
        .await
        {
            Ok(stream) => queued.push(stream.unwrap()),
            Err(_) => break,
        }
        assert!(queued.len() < 64, "the listen queue never filled");
    }
    let (failures, log) = given_up(&format!("http://{full}"), "").await;
    assert!(log.contains("(Connect)"), "{log}");
    assert_retried(failures, &log);
}

/// Over gRPC, tonic answers a refused connection `UNAVAILABLE`, which is
/// retried and counted the same way.
#[cfg(feature = "grpc")]
#[tokio::test]
async fn grpc_refused_connection_is_retried_and_counted() {
    let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
    socket
        .bind(&std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0)).into())
        .unwrap();
    let closed = socket.local_addr().unwrap().as_socket().unwrap();
    let (failures, log) = given_up(&format!("http://{closed}"), "protocol = \"grpc\"\n").await;
    assert!(log.contains("the collector answered Unavailable"), "{log}");
    assert_retried(failures, &log);
}

/// 429, 502, 503 and 504 are retried, after the collector's wait if it
/// gives one; any other refusal is not.
#[test]
fn which_http_answers_are_retried() {
    let wait = Some(Duration::from_secs(7));
    for status in [
        StatusCode::TOO_MANY_REQUESTS,
        StatusCode::BAD_GATEWAY,
        StatusCode::SERVICE_UNAVAILABLE,
        StatusCode::GATEWAY_TIMEOUT,
    ] {
        assert_eq!(super::http_retry(status, None), Retry::Backoff, "{status}");
        assert_eq!(
            super::http_retry(status, wait),
            Retry::After(Duration::from_secs(7)),
            "{status}"
        );
    }
    for status in [
        StatusCode::BAD_REQUEST,
        StatusCode::UNAUTHORIZED,
        StatusCode::NOT_FOUND,
        StatusCode::PAYLOAD_TOO_LARGE,
        StatusCode::INTERNAL_SERVER_ERROR,
        StatusCode::NOT_IMPLEMENTED,
    ] {
        assert_eq!(super::http_retry(status, wait), Retry::Never, "{status}");
    }
}

/// `OTEL_SDK_DISABLED=true` turns a configured export off without
/// reading the other variables, so a bad one does not fail the check;
/// any other value leaves it on.
#[test]
fn otel_sdk_disabled_turns_the_export_off() {
    for value in ["true", "TRUE", " True "] {
        let environment = |name: &str| match name {
            "OTEL_SDK_DISABLED" => Some(value.to_owned()),
            "OTEL_EXPORTER_OTLP_PROTOCOL" => Some("carrier-pigeon".to_owned()),
            _ => token(name),
        };
        let config = load(&exporting("http://127.0.0.1:4318"), &environment);
        assert!(super::disabled(&config, &environment), "{value:?}");
        assert!(Prepared::new(&config, &environment).unwrap().is_none());
        assert_eq!(Settings::resolve(&config, &environment), Ok(None));
        assert_eq!(super::check(&config, &environment), Ok(()));
    }
    for value in ["false", "1", "yes", ""] {
        let environment = |name: &str| match name {
            "OTEL_SDK_DISABLED" => Some(value.to_owned()),
            _ => token(name),
        };
        let config = load(&exporting("http://127.0.0.1:4318"), &environment);
        assert!(!super::disabled(&config, &environment), "{value:?}");
        assert!(Prepared::new(&config, &environment).unwrap().is_some());
    }
    // Without the table there is nothing to turn off, and nothing to say.
    let environment = |name: &str| (name == "OTEL_SDK_DISABLED").then(|| "true".to_owned());
    assert!(!super::disabled(&load(CHECKS, &environment), &environment));
}

/// Over gRPC, `UNAVAILABLE` is retried, after the `RetryInfo` delay if
/// there is one, and `RESOURCE_EXHAUSTED` only with a `RetryInfo`.
#[cfg(feature = "grpc")]
#[test]
fn which_grpc_answers_are_retried() {
    use super::grpc::{ProtoAny, ProtoDuration, RetryInfo, StatusDetails, retry};
    use tonic::{Code, Status};

    let info = StatusDetails {
        details: vec![
            ProtoAny {
                type_url: "type.googleapis.com/google.rpc.DebugInfo".into(),
                value: vec![1, 2, 3],
            },
            ProtoAny {
                type_url: "type.googleapis.com/google.rpc.RetryInfo".into(),
                value: RetryInfo {
                    retry_delay: Some(ProtoDuration {
                        seconds: 3,
                        nanos: 500_000_000,
                    }),
                }
                .encode_to_vec(),
            },
        ],
    }
    .encode_to_vec();
    let with = |code| Status::with_details(code, "busy", info.clone().into());
    let delay = Retry::After(Duration::from_millis(3_500));

    assert_eq!(
        retry(&Status::new(Code::Unavailable, "busy")),
        Retry::Backoff
    );
    assert_eq!(retry(&with(Code::Unavailable)), delay);
    assert_eq!(
        retry(&Status::new(Code::ResourceExhausted, "full")),
        Retry::Never
    );
    assert_eq!(retry(&with(Code::ResourceExhausted)), delay);
    for code in [
        Code::InvalidArgument,
        Code::Unauthenticated,
        Code::Internal,
        Code::DeadlineExceeded,
    ] {
        assert_eq!(retry(&with(code)), Retry::Never, "{code:?}");
    }
    let garbled = Status::with_details(Code::Unavailable, "busy", vec![0xff, 0xff].into());
    assert_eq!(retry(&garbled), Retry::Backoff);
}

/// A gRPC collector answering `UNAVAILABLE` and then OK takes the same
/// export on the retry; the refused call is counted.
#[cfg(feature = "grpc")]
#[tokio::test]
async fn grpc_unavailable_is_retried_and_counted() {
    use opentelemetry_proto::tonic::collector::metrics::v1::metrics_service_server::{
        MetricsService, MetricsServiceServer,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fake(
        mpsc::UnboundedSender<ExportMetricsServiceRequest>,
        AtomicUsize,
    );

    #[tonic::async_trait]
    impl MetricsService for Fake {
        async fn export(
            &self,
            request: tonic::Request<ExportMetricsServiceRequest>,
        ) -> Result<tonic::Response<ExportMetricsServiceResponse>, tonic::Status> {
            let _ = self.0.send(request.into_inner());
            if self.1.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(tonic::Status::unavailable("warming up"));
            }
            Ok(tonic::Response::new(ExportMetricsServiceResponse::default()))
        }
    }

    let (sender, mut received) = mpsc::unbounded_channel();
    let app =
        tonic::service::Routes::new(MetricsServiceServer::new(Fake(sender, AtomicUsize::new(0))))
            .into_axum_router();
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
    let mut prepared = Prepared::new(&config, &|_| None).unwrap().unwrap();
    prepared.settings.interval = Duration::from_secs(1);
    prepared.settings.timeout = Duration::from_millis(500);
    prepared.settings.backoff = Backoff {
        base: Duration::from_millis(1),
        cap: Duration::from_millis(1),
        jitter: Jitter::None,
    };
    let exporter = prepared.start(Arc::clone(&metrics), Arc::new(snapshot()), no_slack());
    let mut next = async || {
        tokio::time::timeout(Duration::from_secs(5), received.recv())
            .await
            .expect("no export reached the collector within 5s")
            .unwrap()
    };
    let first = next().await;
    let retried = next().await;
    exporter.stop().await;
    assert_eq!(retried, first, "the retry sent a different export");
    assert_eq!(metrics.export.failures(), 1);
}
