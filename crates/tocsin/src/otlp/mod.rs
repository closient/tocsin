// SPDX-License-Identifier: AGPL-3.0-only

//! The OpenTelemetry export: when `[telemetry.otlp]` is configured, the
//! checks' metrics, and the Slack connections' when `/metrics` has them,
//! are pushed to a collector over OTLP every `interval`.
//!
//! What is sent is a [`Reading`], the same one `/metrics` is written from
//! ([`Metrics::read`]), so the two cannot disagree. An export runs on its
//! own task and never holds a lock while it waits on the network: probes
//! and database writes do not wait on the collector. A failed export is
//! logged and counted in `tocsin_otlp_export_failures_total`; the header
//! values, which may be secrets, are never logged.
//!
//! docs/metrics.md lists what is sent and where each setting comes from.

#[cfg(feature = "grpc")]
mod grpc;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use opentelemetry_proto::tonic::collector::metrics::v1::{
    ExportMetricsServiceRequest, ExportMetricsServiceResponse,
};
use opentelemetry_proto::tonic::common::v1::{AnyValue, InstrumentationScope, KeyValue, any_value};
use opentelemetry_proto::tonic::metrics::v1::{
    AggregationTemporality, Gauge, Histogram, HistogramDataPoint, Metric, NumberDataPoint,
    ResourceMetrics, ScopeMetrics, Sum, metric, number_data_point,
};
use opentelemetry_proto::tonic::resource::v1::Resource;
use prost::Message as _;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use tocsin_core::Health;
use tocsin_core::config::{Config, Otlp, OtlpProtocol};
use tocsin_core::state::ComponentStatus;
use tocsin_engine::MemberKind;
use tocsin_engine::ports::BoxFuture;
use tocsin_store::Store;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};
use url::Url;

use crate::http::LiveStatus;
use crate::metrics::{
    MemberReading, Metrics, Reading, SlackCensus, SlackReading, kind_name, up_value,
};

/// How often metrics are sent when neither the file nor
/// `OTEL_METRIC_EXPORT_INTERVAL` says.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

/// How long an export may take when neither the file nor the environment
/// says, unless `interval` is shorter.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// The attribute names, in tocsin's namespace: OpenTelemetry's semantic
/// conventions have none for a monitor's checks (docs/metrics.md).
const CHECK_ID: &str = "tocsin.check.id";
const CHECK_KIND: &str = "tocsin.check.kind";
const CHECK_TYPE: &str = "tocsin.check.type";
const CHECK_HEALTH: &str = "tocsin.check.health";
const COMPONENT_ID: &str = "tocsin.component.id";
const COMPONENT_STATUS: &str = "tocsin.component.status";
const NODE_ID: &str = "tocsin.node.id";
const NOTIFIER_ID: &str = "tocsin.notifier.id";
const SLACK_CONNECTION: &str = "tocsin.slack.connection";
const SLACK_PURPOSE: &str = "tocsin.slack.purpose";
const SLACK_STATE: &str = "tocsin.slack.state";

/// The OpenTelemetry export as configured: `[telemetry.otlp]` merged with
/// the standard `OTEL_*` environment variables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Where the metrics are sent: for `http/protobuf`, the full URL.
    pub endpoint: Url,
    /// How.
    pub protocol: OtlpProtocol,
    /// Sent with each export. Each value is marked sensitive, so its
    /// `Debug` is redacted.
    pub headers: HeaderMap,
    /// How often.
    pub interval: Duration,
    /// How long one export may take.
    pub timeout: Duration,
    /// The resource's attributes, `service.name`, `service.version` and
    /// `service.instance.id` among them.
    pub resource: BTreeMap<String, String>,
}

