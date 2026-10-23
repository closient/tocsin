// SPDX-License-Identifier: AGPL-3.0-only

//! The `tls` probe, and the certificate inspection the `http` probe shares.
//!
//! A [`Recorder`] stands in for rustls' certificate verifier: it runs the
//! standard `webpki` verification, records the chain and the result, and
//! then either enforces the result or lets the handshake continue, so a
//! certificate that does not verify can still be reported on.

use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, SignatureScheme};
use tocsin_core::config::Check;
use tocsin_core::probe::{
    CertificateFacts, CertificateSummary, ProbeError, ProbeErrorKind, TlsFacts, rfc3339,
};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use x509_parser::extensions::GeneralName;

use crate::{ProbeContext, connect_error, error_chain, milliseconds, split_target};

/// What the verifier saw during one handshake.
#[derive(Debug, Clone)]
struct Recorded {
    server_name: String,
    chain: Vec<CertificateDer<'static>>,
    chain_error: Option<String>,
    hostname_valid: bool,
}

/// A certificate verifier that records what it verified.
#[derive(Debug)]
pub(crate) struct Recorder {
    /// `None` when the context trusts no roots, so no chain verifies.
    webpki: Option<Arc<WebPkiServerVerifier>>,
    provider: Arc<CryptoProvider>,
    enforce: bool,
    seen: Mutex<Option<Recorded>>,
}

impl Recorder {
    /// A recorder trusting `context`'s roots; when `enforce` is false, a
    /// certificate that does not verify is recorded but not rejected.
    pub(crate) fn new(context: &ProbeContext, enforce: bool) -> Result<Arc<Self>, ProbeError> {
        let provider = provider();
        let webpki = if context.roots.is_empty() {
            None
        } else {
            let verifier = WebPkiServerVerifier::builder_with_provider(
                context.roots.clone(),
                provider.clone(),
            )
            .build()
            .map_err(|error| {
                ProbeError::new(
                    ProbeErrorKind::Setup,
                    format!("could not build the certificate verifier: {error}"),
                )
            })?;
            Some(verifier)
        };
        Ok(Arc::new(Recorder {
            webpki,
            provider,
            enforce,
            seen: Mutex::new(None),
        }))
    }

    /// A rustls client configuration verifying through this recorder.
    pub(crate) fn client_config(self: &Arc<Self>) -> Result<ClientConfig, ProbeError> {
        let config = ClientConfig::builder_with_provider(self.provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|error| {
                ProbeError::new(ProbeErrorKind::Setup, format!("TLS setup failed: {error}"))
            })?
            .dangerous()
            .with_custom_certificate_verifier(self.clone())
            .with_no_client_auth();
        Ok(config)
    }

    /// The certificate facts of the last handshake, if there was one.
    pub(crate) fn facts(&self) -> Option<CertificateFacts> {
        let seen = self
            .seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()?;
        Some(certificate_facts(&seen))
    }

