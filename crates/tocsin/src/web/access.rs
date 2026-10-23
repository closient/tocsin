// SPDX-License-Identifier: AGPL-3.0-only

//! Cloudflare Access: verifies the `Cf-Access-Jwt-Assertion` token Access
//! adds to every request it lets through.
//!
//! The token is an RS256 JWT signed by the team's keys, published at
//! `https://<team>.cloudflareaccess.com/cdn-cgi/access/certs`. Only a valid
//! signature from one of those keys, for this application's audience,
//! within its lifetime, proves who is asking. The
//! `Cf-Access-Authenticated-User-Email` header proves nothing: anything
//! that reaches tocsin without passing through Access can send it.
//!
//! Hand-written on `ring` rather than a JWT crate: the only RS256 backends
//! `jsonwebtoken` offers are `aws-lc-rs` (a C build) and the `rsa` crate,
//! which carries an open advisory (RUSTSEC-2023-0071) that `cargo deny`
//! rejects. Verification is the part `ring` does; the rest is parsing and
//! a handful of comparisons, each with a test.

use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents};
use serde::Deserialize;
use tokio::sync::{Mutex, RwLock};

/// The header Access puts the token in.
pub const TOKEN_HEADER: &str = "cf-access-jwt-assertion";

/// Clock skew tolerated on `exp` and `nbf`.
const LEEWAY: Duration = Duration::from_secs(30);
/// The shortest gap between two key fetches, so a stream of tokens naming
/// unknown keys cannot turn tocsin into a request amplifier.
const REFRESH_COOLDOWN: Duration = Duration::from_secs(60);
/// Keys older than this are refetched; Access rotates them every six weeks
/// and publishes the next one ahead of time.
const MAX_KEY_AGE: Duration = Duration::from_hours(1);
/// Longest token accepted; real ones are about a kilobyte.
const MAX_TOKEN_LENGTH: usize = 16 * 1024;
/// Largest key document accepted.
const MAX_CERTS_LENGTH: usize = 64 * 1024;
/// How long one key fetch may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a token was not accepted. Logged, never shown: the person sees only
/// that they are not signed in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    /// Not three base64url parts holding JSON.
    Malformed(&'static str),
    /// A header `alg` other than `RS256`, including `none` and `HS256`.
    Algorithm(String),
    /// A `kid` the team does not publish, even after refetching its keys.
    UnknownKey(String),
    /// The keys could not be fetched and none are cached.
    KeysUnavailable,
    /// The signature does not verify with the named key.
    Signature,
    /// `iss` is not this team.
    Issuer,
    /// `aud` does not name this application.
    Audience,
    /// Past `exp`.
    Expired,
    /// Before `nbf`.
    NotYetValid,
    /// No usable `email` claim, as for a service token.
    NoEmail,
}

impl fmt::Display for Rejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Rejection::Malformed(why) => write!(formatter, "malformed token: {why}"),
            Rejection::Algorithm(alg) => write!(formatter, "algorithm {alg:?} is not RS256"),
            Rejection::UnknownKey(kid) => write!(formatter, "unknown key id {kid:?}"),
            Rejection::KeysUnavailable => formatter.write_str("signing keys unavailable"),
            Rejection::Signature => formatter.write_str("bad signature"),
            Rejection::Issuer => formatter.write_str("wrong issuer"),
            Rejection::Audience => formatter.write_str("wrong audience"),
            Rejection::Expired => formatter.write_str("expired"),
            Rejection::NotYetValid => formatter.write_str("not valid yet"),
            Rejection::NoEmail => formatter.write_str("no email claim"),
        }
    }
}

/// Verifies Access tokens for one team and application.
pub struct Verifier {
    issuer: String,
    audience: String,
    certs_url: String,
    client: reqwest::Client,
    keys: RwLock<Keys>,
    /// When the last fetch started; held while fetching so concurrent
    /// requests for an unknown key wait for one fetch instead of each
    /// starting their own.
    last_fetch: Mutex<Option<Instant>>,
    cooldown: Duration,
}

#[derive(Default)]
struct Keys {
    by_id: HashMap<String, RsaPublicKeyComponents<Vec<u8>>>,
    fetched: Option<Instant>,
}

