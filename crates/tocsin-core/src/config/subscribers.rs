// SPDX-License-Identifier: AGPL-3.0-only

//! `[subscribers]`: email subscriptions on the public status page.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::values::{Cidr, Duration, Secret};

/// `[subscribers]`: lets people subscribe by email to incidents on the
/// components they choose and to the releases of public projects. Without
/// this table the page shows no form and the subscriber API answers 404.
///
/// It needs `[server] public_url`, for the links in each email, and a
/// complete `[server.email]` to send from. Like `[server]`, it is read only
/// at startup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Subscribers {
    /// Sign-ups one client IP address may make per `rate_window`. Past
    /// it, a sign-up is refused with 429 and `Retry-After`.
    #[serde(default = "default_signups_per_ip")]
    pub signups_per_ip: u32,
    /// Sign-ups for one email address per `rate_window`, from anywhere.
    /// Past it, a sign-up is refused with 429 and `Retry-After`.
    #[serde(default = "default_signups_per_email")]
    pub signups_per_email: u32,
    /// The window both sign-up limits count over.
    #[serde(default = "default_rate_window")]
    pub rate_window: Duration,
    /// How long a confirmation link works. An address not confirmed in
    /// time is forgotten.
    #[serde(default = "default_confirm_within")]
    pub confirm_within: Duration,
    /// A header your proxy sets to the client's IP address, such as
    /// `CF-Connecting-IP`. It is believed only when the TCP peer is inside
    /// `trusted_proxies`; otherwise the peer's own address is the client's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_ip_header: Option<String>,
    /// The networks your proxy connects from. Required with
    /// `client_ip_header`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trusted_proxies: Vec<Cidr>,
    /// Cloudflare Turnstile on the sign-up form, as `[subscribers.turnstile]`.
    /// With it, signing up needs JavaScript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turnstile: Option<Turnstile>,
}

/// `[subscribers.turnstile]`: a Cloudflare Turnstile widget every sign-up
/// must pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Turnstile {
    /// The widget's site key, which the page shows.
    pub site_key: String,
    /// The widget's secret key, which tocsin sends to Cloudflare to check
    /// each answer. Secret.
    pub secret_key: Secret,
}

fn default_signups_per_ip() -> u32 {
    10
}

fn default_signups_per_email() -> u32 {
    3
}

fn default_rate_window() -> Duration {
    Duration::from_secs(3600)
}

fn default_confirm_within() -> Duration {
    Duration::from_secs(2 * 86_400)
}
