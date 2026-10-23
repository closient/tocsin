// SPDX-License-Identifier: AGPL-3.0-only

//! The `icmp` probe: one echo request.
//!
//! The socket is an unprivileged ICMP datagram socket where the kernel
//! allows one (on Linux, the process's group within
//! `net.ipv4.ping_group_range`), and a raw socket otherwise, which needs
//! `CAP_NET_RAW`. With neither, the probe fails with a setup error that
//! names both.

use std::io::ErrorKind;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU16, Ordering};

use surge_ping::{Client, Config, ICMP, PingIdentifier, PingSequence, SurgeError};
use tocsin_core::config::Check;
use tocsin_core::probe::{IcmpFacts, ProbeError, ProbeErrorKind};

use crate::{error_chain, milliseconds, split_target};

/// The echo request's payload.
const PAYLOAD: &[u8] = b"tocsin";

/// Distinguishes concurrent probes' echo requests on a raw socket; an
/// unprivileged socket has the kernel choose the identifier instead.
static NEXT_IDENTIFIER: AtomicU16 = AtomicU16::new(0);

pub(crate) async fn probe(check: &Check) -> Result<IcmpFacts, ProbeError> {
    let (host, _) = split_target(&check.target, 0)?;
    let address = resolve(host).await?;
    let kind = if address.is_ipv4() {
        ICMP::V4
    } else {
        ICMP::V6
    };
    let client = Client::new(&Config::builder().kind(kind).build()).map_err(|error| {
        if error.kind() == ErrorKind::PermissionDenied {
            ProbeError::new(
                ProbeErrorKind::Setup,
                format!(
                    "no ICMP socket is allowed: {error}. Allow unprivileged ping by adding \
                     tocsin's group to the net.ipv4.ping_group_range sysctl, or grant the \
                     binary CAP_NET_RAW"
                ),
            )
        } else {
            ProbeError::new(
                ProbeErrorKind::Setup,
                format!("could not open an ICMP socket: {}", error_chain(&error)),
            )
        }
    })?;
    let identifier = PingIdentifier(NEXT_IDENTIFIER.fetch_add(1, Ordering::Relaxed));
    let mut pinger = client.pinger(address, identifier).await;
    pinger.timeout(check.timeout.get());
    let (_, round_trip) =
        pinger
            .ping(PingSequence(0), PAYLOAD)
            .await
            .map_err(|error| match error {
                SurgeError::Timeout { .. } => ProbeError::new(
                    ProbeErrorKind::Timeout,
                    format!("{address} did not answer the echo request in time"),
                ),
                other => ProbeError::new(
                    ProbeErrorKind::Connect,
                    format!("echo request to {address} failed: {}", error_chain(&other)),
                ),
            })?;
    Ok(IcmpFacts {
        latency_ms: milliseconds(round_trip),
        address: address.to_string(),
    })
}

async fn resolve(host: &str) -> Result<IpAddr, ProbeError> {
    if let Ok(address) = host.parse::<IpAddr>() {
        return Ok(address);
    }
    tokio::net::lookup_host((host, 0))
        .await
        .map_err(|error| {
            ProbeError::new(
                ProbeErrorKind::Dns,
                format!("could not resolve {host}: {error}"),
            )
        })?
        .next()
        .map(|address| address.ip())
        .ok_or_else(|| ProbeError::new(ProbeErrorKind::Dns, format!("{host} has no addresses")))
}