impl Settings {
    /// The export `config` turns on, reading the `OTEL_*` environment
    /// variables through `variable`; `None` without `[telemetry.otlp]`,
    /// whatever the environment holds.
    ///
    /// # Errors
    ///
    /// Returns what is wrong, naming the setting or variable but never a
    /// header's value.
    pub fn resolve(
        config: &Config,
        variable: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Option<Settings>, String> {
        let Some(otlp) = config
            .telemetry
            .as_ref()
            .and_then(|telemetry| telemetry.otlp.as_ref())
        else {
            return Ok(None);
        };
        let set = |name: &str| variable(name).filter(|value| !value.trim().is_empty());

        let protocol = match otlp.protocol {
            Some(protocol) => protocol,
            None => match first(
                &set,
                &[
                    "OTEL_EXPORTER_OTLP_METRICS_PROTOCOL",
                    "OTEL_EXPORTER_OTLP_PROTOCOL",
                ],
            ) {
                Some((name, value)) => {
                    OtlpProtocol::parse(value.trim()).map_err(|error| format!("{name}: {error}"))?
                }
                None => OtlpProtocol::HttpProtobuf,
            },
        };
        if protocol == OtlpProtocol::Grpc && !cfg!(feature = "grpc") {
            return Err(
                "OTLP over `grpc` needs a tocsin built with the `grpc` feature; \
                 use \"http/protobuf\""
                    .to_owned(),
            );
        }

        let endpoint = endpoint(otlp, protocol, &set)?;

        // Later sources win: the generic variable, the per-signal one,
        // then the file.
        let mut headers = HeaderMap::new();
        for name in [
            "OTEL_EXPORTER_OTLP_HEADERS",
            "OTEL_EXPORTER_OTLP_METRICS_HEADERS",
        ] {
            if let Some(text) = set(name) {
                for (key, value) in pairs(&text, name)? {
                    insert_header(&mut headers, &key, &value, name)?;
                }
            }
        }
        for (key, value) in &otlp.headers {
            insert_header(
                &mut headers,
                key,
                value.expose(),
                "`[telemetry.otlp] headers`",
            )?;
        }

        let interval = match otlp.interval {
            Some(interval) => interval.get(),
            None => match set("OTEL_METRIC_EXPORT_INTERVAL") {
                Some(text) => {
                    let interval = millis(&text, "OTEL_METRIC_EXPORT_INTERVAL")?;
                    if interval < Duration::from_secs(1) {
                        return Err("OTEL_METRIC_EXPORT_INTERVAL must be at least 1000".to_owned());
                    }
                    interval
                }
                None => DEFAULT_INTERVAL,
            },
        };
        let timeout = match otlp.timeout {
            Some(timeout) => timeout.get(),
            None => match first(
                &set,
                &[
                    "OTEL_EXPORTER_OTLP_METRICS_TIMEOUT",
                    "OTEL_EXPORTER_OTLP_TIMEOUT",
                ],
            ) {
                Some((name, text)) => millis(&text, name)?,
                None => DEFAULT_TIMEOUT.min(interval),
            },
        };
        if timeout > interval {
            return Err(format!(
                "the export timeout ({timeout:?}) must not be longer than the interval \
                 ({interval:?}): an export still running when the next is due would be abandoned"
            ));
        }

        Ok(Some(Settings {
            endpoint,
            protocol,
            headers,
            interval,
            timeout,
            resource: resource(config, otlp, &set)?,
        }))
    }
}

/// The first of `names` that is set, and its value.
fn first<'a>(set: &dyn Fn(&str) -> Option<String>, names: &[&'a str]) -> Option<(&'a str, String)> {
    names
        .iter()
        .find_map(|name| set(name).map(|value| (*name, value)))
}

/// The resource: `OTEL_RESOURCE_ATTRIBUTES`, then `OTEL_SERVICE_NAME`,
/// then the file's `resource`, each winning over the one before; then
/// tocsin's own, which win over all three.
fn resource(
    config: &Config,
    otlp: &Otlp,
    set: &dyn Fn(&str) -> Option<String>,
) -> Result<BTreeMap<String, String>, String> {
    let mut attributes = BTreeMap::new();
    if let Some(text) = set("OTEL_RESOURCE_ATTRIBUTES") {
        attributes.extend(pairs(&text, "OTEL_RESOURCE_ATTRIBUTES")?);
    }
    if let Some(name) = set("OTEL_SERVICE_NAME") {
        attributes.insert("service.name".to_owned(), name.trim().to_owned());
    }
    attributes.extend(otlp.resource.clone());
    attributes
        .entry("service.name".to_owned())
        .or_insert_with(|| "tocsin".to_owned());
    attributes.insert(
        "service.version".to_owned(),
        env!("CARGO_PKG_VERSION").to_owned(),
    );
    attributes.insert(
        "service.instance.id".to_owned(),
        config.server.node_id.clone().unwrap_or_default(),
    );
    Ok(attributes)
}