#[derive(Deserialize)]
struct Header {
    alg: String,
    kid: Option<String>,
    crit: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct Claims {
    iss: Option<String>,
    aud: Option<Audience>,
    exp: Option<f64>,
    nbf: Option<f64>,
    email: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize)]
struct KeySet {
    keys: Vec<Jwk>,
}

#[derive(Deserialize)]
struct Jwk {
    kid: Option<String>,
    kty: String,
    alg: Option<String>,
    #[serde(rename = "use")]
    usage: Option<String>,
    n: Option<String>,
    e: Option<String>,
}

impl Verifier {
    /// A verifier for `team_domain` (`name` or `name.cloudflareaccess.com`)
    /// and the application's AUD tag.
    ///
    /// # Errors
    ///
    /// The HTTP client cannot be built.
    pub fn new(team_domain: &str, audience: &str) -> Result<Self, String> {
        let team = team_domain
            .strip_suffix(".cloudflareaccess.com")
            .unwrap_or(team_domain);
        let issuer = format!("https://{team}.cloudflareaccess.com");
        let certs_url = format!("{issuer}/cdn-cgi/access/certs");
        // reqwest is built without a default provider; this is the one.
        // Installing twice is harmless, so the result is ignored.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("tocsin/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| format!("cannot start HTTP client: {error}"))?;
        Ok(Verifier {
            issuer,
            audience: audience.to_owned(),
            certs_url,
            client,
            keys: RwLock::new(Keys::default()),
            last_fetch: Mutex::new(None),
            cooldown: REFRESH_COOLDOWN,
        })
    }

    /// Fetches keys from `url` instead of the team's certs URL. Tests only:
    /// the issuer, audience and every other check stay exactly as built.
    #[cfg(test)]
    pub fn with_certs_url(mut self, url: impl Into<String>) -> Self {
        self.certs_url = url.into();
        self
    }

    /// Changes the shortest gap between key fetches. Tests only.
    #[cfg(test)]
    pub fn with_cooldown(mut self, cooldown: Duration) -> Self {
        self.cooldown = cooldown;
        self
    }

    /// The `iss` every token must carry.
    #[cfg(test)]
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// Checks `token` and returns the email address it vouches for.
    ///
    /// # Errors
    ///
    /// Why the token proves nothing.
    pub async fn verify(&self, token: &str, now: SystemTime) -> Result<String, Rejection> {
        if token.len() > MAX_TOKEN_LENGTH {
            return Err(Rejection::Malformed("too long"));
        }
        let mut parts = token.split('.');
        let (Some(header_part), Some(payload_part), Some(signature_part), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(Rejection::Malformed("not three parts"));
        };
        let header: Header = decode_json(header_part, "header")?;
        // RS256 only: never `none`, never an HMAC keyed with the public key.
        if header.alg != "RS256" {
            return Err(Rejection::Algorithm(header.alg));
        }
        if header.crit.is_some() {
            return Err(Rejection::Malformed("unsupported critical header"));
        }
        let kid = header
            .kid
            .filter(|kid| !kid.is_empty())
            .ok_or(Rejection::Malformed("no key id"))?;
        let signature = URL_SAFE_NO_PAD
            .decode(signature_part)
            .map_err(|_| Rejection::Malformed("signature is not base64url"))?;
        let key = self.key(&kid).await?;
        let signed = &token[..header_part.len() + 1 + payload_part.len()];
        key.verify(&RSA_PKCS1_2048_8192_SHA256, signed.as_bytes(), &signature)
            .map_err(|_| Rejection::Signature)?;

        let claims: Claims = decode_json(payload_part, "payload")?;
        if claims.iss.as_deref() != Some(self.issuer.as_str()) {
            return Err(Rejection::Issuer);
        }
        let audience_matches = match &claims.aud {
            Some(Audience::One(audience)) => *audience == self.audience,
            Some(Audience::Many(audiences)) => audiences.contains(&self.audience),
            None => false,
        };
        if !audience_matches {
            return Err(Rejection::Audience);
        }
        let now = seconds(now);
        let leeway = LEEWAY.as_secs_f64();
        match claims.exp {
            Some(exp) if exp.is_finite() && now < exp + leeway => {}
            _ => return Err(Rejection::Expired),
        }
        if let Some(nbf) = claims.nbf
            && !(nbf.is_finite() && nbf - leeway <= now)
        {
            return Err(Rejection::NotYetValid);
        }
        let email = claims.email.ok_or(Rejection::NoEmail)?;
        match tocsin_core::config::email_address(&email) {
            Ok(bare) if bare == email => Ok(email),
            _ => Err(Rejection::NoEmail),
        }
    }

