// SPDX-License-Identifier: AGPL-3.0-only

//! Helpers shared by tocsin's outbound HTTP clients and its clocks.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A client builder with the TLS provider installed, `timeout` per
/// request, and tocsin's user agent. Callers add their own policies.
pub fn http_client(timeout: Duration) -> reqwest::ClientBuilder {
    // reqwest is built without a default provider; this is the one.
    // Installing twice is harmless, so the result is ignored.
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(concat!("tocsin/", env!("CARGO_PKG_VERSION")))
}

/// Time since the Unix epoch; a clock set before it reads as zero.
pub fn since_epoch(now: SystemTime) -> Duration {
    now.duration_since(UNIX_EPOCH).unwrap_or_default()
}
