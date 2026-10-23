// SPDX-License-Identifier: AGPL-3.0-only

//! `[[check]]`: a probe tocsin runs on an interval.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::Link;
use super::assertion::Assertion;
use super::values::{Duration, Id};

/// A probe run on an interval, whose result sets a component's health.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Check {
    /// Unique among checks in every file.
    pub id: Id,
    /// Display name; the id is shown when it is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The `[[component]]` whose health this check sets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component: Option<Id>,
    /// Which probe runs.
    #[serde(rename = "type")]
    pub check_type: CheckType,
    /// What is probed. Its form depends on the type: a URL for `http`,
    /// `host:port` for `tcp`, `host` or `host:port` for `tls`, a domain
    /// name for `dns` and a host for `icmp`.
    pub target: String,
    /// How often the probe runs.
    #[serde(default = "default_interval")]
    pub interval: Duration,
    /// How long one probe may take before it counts as failed. Must be
    /// shorter than the interval.
    #[serde(default = "default_timeout")]
    pub timeout: Duration,
    /// Consecutive results that disagree with the current state before the
    /// state changes.
    #[serde(default = "default_confirm")]
    pub confirm: u32,
    /// Settings for `http` checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpSettings>,
    /// Settings for `dns` checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns: Option<DnsSettings>,
    /// Settings for `tls` checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<TlsSettings>,
    /// Conditions a result must meet, each `[[check.assert]]` table holding
    /// one. With none, a probe that completes counts as up (for `http`, a
    /// 2xx or 3xx status).
    #[serde(default, rename = "assert", skip_serializing_if = "Vec::is_empty")]
    pub assertions: Vec<Assertion>,
    /// Links shown with the check, such as its dashboard or runbook, each a
    /// `[[check.link]]` table.
    #[serde(default, rename = "link", skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<Link>,
}

fn default_interval() -> Duration {
    Duration::from_secs(60)
}

fn default_timeout() -> Duration {
    Duration::from_secs(10)
}

fn default_confirm() -> u32 {
    1
}

/// The probe a check runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CheckType {
    /// An HTTP or HTTPS request.
    Http,
    /// A TCP connection.
    Tcp,
    /// A DNS lookup.
    Dns,
    /// An ICMP echo (ping).
    Icmp,
    /// A TLS handshake, inspecting the certificate.
    Tls,
}

impl CheckType {
    /// The name used in configuration.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            CheckType::Http => "http",
            CheckType::Tcp => "tcp",
            CheckType::Dns => "dns",
            CheckType::Icmp => "icmp",
            CheckType::Tls => "tls",
        }
    }
}

/// `[check.http]`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HttpSettings {
    /// Request method.
    #[serde(default)]
    pub method: HttpMethod,
    /// Whether redirects are followed; when not, a 3xx is the result.
    #[serde(default)]
    pub follow_redirects: bool,
    /// Request headers.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Request body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Whether the server's certificate must verify. Defaults to true.
    #[serde(default = "default_true")]
    pub verify_certificate: bool,
}

fn default_true() -> bool {
    true
}

/// An HTTP request method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    /// `GET`.
    #[default]
    Get,
    /// `HEAD`.
    Head,
    /// `POST`.
    Post,
    /// `PUT`.
    Put,
    /// `PATCH`.
    Patch,
    /// `DELETE`.
    Delete,
    /// `OPTIONS`.
    Options,
}

/// `[check.dns]`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DnsSettings {
    /// The record type queried.
    #[serde(default)]
    pub record_type: RecordType,
    /// The resolver queried, as `ip` or `ip:port`; the system resolver when
    /// absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolver: Option<String>,
}

/// A DNS record type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum RecordType {
    /// IPv4 address.
    #[default]
    A,
    /// IPv6 address.
    Aaaa,
    /// Canonical name.
    Cname,
    /// Mail exchanger.
    Mx,
    /// Name server.
    Ns,
    /// Text.
    Txt,
    /// Service locator.
    Srv,
    /// Certification authority authorization.
    Caa,
}

/// `[check.tls]`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TlsSettings {
    /// The server name sent in the handshake (SNI); the target's host when
    /// absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
}