/// `base`, a collector's base URL from `source`, with the metrics path
/// added for `http/protobuf`; gRPC takes it as is.
fn signal_url(base: &str, protocol: OtlpProtocol, source: &str) -> Result<Url, String> {
    let mut url = parse_url(base, source)?;
    if protocol == OtlpProtocol::HttpProtobuf {
        let path = format!("{}/v1/metrics", url.path().trim_end_matches('/'));
        url.set_path(&path);
    }
    Ok(url)
}

fn parse_url(text: &str, source: &str) -> Result<Url, String> {
    let url = Url::parse(text).map_err(|error| format!("{source} is not a URL: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") || !url.has_host() {
        return Err(format!("{source} must be an http:// or https:// URL"));
    }
    Ok(url)
}

/// The `key=value` pairs of a comma-separated list such as
/// `OTEL_EXPORTER_OTLP_HEADERS`, each part trimmed and percent-decoded.
/// Errors name `source`, never a value.
fn pairs(text: &str, source: &str) -> Result<Vec<(String, String)>, String> {
    text.split(',')
        .filter(|entry| !entry.trim().is_empty())
        .map(|entry| {
            let (key, value) = entry
                .split_once('=')
                .ok_or_else(|| format!("{source} holds an entry with no `=`"))?;
            let decode = |part: &str| {
                percent_encoding::percent_decode_str(part.trim())
                    .decode_utf8()
                    .map(std::borrow::Cow::into_owned)
                    .map_err(|_| format!("{source} holds an entry that is not UTF-8 once decoded"))
            };
            let key = decode(key)?;
            if key.is_empty() {
                return Err(format!("{source} holds an entry with no name"));
            }
            Ok((key, decode(value)?))
        })
        .collect()
}

/// Adds header `name` from `source`, replacing any earlier one, its value
/// marked sensitive.
fn insert_header(
    headers: &mut HeaderMap,
    name: &str,
    value: &str,
    source: &str,
) -> Result<(), String> {
    let key = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| format!("header `{name}` in {source} is not a valid header name"))?;
    let mut value = HeaderValue::from_str(value).map_err(|_| {
        format!("the value of header `{name}` in {source} is not a valid header value")
    })?;
    value.set_sensitive(true);
    headers.insert(key, value);
    Ok(())
}

/// Where the metrics go: the file's endpoint, then the variables, then
/// the default for `protocol`.
fn endpoint(
    otlp: &Otlp,
    protocol: OtlpProtocol,
    set: &dyn Fn(&str) -> Option<String>,
) -> Result<Url, String> {
    if let Some(base) = &otlp.endpoint {
        signal_url(base, protocol, "`[telemetry.otlp] endpoint`")
    } else if let Some(full) = set("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT") {
        // The per-signal variable is the full URL, used as is.
        parse_url(full.trim(), "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT")
    } else if let Some(base) = set("OTEL_EXPORTER_OTLP_ENDPOINT") {
        signal_url(base.trim(), protocol, "OTEL_EXPORTER_OTLP_ENDPOINT")
    } else {
        let base = match protocol {
            OtlpProtocol::HttpProtobuf => "http://localhost:4318",
            OtlpProtocol::Grpc => "http://localhost:4317",
        };
        signal_url(base, protocol, "the default endpoint")
    }
}

/// A duration in whole milliseconds, as the `OTEL_*` variables give it.
fn millis(text: &str, source: &str) -> Result<Duration, String> {
    match text.trim().parse::<u64>() {
        Ok(millis) if millis > 0 => Ok(Duration::from_millis(millis)),
        _ => Err(format!(
            "{source} must be a whole number of milliseconds above 0"
        )),
    }
}

/// Checks the export `config` turns on can start, as `tocsin config check`
/// and every load do.
///
/// # Errors
///
/// Returns what [`Settings::resolve`] does.
pub fn check(config: &Config, variable: &dyn Fn(&str) -> Option<String>) -> Result<(), String> {
    Settings::resolve(config, variable).map(drop)
}

