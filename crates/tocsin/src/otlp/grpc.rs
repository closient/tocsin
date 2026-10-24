// SPDX-License-Identifier: AGPL-3.0-only

//! OTLP over gRPC, with the `grpc` feature.

use opentelemetry_proto::tonic::collector::metrics::v1::metrics_service_client::MetricsServiceClient;
use opentelemetry_proto::tonic::collector::metrics::v1::{
    ExportMetricsServiceRequest, ExportMetricsServiceResponse,
};
use tonic::metadata::{AsciiMetadataKey, AsciiMetadataValue, MetadataMap};
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};

use super::Settings;

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
            .connect_timeout(settings.timeout)
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
    ) -> Result<ExportMetricsServiceResponse, String> {
        let mut request = tonic::Request::new(request);
        *request.metadata_mut() = self.metadata.clone();
        self.client
            .clone()
            .export(request)
            .await
            .map(tonic::Response::into_inner)
            .map_err(|status| {
                format!(
                    "the collector answered {:?}: {}",
                    status.code(),
                    super::excerpt(status.message())
                )
            })
    }
}
