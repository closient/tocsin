// SPDX-License-Identifier: AGPL-3.0-only

//! OTLP over gRPC, with the `grpc` feature.

use opentelemetry_proto::tonic::collector::metrics::v1::metrics_service_client::MetricsServiceClient;
use std::time::Duration;

use opentelemetry_proto::tonic::collector::metrics::v1::{
    ExportMetricsServiceRequest, ExportMetricsServiceResponse,
};
use prost::Message as _;
use tonic::Code;
use tonic::metadata::{AsciiMetadataKey, AsciiMetadataValue, MetadataMap};
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};

use super::{Failed, Retry, Settings};

/// Sends each export as a `MetricsService/Export` call.
pub(super) struct GrpcSender {
    client: MetricsServiceClient<Channel>,
    metadata: MetadataMap,
}

impl GrpcSender {
    /// A client for the endpoint, which connects at the first export and
    /// reconnects as needed.
    pub(super) fn new(settings: &Settings) -> Result<Self, String> {
        let mut endpoint = Endpoint::from_shared(settings.endpoint.to_string())
            .map_err(|error| format!("the OTLP endpoint is not a gRPC endpoint: {error}"))?
            // A connection not made in time is `UNAVAILABLE`, retried.
            .connect_timeout(super::connect_timeout(settings))
            .timeout(settings.timeout);
        if settings.endpoint.scheme() == "https" {
            endpoint = endpoint
                .tls_config(ClientTlsConfig::new().with_native_roots())
                .map_err(|error| format!("cannot set up TLS for OTLP: {error}"))?;
        }
        let mut metadata = MetadataMap::new();
        for (name, value) in &settings.headers {
            let key = AsciiMetadataKey::from_bytes(name.as_str().as_bytes())
                .map_err(|_| format!("header `{name}` cannot be sent as gRPC metadata"))?;
            let mut value = value
                .to_str()
                .ok()
                .and_then(|text| AsciiMetadataValue::try_from(text).ok())
                .ok_or_else(|| {
                    format!("the value of header `{name}` cannot be sent as gRPC metadata")
                })?;
            value.set_sensitive(true);
            metadata.insert(key, value);
        }
        Ok(GrpcSender {
            client: MetricsServiceClient::new(endpoint.connect_lazy()),
            metadata,
        })
    }

    pub(super) async fn send(
        &self,
        request: ExportMetricsServiceRequest,
    ) -> Result<ExportMetricsServiceResponse, Failed> {
        let mut request = tonic::Request::new(request);
        *request.metadata_mut() = self.metadata.clone();
        self.client
            .clone()
            .export(request)
            .await
            .map(tonic::Response::into_inner)
            .map_err(|status| Failed {
                reason: format!(
                    "the collector answered {:?}: {}",
                    status.code(),
                    super::excerpt(status.message())
                ),
                retry: retry(&status),
            })
    }
}

/// Whether a call that failed with `status` may be made again, as the
/// OTLP/gRPC specification has it: `UNAVAILABLE` may, and
/// `RESOURCE_EXHAUSTED` only when the collector says when, in a
/// `RetryInfo`; after its delay, if it gives one.
pub(super) fn retry(status: &tonic::Status) -> Retry {
    let delay = retry_delay(status.details());
    match status.code() {
        Code::Unavailable => delay.map_or(Retry::Backoff, Retry::After),
        Code::ResourceExhausted => delay.map_or(Retry::Never, Retry::After),
        _ => Retry::Never,
    }
}

/// The `type_url` of a `google.rpc.RetryInfo`.
const RETRY_INFO: &str = "type.googleapis.com/google.rpc.RetryInfo";

/// The details of a `google.rpc.Status`, all this reads of it.
#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct StatusDetails {
    #[prost(message, repeated, tag = "3")]
    pub(super) details: Vec<ProtoAny>,
}

/// A `google.protobuf.Any`.
#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct ProtoAny {
    #[prost(string, tag = "1")]
    pub(super) type_url: String,
    #[prost(bytes = "vec", tag = "2")]
    pub(super) value: Vec<u8>,
}

/// A `google.rpc.RetryInfo`.
#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct RetryInfo {
    #[prost(message, optional, tag = "1")]
    pub(super) retry_delay: Option<ProtoDuration>,
}

/// A `google.protobuf.Duration`.
#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct ProtoDuration {
    #[prost(int64, tag = "1")]
    pub(super) seconds: i64,
    #[prost(int32, tag = "2")]
    pub(super) nanos: i32,
}

/// The delay a `RetryInfo` in a status's `details` asks for, if any. A
/// negative one waits nothing; one too long to fit a `Duration` is
/// ignored.
fn retry_delay(details: &[u8]) -> Option<Duration> {
    let details = StatusDetails::decode(details).ok()?;
    let info = details
        .details
        .iter()
        .find(|detail| detail.type_url == RETRY_INFO)?;
    let delay = RetryInfo::decode(info.value.as_slice()).ok()?.retry_delay?;
    let seconds = u64::try_from(delay.seconds).unwrap_or(0);
    let nanos = u32::try_from(delay.nanos).unwrap_or(0);
    Duration::from_secs(seconds).checked_add(Duration::from_nanos(u64::from(nanos)))
}