/// The OTLP request for `reading`, taken on `node` at `now`.
pub fn request(
    reading: &Reading,
    node: &str,
    resource: &BTreeMap<String, String>,
    now: SystemTime,
) -> ExportMetricsServiceRequest {
    let metrics = Points::of(reading, node, nanos(now)).metrics();
    ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            resource: Some(Resource {
                attributes: resource
                    .iter()
                    .map(|(key, value)| attribute(key, value))
                    .collect(),
                ..Resource::default()
            }),
            scope_metrics: vec![ScopeMetrics {
                scope: Some(InstrumentationScope {
                    name: "tocsin".to_owned(),
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                    ..InstrumentationScope::default()
                }),
                metrics,
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }],
    }
}

/// The data points of one export, by metric.
#[derive(Default)]
struct Points {
    up: Vec<NumberDataPoint>,
    durations: Vec<HistogramDataPoint>,
    last_runs: Vec<NumberDataPoint>,
    last_pings: Vec<NumberDataPoint>,
    results: Vec<NumberDataPoint>,
    statuses: Vec<NumberDataPoint>,
    slack_connections: Vec<NumberDataPoint>,
    slack_up: Vec<NumberDataPoint>,
}

impl Points {
    /// The points `reading` holds, taken on `node` at `now`.
    fn of(reading: &Reading, node: &str, now: u64) -> Self {
        let mut points = Points::default();
        for member in &reading.members {
            let attributes = member_attributes(member, node);
            if let Some(health) = member.health {
                points.up.push(number(
                    &attributes,
                    now,
                    number_data_point::Value::AsDouble(up_value(health)),
                ));
            }
            if let Some(histogram) = &member.duration {
                let (bounds, counts): (Vec<f64>, Vec<u64>) =
                    histogram.buckets.iter().copied().unzip();
                points.durations.push(HistogramDataPoint {
                    attributes: attributes.clone(),
                    start_time_unix_nano: nanos(histogram.since),
                    time_unix_nano: now,
                    count: histogram.count,
                    sum: Some(histogram.sum),
                    bucket_counts: counts,
                    // The last bucket is everything above the last bound.
                    explicit_bounds: bounds[..bounds.len() - 1].to_vec(),
                    ..HistogramDataPoint::default()
                });
            }
            if let Some(at) = member.last_run {
                points.last_runs.push(number(
                    &attributes,
                    now,
                    number_data_point::Value::AsInt(at),
                ));
            }
            if member.kind == MemberKind::Heartbeat
                && let Some(at) = member.last_ping
            {
                points.last_pings.push(number(
                    &attributes,
                    now,
                    number_data_point::Value::AsInt(at),
                ));
            }
            if let Some(counts) = &member.results {
                for (health, count) in Health::ALL.iter().zip(counts.counts) {
                    let mut attributes = attributes.clone();
                    attributes.push(attribute(CHECK_HEALTH, health.as_str()));
                    points.results.push(NumberDataPoint {
                        start_time_unix_nano: nanos(counts.since),
                        ..number(
                            &attributes,
                            now,
                            number_data_point::Value::AsInt(
                                i64::try_from(count).unwrap_or(i64::MAX),
                            ),
                        )
                    });
                }
            }
        }
        for (component, status) in &reading.components {
            for each in ComponentStatus::ALL {
                points.statuses.push(number(
                    &[
                        attribute(COMPONENT_ID, component),
                        attribute(NODE_ID, node),
                        attribute(COMPONENT_STATUS, each.as_str()),
                    ],
                    now,
                    number_data_point::Value::AsInt(i64::from(each == *status)),
                ));
            }
        }
        if let Some(slack) = &reading.slack {
            points.slack(slack, now);
        }
        points
    }

    /// Adds `slack`'s points, taken at `now`.
    fn slack(&mut self, slack: &SlackReading, now: u64) {
        for (purpose, state, count) in slack.counted() {
            self.slack_connections.push(number(
                &[
                    attribute(SLACK_PURPOSE, purpose),
                    attribute(SLACK_STATE, state),
                ],
                now,
                number_data_point::Value::AsInt(count),
            ));
        }
        for named in &slack.named {
            self.slack_up.push(number(
                &[
                    attribute(NOTIFIER_ID, &named.notifier),
                    attribute(SLACK_CONNECTION, &named.connection),
                ],
                now,
                number_data_point::Value::AsInt(i64::from(named.live)),
            ));
        }
    }

