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
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents};
use serde::Deserialize;
use tokio::sync::watch;

use crate::net::{http_client, since_epoch};

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
/// How long past [`MAX_KEY_AGE`] cached keys are still used while refetches
/// fail. Beyond it a revoked key could still be trusted, so tokens are
/// refused until a fetch succeeds.
const KEY_GRACE: Duration = Duration::from_hours(1);
/// The first wait between fetches while no keys have ever been fetched; it
/// doubles after each failure, up to [`REFRESH_COOLDOWN`].
const STARTUP_BACKOFF: Duration = Duration::from_secs(1);
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
    /// The cached keys are past [`MAX_KEY_AGE`] plus [`KEY_GRACE`] and
    /// cannot be refetched.
    KeysStale,
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
            Rejection::KeysStale => formatter.write_str("signing keys too old to trust"),
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
    keys: Arc<KeyCache>,
}

/// The team's keys, refreshed by one background task at a time.
///
/// Verification never waits on a fetch when it has a usable key: a key
/// past [`MAX_KEY_AGE`] is still used, within [`KEY_GRACE`], while a
/// spawned task refetches. Only a request that has no usable key (an
/// unknown `kid`, nothing fetched yet, or keys past the grace) waits, and
/// then for the one fetch in flight rather than starting its own.
struct KeyCache {
    certs_url: String,
    client: reqwest::Client,
    keys: RwLock<Keys>,
    fetches: Mutex<Fetches>,
    /// Bumped when a fetch finishes, successful or not.
    finished: watch::Sender<u64>,
    limits: Limits,
}

#[derive(Default)]
struct Keys {
    by_id: HashMap<String, RsaPublicKeyComponents<Vec<u8>>>,
    fetched: Option<Instant>,
}

/// Fetch bookkeeping; never held across an `await`.
#[derive(Default)]
struct Fetches {
    in_flight: bool,
    last_started: Option<Instant>,
    /// Failures in a row, for the backoff before the first success.
    failures: u32,
}

/// The timings, constants in production and shortened by tests.
#[derive(Clone, Copy)]
struct Limits {
    cooldown: Duration,
    max_age: Duration,
    grace: Duration,
    backoff: Duration,
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
        let client = http_client(FETCH_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| format!("cannot start HTTP client: {error}"))?;
        Ok(Verifier {
            issuer,
            audience: audience.to_owned(),
            keys: Arc::new(KeyCache {
                certs_url,
                client,
                keys: RwLock::default(),
                fetches: Mutex::default(),
                finished: watch::Sender::new(0),
                limits: Limits {
                    cooldown: REFRESH_COOLDOWN,
                    max_age: MAX_KEY_AGE,
                    grace: KEY_GRACE,
                    backoff: STARTUP_BACKOFF,
                },
            }),
        })
    }

    /// Fetches keys from `url` instead of the team's certs URL. Tests only:
    /// the issuer, audience and every other check stay exactly as built.
    #[cfg(test)]
    pub fn with_certs_url(mut self, url: impl Into<String>) -> Self {
        self.cache().certs_url = url.into();
        self
    }

    /// Changes the shortest gap between key fetches. Tests only.
    #[cfg(test)]
    pub fn with_cooldown(mut self, cooldown: Duration) -> Self {
        self.cache().limits.cooldown = cooldown;
        self
    }

    /// Changes how old keys may get before a refetch (`max_age`) and how
    /// much longer they are used while refetches fail (`grace`). Tests only.
    #[cfg(test)]
    pub fn with_key_age(mut self, max_age: Duration, grace: Duration) -> Self {
        let limits = &mut self.cache().limits;
        limits.max_age = max_age;
        limits.grace = grace;
        self
    }

    /// Changes the first wait between fetches before any succeeds. Tests
    /// only.
    #[cfg(test)]
    pub fn with_backoff(mut self, backoff: Duration) -> Self {
        self.cache().limits.backoff = backoff;
        self
    }

    /// Waits for the fetch in flight, if any, to finish. Tests only.
    #[cfg(test)]
    pub async fn settle(&self) {
        let mut finished = self.keys.finished.subscribe();
        if self.keys.fetches().in_flight {
            let _ = finished.changed().await;
        }
    }

    #[cfg(test)]
    fn cache(&mut self) -> &mut KeyCache {
        Arc::get_mut(&mut self.keys).expect("configured before use")
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
        let key = KeyCache::key(&self.keys, &kid).await?;
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
        let now = since_epoch(now).as_secs_f64();
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
}