    /// Whether the last handshake was refused because its certificate did
    /// not verify.
    pub(crate) fn rejected(&self) -> bool {
        self.enforce
            && self
                .seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
                .is_some_and(|seen| seen.chain_error.is_some() || !seen.hostname_valid)
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

impl ServerCertVerifier for Recorder {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let result = match &self.webpki {
            Some(webpki) => webpki.verify_server_cert(
                end_entity,
                intermediates,
                server_name,
                ocsp_response,
                now,
            ),
            None => Err(rustls::Error::General(
                "no trusted root certificates are configured".to_owned(),
            )),
        };
        // WebPKI checks the name last, so a name mismatch means the chain
        // itself verified.
        let chain_error = match &result {
            Ok(_)
            | Err(rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => None,
            Err(error) => Some(error.to_string()),
        };
        let hostname_valid = webpki::EndEntityCert::try_from(end_entity).is_ok_and(|certificate| {
            certificate
                .verify_is_valid_for_subject_name(server_name)
                .is_ok()
        });
        let chain = std::iter::once(end_entity)
            .chain(intermediates)
            .map(|certificate| certificate.clone().into_owned())
            .collect();
        *self.seen.lock().unwrap_or_else(PoisonError::into_inner) = Some(Recorded {
            server_name: server_name.to_str().into_owned(),
            chain,
            chain_error,
            hostname_valid,
        });
        if self.enforce {
            result
        } else {
            Ok(ServerCertVerified::assertion())
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            certificate,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            certificate,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn certificate_facts(seen: &Recorded) -> CertificateFacts {
    let chain: Vec<CertificateSummary> = seen.chain.iter().map(|der| summarize(der)).collect();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        });
    let expires_in_seconds = seen
        .chain
        .first()
        .and_then(|der| x509_parser::parse_x509_certificate(der).ok())
        .map(|(_, certificate)| certificate.validity().not_after.timestamp() - now);
    CertificateFacts {
        server_name: seen.server_name.clone(),
        chain,
        expires_in_seconds,
        days_remaining: expires_in_seconds.map(|seconds| seconds.div_euclid(86_400)),
        chain_valid: seen.chain_error.is_none(),
        chain_error: seen.chain_error.clone(),
        hostname_valid: seen.hostname_valid,
    }
}

fn summarize(der: &CertificateDer<'_>) -> CertificateSummary {
    let Ok((_, certificate)) = x509_parser::parse_x509_certificate(der) else {
        return CertificateSummary {
            subject: "(unparsable certificate)".to_owned(),
            issuer: String::new(),
            not_before: String::new(),
            not_after: String::new(),
            subject_alternative_names: Vec::new(),
        };
    };
    let subject_alternative_names = certificate
        .subject_alternative_name()
        .ok()
        .flatten()
        .map(|extension| {
            extension
                .value
                .general_names
                .iter()
                .filter_map(|name| match name {
                    GeneralName::DNSName(name) => Some((*name).to_owned()),
                    GeneralName::IPAddress(bytes) => ip_address(bytes).map(|ip| ip.to_string()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    CertificateSummary {
        subject: certificate.subject().to_string(),
        issuer: certificate.issuer().to_string(),
        not_before: rfc3339(certificate.validity().not_before.timestamp()),
        not_after: rfc3339(certificate.validity().not_after.timestamp()),
        subject_alternative_names,
    }
}

fn ip_address(bytes: &[u8]) -> Option<IpAddr> {
    match bytes.len() {
        4 => <[u8; 4]>::try_from(bytes).ok().map(IpAddr::from),
        16 => <[u8; 16]>::try_from(bytes).ok().map(IpAddr::from),
        _ => None,
    }
}

/// The name to send as SNI and verify against.
pub(crate) fn server_name(name: &str) -> Result<ServerName<'static>, ProbeError> {
    ServerName::try_from(name.to_owned()).map_err(|error| {
        ProbeError::new(
            ProbeErrorKind::Setup,
            format!("`{name}` is not a valid TLS server name: {error}"),
        )
    })
}

/// Connects, completes a TLS handshake and reports the certificate. The
/// handshake goes ahead whatever the certificate, so `chain_valid` and
/// `hostname_valid` can be asserted.
pub(crate) async fn probe(check: &Check, context: &ProbeContext) -> Result<TlsFacts, ProbeError> {
    let (host, port) = split_target(&check.target, 443)?;
    let name = check
        .tls
        .as_ref()
        .and_then(|tls| tls.server_name.as_deref())
        .unwrap_or(host);
    let server_name = server_name(name)?;
    let recorder = Recorder::new(context, false)?;
    let connector = TlsConnector::from(Arc::new(recorder.client_config()?));

    let start = Instant::now();
    let stream = TcpStream::connect((host, port))
        .await
        .map_err(|error| connect_error(&error, &format!("could not connect to {host}:{port}")))?;
    let mut tls = connector
        .connect(server_name, stream)
        .await
        .map_err(|error| {
            ProbeError::new(
                ProbeErrorKind::Tls,
                format!(
                    "TLS handshake with {host}:{port} failed: {}",
                    error_chain(&error)
                ),
            )
        })?;
    let latency_ms = milliseconds(start.elapsed());
    // Close politely; the server's reaction is not part of the result.
    let _ = tokio::io::AsyncWriteExt::shutdown(&mut tls).await;
    let certificate = recorder.facts().ok_or_else(|| {
        ProbeError::new(
            ProbeErrorKind::Tls,
            "the handshake completed without a server certificate",
        )
    })?;
    Ok(TlsFacts {
        latency_ms,
        certificate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unparsable_leaf_has_no_expiry() {
        let seen = Recorded {
            server_name: "example.com".to_owned(),
            chain: vec![CertificateDer::from(vec![0x30, 0x03, 0x02, 0x01, 0x00])],
            chain_error: Some("invalid certificate encoding".to_owned()),
            hostname_valid: false,
        };
        let facts = certificate_facts(&seen);
        assert_eq!(facts.expires_in_seconds, None);
        assert_eq!(facts.days_remaining, None);
        assert_eq!(facts.chain[0].subject, "(unparsable certificate)");
    }
}