    /// The metrics holding the points; one with none is left out.
    fn metrics(self) -> Vec<Metric> {
        let cumulative = AggregationTemporality::Cumulative as i32;
        [
            metric(
                "tocsin.check.up",
                "Each check's and heartbeat's confirmed health: 1 up, 0.5 degraded, 0 down",
                "1",
                metric::Data::Gauge(Gauge {
                    data_points: self.up,
                }),
            ),
            metric(
                "tocsin.check.duration",
                "How long each check's probes take",
                "s",
                metric::Data::Histogram(Histogram {
                    data_points: self.durations,
                    aggregation_temporality: cumulative,
                }),
            ),
            metric(
                "tocsin.check.last_run",
                "When each check's latest probe started, in unix seconds",
                "s",
                metric::Data::Gauge(Gauge {
                    data_points: self.last_runs,
                }),
            ),
            metric(
                "tocsin.heartbeat.last_ping",
                "When each heartbeat was last pinged, in unix seconds",
                "s",
                metric::Data::Gauge(Gauge {
                    data_points: self.last_pings,
                }),
            ),
            metric(
                "tocsin.check.results",
                "Results each check and heartbeat recorded, by health",
                "{result}",
                metric::Data::Sum(Sum {
                    data_points: self.results,
                    aggregation_temporality: cumulative,
                    is_monotonic: true,
                }),
            ),
            metric(
                "tocsin.component.status",
                "1 for the status each component shows on the status page, 0 for the others",
                "1",
                metric::Data::Gauge(Gauge {
                    data_points: self.statuses,
                }),
            ),
            metric(
                "tocsin.slack.connections",
                "Slack channels connected by signing in, by purpose (operator or subscriber) and state (live, or dead because Slack revoked it)",
                "{connection}",
                metric::Data::Gauge(Gauge {
                    data_points: self.slack_connections,
                }),
            ),
            metric(
                "tocsin.slack.connection.up",
                "1 when the Slack connection a notifier names is connected and live, 0 when it is missing or Slack revoked it",
                "1",
                metric::Data::Gauge(Gauge {
                    data_points: self.slack_up,
                }),
            ),
        ]
        .into_iter()
        .filter(has_points)
        .collect()
    }
}

/// The attributes every point of `member` carries.
fn member_attributes(member: &MemberReading, node: &str) -> Vec<KeyValue> {
    let mut attributes = vec![
        attribute(CHECK_ID, &member.id),
        attribute(CHECK_KIND, kind_name(member.kind)),
    ];
    if !member.component.is_empty() {
        attributes.push(attribute(COMPONENT_ID, &member.component));
    }
    attributes.push(attribute(NODE_ID, node));
    if let Some(check_type) = member.check_type {
        attributes.push(attribute(CHECK_TYPE, check_type.as_str()));
    }
    attributes
}

fn attribute(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(value.to_owned())),
        }),
        ..KeyValue::default()
    }
}

fn number(attributes: &[KeyValue], now: u64, value: number_data_point::Value) -> NumberDataPoint {
    NumberDataPoint {
        attributes: attributes.to_vec(),
        time_unix_nano: now,
        value: Some(value),
        ..NumberDataPoint::default()
    }
}

fn metric(name: &str, description: &str, unit: &str, data: metric::Data) -> Metric {
    Metric {
        name: name.to_owned(),
        description: description.to_owned(),
        unit: unit.to_owned(),
        data: Some(data),
        ..Metric::default()
    }
}

/// Whether `metric` has any point: one with none is left out.
fn has_points(metric: &Metric) -> bool {
    match &metric.data {
        Some(metric::Data::Gauge(gauge)) => !gauge.data_points.is_empty(),
        Some(metric::Data::Sum(sum)) => !sum.data_points.is_empty(),
        Some(metric::Data::Histogram(histogram)) => !histogram.data_points.is_empty(),
        _ => false,
    }
}

