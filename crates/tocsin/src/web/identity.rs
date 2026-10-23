// SPDX-License-Identifier: AGPL-3.0-only

//! Who is asking, as proven by the identity-aware proxy in front of
//! tocsin, and whether they may use the admin pages.
//!
//! A handler that needs a signed-in person takes [`Admin`] (pages) or
//! [`AdminForm`] (writes, which adds the CSRF checks):
//!
//! ```ignore
//! async fn page(State(state): State<AppState>, admin: Admin) -> Response
//! async fn write(State(state): State<AppState>, AdminForm { admin, fields }: AdminForm) -> Response
//! ```
//!
//! With `[server.auth] mode = "none"` both answer 404, so a page behind
//! them does not exist until auth is configured.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::SystemTime;

use axum::extract::{ConnectInfo, FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, StatusCode, header};
use tocsin_core::config::{Auth, AuthMode, Cidr, Config, DEFAULT_IDENTITY_HEADER};
use tocsin_core::incident::Actor;

use super::access::{self, Verifier};
use super::csrf;
use super::form::Fields;
use super::page::HtmlError;
use crate::http::AppState;

/// The largest form body accepted.
const FORM_LIMIT: usize = 64 * 1024;

/// The admin pages' authentication: built from `[server.auth]` when it
/// turns them on.
pub struct AdminAuth {
    source: Source,
    allow: AllowList,
    csrf: csrf::Key,
    /// `[server] public_url`'s origin, the only one forms are taken from.
    public_origin: String,
}

enum Source {
    Access(Box<Verifier>),
    Header {
        name: HeaderName,
        trusted: Vec<Cidr>,
    },
}

/// `allowed_emails` and `allowed_domains`, lowercased.
struct AllowList {
    emails: Vec<String>,
    domains: Vec<String>,
}

impl AllowList {
    fn new(auth: &Auth) -> Self {
        AllowList {
            emails: auth
                .allowed_emails
                .iter()
                .map(|email| email.to_ascii_lowercase())
                .collect(),
            domains: auth
                .allowed_domains
                .iter()
                .map(|domain| domain.to_ascii_lowercase())
                .collect(),
        }
    }

    /// Everyone when both lists are empty; otherwise a listed address or
    /// an address at a listed domain. `email` is lowercase.
    fn allows(&self, email: &str) -> bool {
        if self.emails.is_empty() && self.domains.is_empty() {
            return true;
        }
        let domain = email.rsplit_once('@').map(|(_, domain)| domain);
        self.emails.iter().any(|allowed| allowed == email)
            || domain.is_some_and(|domain| self.domains.iter().any(|allowed| allowed == domain))
    }
}

impl AdminAuth {
    /// The admin pages' auth, or `None` when `[server.auth]` leaves them
    /// off.
    ///
    /// # Errors
    ///
    /// A part that needs the system (the HTTP client, the random source)
    /// cannot start, or `[server] public_url` is missing.
    pub fn from_config(config: &Config) -> Result<Option<Self>, String> {
        let Some(auth) = &config.server.auth else {
            return Ok(None);
        };
        let source = match auth.mode {
            AuthMode::None => return Ok(None),
            AuthMode::CloudflareAccess => {
                // The loader guarantees both for this mode.
                let (Some(team_domain), Some(audience)) = (&auth.team_domain, &auth.audience)
                else {
                    return Err("cloudflare_access needs team_domain and audience".to_owned());
                };
                Source::Access(Box::new(Verifier::new(team_domain, audience)?))
            }
            AuthMode::TrustedHeader => Source::Header {
                name: HeaderName::from_bytes(
                    auth.header
                        .as_deref()
                        .unwrap_or(DEFAULT_IDENTITY_HEADER)
                        .as_bytes(),
                )
                .map_err(|error| format!("invalid identity header: {error}"))?,
                trusted: auth.trusted_proxies.clone(),
            },
        };
        Ok(Some(Self::new(source, auth, config)?))
    }

    fn new(source: Source, auth: &Auth, config: &Config) -> Result<Self, String> {
        let public_origin = config
            .server
            .public_url
            .as_deref()
            .and_then(|public_url| url::Url::parse(public_url).ok())
            .map(|public_url| public_url.origin().ascii_serialization())
            .ok_or(
                "the admin page needs a valid `[server] public_url`, which the loader requires",
            )?;
        Ok(AdminAuth {
            source,
            allow: AllowList::new(auth),
            csrf: csrf::Key::generate()?,
            public_origin,
        })
    }

    /// Cloudflare Access auth with a given verifier. Tests only, to point
    /// the verifier at a local key server.
    #[cfg(test)]
    pub fn with_verifier(verifier: Verifier, config: &Config) -> Self {
        let auth = config.server.auth.clone().unwrap_or_default();
        Self::new(Source::Access(Box::new(verifier)), &auth, config).unwrap()
    }

    /// The Cloudflare Access verifier, if that is the mode. Tests only.
    #[cfg(test)]
    pub fn verifier(&self) -> Option<&Verifier> {
        match &self.source {
            Source::Access(verifier) => Some(verifier),
            Source::Header { .. } => None,
        }
    }

