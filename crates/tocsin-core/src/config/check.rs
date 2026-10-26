// SPDX-License-Identifier: AGPL-3.0-only

//! `[[check]]`: a probe tocsin runs on an interval.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::Link;
use super::assertion::Assertion;
use super::values::{ByteSize, Duration, Id, Secret, Text};

/// A probe run on an interval, whose result sets a component's health.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Check {
    /// Unique among checks and heartbeats in every file: they share one
    /// history.
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
    /// Settings for `tcp` checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcp: Option<TcpSettings>,
    /// Conditions a result must meet, each `[[check.assert]]` table holding
    /// one. With none, a probe that completes counts as up, except that
    /// `http` needs a 2xx or 3xx status, `dns` a `NOERROR` response and
    /// `tls` a certificate that verifies for the host.
    #[serde(default, rename = "assert", skip_serializing_if = "Vec::is_empty")]
    pub assertions: Vec<Assertion>,
    /// Links shown with the check, such as its dashboard or runbook, each a
    /// `[[check.link]]` table.
    #[serde(default, rename = "link", skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<Link>,
    /// Labels on the check's alerts, such as `{ team = "ops" }`: a receiver
    /// like Alertmanager routes on them. `[alerting]` can require some and
    /// limit their values.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[schemars(extend(
        "propertyNames" = { "pattern": super::LABEL_NAME_PATTERN },
        "additionalProperties" = { "type": "string", "minLength": 1 },
    ))]
    pub labels: BTreeMap<String, String>,
    /// Annotations on the check's alerts, such as a runbook: shown, not
    /// routed on. `[alerting]` can require some and limit their values.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[schemars(extend(
        "propertyNames" = { "pattern": super::LABEL_NAME_PATTERN },
        "additionalProperties" = { "type": "string", "minLength": 1 },
    ))]
    pub annotations: BTreeMap<String, String>,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HttpSettings {
    /// Request method.
    #[serde(default)]
    pub method: HttpMethod,
    /// Whether redirects are followed; when not, a 3xx is the result.
    #[serde(default)]
    pub follow_redirects: bool,
    /// Request headers. A value may be a secret, `${ENV:NAME}`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, Text>,
    /// Request body. It may be a secret, `${ENV:NAME}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Text>,
    /// Whether the server's certificate must verify. Defaults to true.
    /// When false, the request proceeds and the certificate is still
    /// reported, so `chain_valid` and `hostname_valid` can be asserted.
    #[serde(default = "default_true")]
    pub verify_certificate: bool,
    /// The host name sent as TLS SNI and `Host`, and checked against the
    /// certificate, when it differs from the target's host. The target's
    /// host is still the address connected to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
    /// Sent as `Authorization: Bearer <token>`. Secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token: Option<Secret>,
    /// Sent as HTTP basic authentication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basic_auth: Option<BasicAuth>,
    /// The most of the response body read, such as `"512KiB"`; the rest
    /// is not read, and assertions see only what was. At most 8MiB.
    /// Defaults to 64KiB.
    #[serde(default = "default_max_body")]
    pub max_body: ByteSize,
}

/// How much of a response body an `http` probe reads unless `max_body`
/// says otherwise.
pub const DEFAULT_MAX_BODY: ByteSize = ByteSize::from_kib(64);

/// The largest `max_body` a check may set: a probe holds the body in
/// memory while it evaluates, so a cap bounds what one target can cost.
pub const MAX_BODY_CEILING: ByteSize = ByteSize::from_mib(8);

/// `[check.http.basic_auth]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BasicAuth {
    /// The user name. It may be a secret, `${ENV:NAME}`.
    pub username: Text,
    /// The password. Secret.
    pub password: Secret,
}

impl Default for HttpSettings {
    fn default() -> Self {
        HttpSettings {
            method: HttpMethod::default(),
            follow_redirects: false,
            headers: BTreeMap::new(),
            body: None,
            verify_certificate: default_true(),
            server_name: None,
            bearer_token: None,
            basic_auth: None,
            max_body: DEFAULT_MAX_BODY,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_max_body() -> ByteSize {
    DEFAULT_MAX_BODY
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

/// `[check.tcp]`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TcpSettings {
    /// Text written once connected, before the banner is read. Escapes such
    /// as `\r\n` are written in TOML's own string syntax.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send: Option<String>,
}
