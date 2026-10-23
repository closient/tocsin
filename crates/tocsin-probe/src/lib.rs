// SPDX-License-Identifier: AGPL-3.0-only

//! Network probes for tocsin: HTTP(S), TLS, TCP, DNS and ICMP.
//!
//! [`run`] runs one check once and returns a [`ProbeOutcome`]: the facts
//! the probe gathered, each assertion's verdict and the resulting health.
//! Evaluating assertions is pure and lives in `tocsin_core::probe`; this
//! crate only does the I/O.
//!
//! Every probe runs inside the check's `timeout` and spawns nothing that
//! outlives it, so dropping the future returned by [`run`] cancels the
//! probe cleanly.

mod dns;
mod http;
mod icmp;
mod tcp;
mod tls;

use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rustls::RootCertStore;
use tocsin_core::config::{Check, CheckType, host_and_port};
use tocsin_core::probe::{CompiledCheck, Facts, ProbeError, ProbeErrorKind, ProbeOutcome};

pub use tocsin_core::probe;

/// What every probe shares: the roots certificates are verified against
/// and the `User-Agent` HTTP probes send.
#[derive(Debug, Clone)]
pub struct ProbeContext {
    roots: Arc<RootCertStore>,
    user_agent: String,
}

impl ProbeContext {
    /// A context trusting the operating system's root certificates.
    ///
    /// # Errors
    ///
    /// Returns a [`ProbeErrorKind::Setup`] error when no root certificate
    /// could be loaded.
    pub fn system() -> Result<Self, ProbeError> {
        let found = rustls_native_certs::load_native_certs();
        let mut roots = RootCertStore::empty();
        let (added, _) = roots.add_parsable_certificates(found.certs);
        if added == 0 {
            let reasons: Vec<String> = found.errors.iter().map(ToString::to_string).collect();
            return Err(ProbeError::new(
                ProbeErrorKind::Setup,
                format!(
                    "no root certificates could be loaded from the system{}",
                    if reasons.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", reasons.join("; "))
                    }
                ),
            ));
        }
        Ok(Self::with_roots(roots))
    }

    /// A context trusting only `roots`.
    #[must_use]
    pub fn with_roots(roots: RootCertStore) -> Self {
        ProbeContext {
            roots: Arc::new(roots),
            user_agent: format!("tocsin/{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

/// Runs `compiled`'s check once, within its timeout.
///
/// A probe that cannot complete (a timeout, a refused connection, a
/// certificate that does not verify) is down, with the reason in
/// [`ProbeOutcome::error`]. A probe that completes is judged by the check's
/// assertions.
pub async fn run(compiled: &CompiledCheck, context: &ProbeContext) -> ProbeOutcome {
    let check = compiled.check();
    let started_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        });
    let start = Instant::now();
    let result = tokio::time::timeout(check.timeout.get(), probe(check, context)).await;
    let duration_ms = milliseconds(start.elapsed());
    match result {
        Ok(Ok(facts)) => ProbeOutcome::completed(compiled, started_at_ms, duration_ms, facts),
        Ok(Err(error)) => ProbeOutcome::failed(check, started_at_ms, duration_ms, error),
        Err(_) => ProbeOutcome::failed(
            check,
            started_at_ms,
            duration_ms,
            ProbeError::new(
                ProbeErrorKind::Timeout,
                format!("no result within the check's timeout of {}", check.timeout),
            ),
        ),
    }
}

async fn probe(check: &Check, context: &ProbeContext) -> Result<Facts, ProbeError> {
    match check.check_type {
        CheckType::Http => http::probe(check, context).await.map(Facts::Http),
        CheckType::Tls => tls::probe(check, context).await.map(Facts::Tls),
        CheckType::Tcp => tcp::probe(check).await.map(Facts::Tcp),
        CheckType::Dns => dns::probe(check).await.map(Facts::Dns),
        CheckType::Icmp => icmp::probe(check).await.map(Facts::Icmp),
    }
}

/// Whole milliseconds in `duration`, saturating.
fn milliseconds(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The host and port of a `tcp`, `tls` or `icmp` target, with `default`
/// when the target names no port.
fn split_target(target: &str, default: u16) -> Result<(&str, u16), ProbeError> {
    host_and_port(target)
        .map(|(host, port)| (host, port.unwrap_or(default)))
        .ok_or_else(|| {
            ProbeError::new(
                ProbeErrorKind::Setup,
                format!("`{target}` is not host or host:port"),
            )
        })
}

/// Formats an error with every source in its chain, which is where the
/// useful part of a network error usually is.
fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        source = cause.source();
    }
    message
}

/// The kind of failure an I/O error from connecting is.
fn connect_error(error: &std::io::Error, what: &str) -> ProbeError {
    let kind = if error.kind() == std::io::ErrorKind::TimedOut {
        ProbeErrorKind::Timeout
    } else {
        ProbeErrorKind::Connect
    };
    ProbeError::new(kind, format!("{what}: {}", error_chain(error)))
}