    /// The email address the proxy vouches for, lowercased, if it is
    /// allowed in.
    async fn identify(&self, parts: &Parts) -> Result<String, HtmlError> {
        let email = match &self.source {
            Source::Access(verifier) => {
                let token = single_header(&parts.headers, access::TOKEN_HEADER)
                    .ok_or_else(|| unauthenticated("no Cf-Access-Jwt-Assertion header"))?;
                verifier
                    .verify(token, SystemTime::now())
                    .await
                    .map_err(|rejection| unauthenticated(&rejection.to_string()))?
            }
            Source::Header { name, trusted } => {
                // The socket's peer, never `X-Forwarded-For`: only the TCP
                // connection itself cannot be forged by the client.
                let Some(ConnectInfo(peer)) = parts.extensions.get::<ConnectInfo<SocketAddr>>()
                else {
                    tracing::error!(
                        "the server was started without peer addresses; refusing trusted_header auth"
                    );
                    return Err(unauthenticated("no peer address"));
                };
                if !trusted.iter().any(|network| network.contains(peer.ip())) {
                    return Err(unauthenticated(&format!(
                        "peer {} is not in trusted_proxies",
                        peer.ip()
                    )));
                }
                let value = single_header(&parts.headers, name.as_str())
                    .ok_or_else(|| unauthenticated(&format!("no single {name} header")))?
                    .trim();
                match tocsin_core::config::email_address(value) {
                    Ok(bare) if bare == value => value.to_owned(),
                    _ => return Err(unauthenticated(&format!("{name} is not an email address"))),
                }
            }
        };
        let email = email.to_ascii_lowercase();
        if !self.allow.allows(&email) {
            tracing::warn!(%email, "admin access denied: not in allowed_emails or allowed_domains");
            return Err(HtmlError::new(
                StatusCode::FORBIDDEN,
                format!("{email} is not allowed to use this page."),
            ));
        }
        Ok(email)
    }
}

/// The value of `name`, when sent exactly once as text.
fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    match (values.next(), values.next()) {
        (Some(value), None) => value.to_str().ok(),
        _ => None,
    }
}

/// A 401 that says only that the person is not signed in; the reason goes
/// to the log.
fn unauthenticated(reason: &str) -> HtmlError {
    tracing::warn!(reason, "admin access denied: not authenticated");
    HtmlError::new(
        StatusCode::UNAUTHORIZED,
        "You are not signed in. Open this page through your sign-in proxy.",
    )
}

/// A signed-in person allowed to use the admin pages.
pub struct Admin {
    /// Their email address, lowercased.
    pub email: String,
    auth: Arc<AdminAuth>,
}

impl Admin {
    /// Who to record as the author of a change.
    pub fn actor(&self) -> Actor {
        Actor::person(&self.email)
    }

    /// A CSRF token for the forms on a page being rendered for them.
    pub fn csrf_token(&self) -> String {
        self.auth.csrf.issue(&self.email, SystemTime::now())
    }
}

impl FromRequestParts<AppState> for Admin {
    type Rejection = HtmlError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, HtmlError> {
        let Some(auth) = &state.admin else {
            return Err(HtmlError::not_found());
        };
        let email = auth.identify(parts).await?;
        Ok(Admin {
            email,
            auth: Arc::clone(auth),
        })
    }
}

/// A form submitted by a signed-in person from a page on this site, with
/// a valid CSRF token.
pub struct AdminForm {
    /// Who submitted it.
    pub admin: Admin,
    /// What they submitted.
    pub fields: Fields,
}

impl FromRequest<AppState> for AdminForm {
    type Rejection = HtmlError;

    async fn from_request(request: Request, state: &AppState) -> Result<Self, HtmlError> {
        let (mut parts, body) = request.into_parts();
        let admin = Admin::from_request_parts(&mut parts, state).await?;
        let auth = &admin.auth;
        if let Err(reason) = csrf::same_origin(&parts.headers, &auth.public_origin) {
            tracing::warn!(reason, email = %admin.email, "admin form refused: cross-origin");
            return Err(HtmlError::new(
                StatusCode::FORBIDDEN,
                "This form was not sent from this site.",
            ));
        }
        let urlencoded = parts
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .is_some_and(|mime| {
                mime.trim()
                    .eq_ignore_ascii_case("application/x-www-form-urlencoded")
            });
        if !urlencoded {
            return Err(HtmlError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Send the form as application/x-www-form-urlencoded.",
            ));
        }
        let body = axum::body::to_bytes(body, FORM_LIMIT)
            .await
            .map_err(|_| HtmlError::new(StatusCode::PAYLOAD_TOO_LARGE, "The form is too large."))?;
        let fields = Fields::parse(&body);
        let token = fields.get(csrf::FIELD);
        if !token.is_some_and(|token| auth.csrf.verify(&admin.email, token, SystemTime::now())) {
            tracing::warn!(email = %admin.email, "admin form refused: missing or invalid CSRF token");
            return Err(HtmlError::new(
                StatusCode::FORBIDDEN,
                "This form has expired. Reload the page and try again.",
            ));
        }
        Ok(AdminForm { admin, fields })
    }
}
