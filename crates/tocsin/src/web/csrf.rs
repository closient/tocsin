// SPDX-License-Identifier: AGPL-3.0-only

//! Cross-site request forgery defences for every form that writes.
//!
//! Two independent checks, both required:
//! - **Origin**: the browser's `Origin` (or, failing that, `Referer`) must
//!   be this site, and `Sec-Fetch-Site`, when sent, must be `same-origin`.
//! - **Token**: a synchronizer token bound to the signed-in identity,
//!   `<expiry>.<HMAC-SHA256(key, identity, expiry)>`, sent as the `csrf`
//!   form field or the `X-CSRF-Token` header (htmx sends the header from
//!   `hx-headers` on `<body>`). The key is random per process, so tokens
//!   die with a restart; the page reloads with a fresh one.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::http::{HeaderMap, header};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};

/// The form field carrying the token.
pub const FIELD: &str = "csrf";
/// The header carrying the token on htmx requests.
pub const HEADER: &str = "x-csrf-token";

/// How long a token is good for: a working day with a page left open.
const LIFETIME: Duration = Duration::from_hours(12);

/// Issues and checks tokens.
pub struct Key(hmac::Key);

impl Key {
    /// A fresh random key.
    ///
    /// # Errors
    ///
    /// The operating system's random source failed.
    pub fn generate() -> Result<Self, String> {
        let mut bytes = [0_u8; 32];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| "the system random source failed".to_owned())?;
        Ok(Key(hmac::Key::new(hmac::HMAC_SHA256, &bytes)))
    }

    /// A token for `identity`, good until [`LIFETIME`] after `now`.
    pub fn issue(&self, identity: &str, now: SystemTime) -> String {
        let expiry = seconds(now) + LIFETIME.as_secs();
        let tag = hmac::sign(&self.0, &message(identity, expiry));
        format!("{expiry}.{}", URL_SAFE_NO_PAD.encode(tag.as_ref()))
    }

    /// Whether `token` was issued by this key for `identity` and has not
    /// expired. The comparison is constant-time.
    pub fn verify(&self, identity: &str, token: &str, now: SystemTime) -> bool {
        let Some((expiry, tag)) = token.split_once('.') else {
            return false;
        };
        if expiry.is_empty() || !expiry.bytes().all(|byte| byte.is_ascii_digit()) {
            return false;
        }
        let (Ok(expiry), Ok(tag)) = (expiry.parse::<u64>(), URL_SAFE_NO_PAD.decode(tag)) else {
            return false;
        };
        expiry > seconds(now) && hmac::verify(&self.0, &message(identity, expiry), &tag).is_ok()
    }
}

/// What the tag covers. The separator cannot occur in an email address,
/// so no two (identity, expiry) pairs share a message.
fn message(identity: &str, expiry: u64) -> Vec<u8> {
    format!("tocsin-csrf-v1\0{identity}\0{expiry}").into_bytes()
}

fn seconds(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// Checks that a write came from a page on this site.
///
/// `public_origin` is `[server] public_url`'s origin; without it, the
/// request's own `Host` is the site, which is right unless the proxy
/// rewrites `Host` (then set `public_url`).
///
/// # Errors
///
/// Why the request looks cross-site, for the log.
pub fn same_origin(headers: &HeaderMap, public_origin: Option<&str>) -> Result<(), &'static str> {
    if let Some(site) = headers.get("sec-fetch-site")
        && site.as_bytes() != b"same-origin"
    {
        return Err("Sec-Fetch-Site is not same-origin");
    }
    let claimed = if let Some(origin) = headers.get(header::ORIGIN) {
        origin.to_str().map_err(|_| "Origin is not text")?
    } else if let Some(referer) = headers.get(header::REFERER) {
        referer.to_str().map_err(|_| "Referer is not text")?
    } else {
        return Err("no Origin or Referer");
    };
    // `null` and anything else that is not a URL fails to parse.
    let claimed = url::Url::parse(claimed).map_err(|_| "Origin is not a URL")?;
    if !matches!(claimed.scheme(), "http" | "https") {
        return Err("Origin is not http or https");
    }
    let matches = if let Some(expected) = public_origin {
        claimed.origin().ascii_serialization() == expected
    } else {
        let host = headers
            .get(header::HOST)
            .and_then(|host| host.to_str().ok())
            .ok_or("no Host")?;
        let authority = match (claimed.host_str(), claimed.port()) {
            (Some(name), Some(port)) => format!("{name}:{port}"),
            (Some(name), None) => name.to_owned(),
            (None, _) => return Err("Origin has no host"),
        };
        authority.eq_ignore_ascii_case(host)
    };
    if matches {
        Ok(())
    } else {
        Err("Origin is another site")
    }
}
