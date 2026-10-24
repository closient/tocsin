// SPDX-License-Identifier: AGPL-3.0-only

//! `[telemetry]`: sending the checks' metrics to an OpenTelemetry collector.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::values::{Duration, Text};

/// `[telemetry]`: where tocsin sends its own telemetry. Without it, it
/// sends none. Like `[server]`, it is read only at startup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Telemetry {
    /// `[telemetry.otlp]`: push the checks' metrics over OTLP. The table
    /// turns the export on, even empty: each key it leaves out is taken
    /// from the standard `OTEL_EXPORTER_OTLP_*` environment variables, and
    /// a key it sets wins over them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otlp: Option<Otlp>,
}

/// `[telemetry.otlp]`: an OTLP metrics exporter.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Otlp {
    /// The collector's base URL, such as `"http://otel-collector:4318"`.
    /// Over `http/protobuf` the metrics go to its `/v1/metrics`. Without
    /// it, `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` (used as is), then
    /// `OTEL_EXPORTER_OTLP_ENDPOINT`, then `http://localhost:4318`
    /// (`http://localhost:4317` over `grpc`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// `"http/protobuf"` or `"grpc"`. Without it,
    /// `OTEL_EXPORTER_OTLP_METRICS_PROTOCOL`, then
    /// `OTEL_EXPORTER_OTLP_PROTOCOL`, then `"http/protobuf"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<OtlpProtocol>,
    /// Headers sent with each export, such as an API key. A value may be
    /// an `${ENV:NAME}` secret, which is never logged. They are added to
    /// those in `OTEL_EXPORTER_OTLP_HEADERS` and
    /// `OTEL_EXPORTER_OTLP_METRICS_HEADERS`; a header named in both is
    /// sent with the value given here.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, Text>,
    /// How often the metrics are sent. Without it,
    /// `OTEL_METRIC_EXPORT_INTERVAL` (milliseconds), then 60s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<Duration>,
    /// How long one export may take before it is abandoned and counted as
    /// failed. Without it, `OTEL_EXPORTER_OTLP_METRICS_TIMEOUT`, then
    /// `OTEL_EXPORTER_OTLP_TIMEOUT` (milliseconds), then 10s or `interval`,
    /// whichever is shorter. It may not be longer than `interval`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Duration>,
    /// Resource attributes added to those tocsin sets, such as
    /// `{ "deployment.environment.name" = "production" }`. They win over
    /// `OTEL_RESOURCE_ATTRIBUTES`, and `service.name` here wins over
    /// `OTEL_SERVICE_NAME` and the default `"tocsin"`. `service.version` and
    /// `service.instance.id` (the `node_id`) are tocsin's own and cannot be
    /// set.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub resource: BTreeMap<String, String>,
}

/// How OTLP is carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum OtlpProtocol {
    /// Protobuf in an HTTP POST, to port 4318 by convention.
    #[serde(rename = "http/protobuf")]
    HttpProtobuf,
    /// gRPC, to port 4317 by convention.
    #[serde(rename = "grpc")]
    Grpc,
}

impl OtlpProtocol {
    /// The name used in configuration and in `OTEL_EXPORTER_OTLP_PROTOCOL`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            OtlpProtocol::HttpProtobuf => "http/protobuf",
            OtlpProtocol::Grpc => "grpc",
        }
    }

    /// Parses the name used in configuration and in
    /// `OTEL_EXPORTER_OTLP_PROTOCOL`.
    ///
    /// # Errors
    ///
    /// Returns a message naming the protocols tocsin speaks.
    pub fn parse(text: &str) -> Result<Self, String> {
        match text {
            "http/protobuf" => Ok(OtlpProtocol::HttpProtobuf),
            "grpc" => Ok(OtlpProtocol::Grpc),
            other => Err(format!(
                "OTLP protocol `{other}` is not supported: use \"http/protobuf\" or \"grpc\""
            )),
        }
    }
}

/// The resource attributes tocsin sets itself, which no configuration
/// overrides.
pub const OWN_RESOURCE_ATTRIBUTES: [&str; 2] = ["service.version", "service.instance.id"];