fn nanos(at: SystemTime) -> u64 {
    u64::try_from(crate::net::since_epoch(at).as_nanos()).unwrap_or(u64::MAX)
}

/// `url` as logged: no user, password, query or fragment, which may hold
/// a credential.
fn shown(url: &Url) -> String {
    let mut url = url.clone();
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    url.to_string()
}

/// How exports reach the collector.
enum Sender {
    Http(HttpSender),
    #[cfg(feature = "grpc")]
    Grpc(grpc::GrpcSender),
}

impl Sender {
    fn new(settings: &Settings) -> Result<Self, String> {
        match settings.protocol {
            OtlpProtocol::HttpProtobuf => HttpSender::new(settings).map(Sender::Http),
            #[cfg(feature = "grpc")]
            OtlpProtocol::Grpc => grpc::GrpcSender::new(settings).map(Sender::Grpc),
            #[cfg(not(feature = "grpc"))]
            OtlpProtocol::Grpc => {
                Err("this tocsin was built without the `grpc` feature".to_owned())
            }
        }
    }

    /// Sends `request`, failing with why the collector did not take all of
    /// it.
    async fn send(&self, request: ExportMetricsServiceRequest) -> Result<(), String> {
        let response = match self {
            Sender::Http(sender) => sender.send(request).await?,
            #[cfg(feature = "grpc")]
            Sender::Grpc(sender) => sender.send(request).await?,
        };
        accepted(response)
    }
}

/// Fails when the collector rejected part of an export.
fn accepted(response: ExportMetricsServiceResponse) -> Result<(), String> {
    match response.partial_success {
        Some(partial) if partial.rejected_data_points > 0 => Err(format!(
            "the collector rejected {} of the data points: {}",
            partial.rejected_data_points,
            excerpt(&partial.error_message)
        )),
        _ => Ok(()),
    }
}

/// `http/protobuf`: a protobuf body sent in a POST to the endpoint.
struct HttpSender {
    client: reqwest::Client,
    url: Url,
    headers: HeaderMap,
}

impl HttpSender {
    fn new(settings: &Settings) -> Result<Self, String> {
        let client = crate::net::http_client(settings.timeout)
            // A redirect would take the headers, credentials among them,
            // somewhere not configured.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| format!("cannot set up HTTP: {}", crate::net::chain(&error)))?;
        Ok(HttpSender {
            client,
            url: settings.endpoint.clone(),
            headers: settings.headers.clone(),
        })
    }

    async fn send(
        &self,
        request: ExportMetricsServiceRequest,
    ) -> Result<ExportMetricsServiceResponse, String> {
        let response = self
            .client
            .post(self.url.clone())
            .headers(self.headers.clone())
            .header(CONTENT_TYPE, "application/x-protobuf")
            .body(request.encode_to_vec())
            .send()
            .await
            .map_err(|error| crate::net::chain(&error.without_url()))?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|error| crate::net::chain(&error.without_url()))?;
        if !status.is_success() {
            return Err(format!(
                "the collector answered {status}{}",
                described(&body)
            ));
        }
        ExportMetricsServiceResponse::decode(body)
            .map_err(|error| format!("the collector's answer is not an OTLP response: {error}"))
    }
}

/// The `google.rpc.Status` an OTLP/HTTP collector answers an error with.
#[derive(Clone, PartialEq, prost::Message)]
struct RpcStatus {
    #[prost(int32, tag = "1")]
    code: i32,
    #[prost(string, tag = "2")]
    message: String,
}

/// What an error response's `body` says, if anything readable.
fn described(body: &[u8]) -> String {
    if body.is_empty() {
        return String::new();
    }
    let text = match RpcStatus::decode(body) {
        Ok(status) if !status.message.is_empty() => status.message,
        _ => match std::str::from_utf8(body) {
            Ok(text) => text.to_owned(),
            Err(_) => return format!(" ({} bytes)", body.len()),
        },
    };
    format!(": {}", excerpt(&text))
}

/// `text` on one line, cut to a length fit for a log line.
fn excerpt(text: &str) -> String {
    const LIMIT: usize = 200;
    let line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(LIMIT) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line,
    }
}

