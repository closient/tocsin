// SPDX-License-Identifier: AGPL-3.0-only

//! The `dns` probe: one lookup, against the configured resolver or the
//! system's, with no cache and no hosts file.

use std::net::{IpAddr, SocketAddr};
use std::time::Instant;

use hickory_resolver::config::{NameServerConfig, ResolveHosts, ResolverConfig};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::net::{DnsError, NetError};
use hickory_resolver::proto::op::ResponseCode;
use hickory_resolver::proto::rr::{Name, RecordType as WireType};
use hickory_resolver::{Resolver, ResolverBuilder};
use tocsin_core::config::{Check, DnsSettings, RecordType};
use tocsin_core::probe::{DnsFacts, ProbeError, ProbeErrorKind};

use crate::{error_chain, milliseconds};

/// The port a resolver given as a bare IP address listens on.
const DNS_PORT: u16 = 53;

pub(crate) async fn probe(check: &Check) -> Result<DnsFacts, ProbeError> {
    let default = DnsSettings::default();
    let settings = check.dns.as_ref().unwrap_or(&default);
    let mut name = Name::from_ascii(&check.target).map_err(|error| {
        ProbeError::new(
            ProbeErrorKind::Setup,
            format!("`{}` is not a domain name: {error}", check.target),
        )
    })?;
    // The target is the whole name; no search domains are tried.
    name.set_fqdn(true);

    let (mut builder, resolver_label) = match &settings.resolver {
        Some(text) => {
            let address = resolver_address(text)?;
            let mut server = NameServerConfig::udp_and_tcp(address.ip());
            for connection in &mut server.connections {
                connection.port = address.port();
            }
            (
                Resolver::builder_with_config(
                    ResolverConfig::from_name_servers(vec![server]),
                    TokioRuntimeProvider::default(),
                ),
                address.to_string(),
            )
        }
        None => (system_builder()?, "system".to_owned()),
    };
    let options = builder.options_mut();
    options.cache_size = 0;
    options.use_hosts_file = ResolveHosts::Never;
    let resolver = builder.build().map_err(|error| {
        ProbeError::new(
            ProbeErrorKind::Setup,
            format!("could not build the resolver: {}", error_chain(&error)),
        )
    })?;

    let wire_type = wire_type(settings.record_type);
    let start = Instant::now();
    let result = resolver.lookup(name, wire_type).await;
    let latency_ms = milliseconds(start.elapsed());
    let (rcode, answers) = match result {
        Ok(lookup) => (
            lookup.message().metadata.response_code,
            lookup
                .answers()
                .iter()
                .filter(|record| record.record_type() == wire_type)
                .map(|record| record.data.to_string())
                .collect(),
        ),
        Err(NetError::Dns(DnsError::NoRecordsFound(empty))) => (empty.response_code, Vec::new()),
        Err(NetError::Dns(DnsError::ResponseCode(code))) => (code, Vec::new()),
        Err(NetError::Timeout) => {
            return Err(ProbeError::new(
                ProbeErrorKind::Timeout,
                format!("{resolver_label} did not answer in time"),
            ));
        }
        Err(error) => {
            return Err(ProbeError::new(
                ProbeErrorKind::Dns,
                format!(
                    "lookup through {resolver_label} failed: {}",
                    error_chain(&error)
                ),
            ));
        }
    };
    Ok(DnsFacts {
        latency_ms,
        resolver: resolver_label,
        record_type: settings.record_type,
        rcode: mnemonic(rcode),
        answers,
    })
}

fn system_builder() -> Result<ResolverBuilder<TokioRuntimeProvider>, ProbeError> {
    Resolver::builder_tokio().map_err(|error| {
        ProbeError::new(
            ProbeErrorKind::Setup,
            format!(
                "could not read the system resolver configuration: {}",
                error_chain(&error)
            ),
        )
    })
}

/// `ip` or `ip:port` (`[ip]:port` for IPv6).
fn resolver_address(text: &str) -> Result<SocketAddr, ProbeError> {
    text.parse::<SocketAddr>()
        .or_else(|_| {
            text.parse::<IpAddr>()
                .map(|ip| SocketAddr::new(ip, DNS_PORT))
        })
        .map_err(|_| {
            ProbeError::new(
                ProbeErrorKind::Setup,
                format!("resolver `{text}` is not ip or ip:port"),
            )
        })
}

fn wire_type(record_type: RecordType) -> WireType {
    match record_type {
        RecordType::A => WireType::A,
        RecordType::Aaaa => WireType::AAAA,
        RecordType::Cname => WireType::CNAME,
        RecordType::Mx => WireType::MX,
        RecordType::Ns => WireType::NS,
        RecordType::Txt => WireType::TXT,
        RecordType::Srv => WireType::SRV,
        RecordType::Caa => WireType::CAA,
    }
}

/// The RFC 1035 mnemonic for a response code, which `rcode` assertions
/// and the implicit `NOERROR` verdict compare against.
fn mnemonic(code: ResponseCode) -> String {
    match u16::from(code) {
        0 => "NOERROR".to_owned(),
        1 => "FORMERR".to_owned(),
        2 => "SERVFAIL".to_owned(),
        3 => "NXDOMAIN".to_owned(),
        4 => "NOTIMP".to_owned(),
        5 => "REFUSED".to_owned(),
        other => format!("RCODE{other}"),
    }
}