impl KeyCache {
    /// The key named `kid`.
    async fn key(
        this: &Arc<Self>,
        kid: &str,
    ) -> Result<RsaPublicKeyComponents<Vec<u8>>, Rejection> {
        let limits = this.limits;
        let mut finished = this.finished.subscribe();
        match this.lookup(kid) {
            Lookup::Fresh(key) => return Ok(key),
            Lookup::Stale(key) => {
                // Usable while one task refetches in the background.
                Self::start_fetch(this);
                return Ok(key);
            }
            Lookup::Missing => {}
        }
        // No usable key: wait for a fetch, ours or one already in flight.
        Self::start_fetch(this);
        if this.fetches().in_flight {
            let _ = tokio::time::timeout(2 * FETCH_TIMEOUT, finished.changed()).await;
        }
        match this.lookup(kid) {
            Lookup::Fresh(key) | Lookup::Stale(key) => Ok(key),
            Lookup::Missing => {
                let keys = this.read_keys();
                match keys.fetched {
                    None => Err(Rejection::KeysUnavailable),
                    Some(fetched) if fetched.elapsed() >= limits.max_age + limits.grace => {
                        tracing::error!(
                            age_seconds = fetched.elapsed().as_secs(),
                            url = %this.certs_url,
                            "refusing Cloudflare Access tokens: the signing keys could not be refetched and are too old to trust"
                        );
                        Err(Rejection::KeysStale)
                    }
                    Some(_) => Err(Rejection::UnknownKey(kid.to_owned())),
                }
            }
        }
    }

    /// `kid` among the cached keys, by how far it can be trusted.
    fn lookup(&self, kid: &str) -> Lookup {
        let keys = self.read_keys();
        let (Some(fetched), Some(key)) = (keys.fetched, keys.by_id.get(kid)) else {
            return Lookup::Missing;
        };
        let age = fetched.elapsed();
        if age < self.limits.max_age {
            Lookup::Fresh(key.clone())
        } else if age < self.limits.max_age + self.limits.grace {
            Lookup::Stale(key.clone())
        } else {
            Lookup::Missing
        }
    }

    /// Spawns a fetch unless one is in flight or the last one started too
    /// recently: within the cooldown once keys exist, and within a backoff
    /// doubling from [`STARTUP_BACKOFF`] up to the cooldown before then.
    fn start_fetch(this: &Arc<Self>) {
        let have_keys = this.read_keys().fetched.is_some();
        {
            let mut fetches = this.fetches();
            let gap = if have_keys {
                this.limits.cooldown
            } else {
                // 1s after the first failure, 2s after the second, ...
                let doublings = fetches.failures.saturating_sub(1).min(16);
                this.limits
                    .backoff
                    .saturating_mul(1 << doublings)
                    .min(this.limits.cooldown)
            };
            if fetches.in_flight || fetches.last_started.is_some_and(|at| at.elapsed() < gap) {
                return;
            }
            fetches.in_flight = true;
            fetches.last_started = Some(Instant::now());
        }
        let cache = Arc::clone(this);
        tokio::spawn(async move {
            // Clears `in_flight` and wakes waiters even if the fetch panics.
            let mut guard = FetchGuard {
                cache: &cache,
                succeeded: false,
            };
            match cache.fetch().await {
                Ok(by_id) => {
                    tracing::info!(keys = by_id.len(), url = %cache.certs_url, "fetched Cloudflare Access keys");
                    *cache.keys.write().unwrap_or_else(PoisonError::into_inner) = Keys {
                        by_id,
                        fetched: Some(Instant::now()),
                    };
                    guard.succeeded = true;
                }
                Err(error) => {
                    tracing::error!(%error, url = %cache.certs_url, "cannot fetch Cloudflare Access keys");
                }
            }
        });
    }

    fn read_keys(&self) -> std::sync::RwLockReadGuard<'_, Keys> {
        self.keys.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn fetches(&self) -> std::sync::MutexGuard<'_, Fetches> {
        self.fetches.lock().unwrap_or_else(PoisonError::into_inner)
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

/// How far a cached key can be trusted.
enum Lookup {
    /// Younger than the maximum age.
    Fresh(RsaPublicKeyComponents<Vec<u8>>),
    /// Older, but within the grace: used while a refetch runs.
    Stale(RsaPublicKeyComponents<Vec<u8>>),
    /// Not cached, or too old to use.
    Missing,
}

/// Ends a fetch: clears `in_flight`, counts the outcome for the backoff,
/// and wakes the requests waiting on it.
struct FetchGuard<'a> {
    cache: &'a KeyCache,
    succeeded: bool,
}

impl Drop for FetchGuard<'_> {
    fn drop(&mut self) {
        {
            let mut fetches = self.cache.fetches();
            fetches.in_flight = false;
            fetches.failures = if self.succeeded {
                0
            } else {
                fetches.failures.saturating_add(1)
            };
        }
        self.cache.finished.send_modify(|count| *count += 1);
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