/// An export configured and ready to start once the checks run.
pub struct Prepared {
    settings: Settings,
    sender: Sender,
    /// The `node_id` the points carry.
    node: String,
}

impl Prepared {
    /// The export `config` turns on, set up but not yet sending; `None`
    /// without `[telemetry.otlp]`. No connection is made here.
    ///
    /// # Errors
    ///
    /// Returns what is wrong with the settings, or why the client cannot
    /// be built.
    pub fn new(
        config: &Config,
        variable: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, String> {
        let Some(settings) = Settings::resolve(config, variable)? else {
            return Ok(None);
        };
        let sender = Sender::new(&settings)?;
        Ok(Some(Prepared {
            settings,
            sender,
            node: config.server.node_id.clone().unwrap_or_default(),
        }))
    }

    /// Starts sending what `metrics` reads of `live` and, when it
    /// describes them, of the Slack connections in `slack`, every
    /// interval from now.
    pub fn start(
        self,
        metrics: Arc<Metrics>,
        live: Arc<dyn LiveStatus>,
        slack: Arc<dyn SlackSource>,
    ) -> Exporter {
        let Prepared {
            settings,
            sender,
            node,
        } = self;
        metrics.export.started();
        tracing::info!(
            endpoint = %shown(&settings.endpoint),
            protocol = settings.protocol.as_str(),
            interval = ?settings.interval,
            timeout = ?settings.timeout,
            headers = %settings
                .headers
                .keys()
                .map(HeaderName::as_str)
                .collect::<Vec<_>>()
                .join(","),
            "sending metrics over OTLP"
        );
        Exporter {
            task: tokio::spawn(export(settings, sender, metrics, live, slack, node)),
        }
    }
}

/// The running export.
pub struct Exporter {
    task: JoinHandle<()>,
}

impl Exporter {
    /// Stops sending; an export under way is abandoned.
    pub async fn stop(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

/// Sends a reading every `settings.interval`, until aborted. Each export
/// ends within `settings.timeout`, which is no longer than the interval,
/// so exports never pile up.
async fn export(
    settings: Settings,
    sender: Sender,
    metrics: Arc<Metrics>,
    live: Arc<dyn LiveStatus>,
    slack: Arc<dyn SlackSource>,
    node: String,
) {
    let mut ticks = tokio::time::interval_at(Instant::now() + settings.interval, settings.interval);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        let once = export_once(&settings, &sender, &metrics, &*live, &*slack, &node);
        let outcome = tokio::time::timeout(settings.timeout, once)
            .await
            .unwrap_or_else(|_| Err(format!("no answer within {:?}", settings.timeout)));
        if let Err(reason) = outcome {
            metrics.export.failed();
            tracing::warn!(
                endpoint = %shown(&settings.endpoint),
                protocol = settings.protocol.as_str(),
                "OTLP metric export failed, counted in tocsin_otlp_export_failures_total: {reason}"
            );
        }
    }
}

/// Reads the state now and sends it. Like a scrape of `/metrics`, it
/// fails whole when the Slack connections it describes cannot be read,
/// rather than sending a reading without them.
async fn export_once(
    settings: &Settings,
    sender: &Sender,
    metrics: &Metrics,
    live: &dyn LiveStatus,
    slack: &dyn SlackSource,
    node: &str,
) -> Result<(), String> {
    let census = if metrics.reads_slack() {
        let census = slack.census().await;
        Some(census.map_err(|error| format!("cannot read the Slack connections: {error}"))?)
    } else {
        None
    };
    let reading = metrics.read(&live.snapshot(), node, census.as_ref());
    let request = request(&reading, node, &settings.resource, SystemTime::now());
    sender.send(request).await
}

/// Where the exporter reads the Slack connections: the store, or a fake
/// in tests.
pub trait SlackSource: Send + Sync {
    /// The Slack connections now.
    fn census(&self) -> BoxFuture<'_, Result<SlackCensus, String>>;
}

impl SlackSource for Store {
    fn census(&self) -> BoxFuture<'_, Result<SlackCensus, String>> {
        Box::pin(async move { self.slack_census().await.map_err(|error| error.to_string()) })
    }
}