    /// The key named `kid`, refetching the team's keys when it is unknown
    /// or the cache is old.
    async fn key(&self, kid: &str) -> Result<RsaPublicKeyComponents<Vec<u8>>, Rejection> {
        {
            let keys = self.keys.read().await;
            let fresh = keys
                .fetched
                .is_some_and(|fetched| fetched.elapsed() < MAX_KEY_AGE);
            if fresh && let Some(key) = keys.by_id.get(kid) {
                return Ok(key.clone());
            }
        }
        self.refresh().await;
        let keys = self.keys.read().await;
        match keys.by_id.get(kid) {
            Some(key) => Ok(key.clone()),
            None if keys.fetched.is_none() => Err(Rejection::KeysUnavailable),
            None => Err(Rejection::UnknownKey(kid.to_owned())),
        }
    }

    /// Fetches the keys unless a fetch started within the cooldown. On
    /// failure the cached keys stay, and the failure is logged.
    async fn refresh(&self) {
        let mut last_fetch = self.last_fetch.lock().await;
        if last_fetch.is_some_and(|at| at.elapsed() < self.cooldown) {
            return;
        }
        *last_fetch = Some(Instant::now());
        match self.fetch().await {
            Ok(by_id) => {
                tracing::info!(keys = by_id.len(), url = %self.certs_url, "fetched Cloudflare Access keys");
                *self.keys.write().await = Keys {
                    by_id,
                    fetched: Some(Instant::now()),
                };
            }
            Err(error) => {
                tracing::error!(%error, url = %self.certs_url, "cannot fetch Cloudflare Access keys");
            }
        }
    }

    async fn fetch(&self) -> Result<HashMap<String, RsaPublicKeyComponents<Vec<u8>>>, String> {
        let mut response = self
            .client
            .get(&self.certs_url)
            .send()
            .await
            .map_err(|error| error.to_string())?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
            if body.len() + chunk.len() > MAX_CERTS_LENGTH {
                return Err(format!("larger than {MAX_CERTS_LENGTH} bytes"));
            }
            body.extend_from_slice(&chunk);
        }
        let set: KeySet = serde_json::from_slice(&body).map_err(|error| error.to_string())?;
        let mut by_id = HashMap::new();
        for jwk in set.keys {
            let usable = jwk.kty == "RSA"
                && jwk.alg.as_deref().is_none_or(|alg| alg == "RS256")
                && jwk.usage.as_deref().is_none_or(|usage| usage == "sig");
            let (true, Some(kid), Some(n), Some(e)) = (usable, jwk.kid, jwk.n, jwk.e) else {
                continue;
            };
            let (Ok(n), Ok(e)) = (URL_SAFE_NO_PAD.decode(n), URL_SAFE_NO_PAD.decode(e)) else {
                tracing::warn!(%kid, "skipping a Cloudflare Access key that is not base64url");
                continue;
            };
            by_id.insert(kid, RsaPublicKeyComponents { n, e });
        }
        if by_id.is_empty() {
            return Err("no RS256 signing keys in the response".to_owned());
        }
        Ok(by_id)
    }
}

fn decode_json<T: for<'de> Deserialize<'de>>(
    part: &str,
    what: &'static str,
) -> Result<T, Rejection> {
    let bytes = URL_SAFE_NO_PAD
        .decode(part)
        .map_err(|_| Rejection::Malformed(what))?;
    serde_json::from_slice(&bytes).map_err(|_| Rejection::Malformed(what))
}

/// Seconds since the epoch; before it reads as zero.
fn seconds(now: SystemTime) -> f64 {
    now.duration_since(UNIX_EPOCH)
        .map_or(0.0, |since| since.as_secs_f64())
}
