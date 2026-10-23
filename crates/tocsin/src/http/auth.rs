// SPDX-License-Identifier: AGPL-3.0-only

//! Bearer-token authorization: required for write routes, optional for
//! reads that show more to a `status:read` token ([`Viewer`]).
//!
//! A handler takes [`Authorized<S>`] for the scope `S` it needs:
//!
//! ```ignore
//! async fn create(State(state): State<AppState>, auth: Authorized<scope::Incident>, ...)
//! ```
//!
//! The extractor compares `Authorization: Bearer <secret>` with every
//! `[[token]]` in constant time, answers 401 when the header is missing or
//! the secret is unknown and 403 when the token lacks the scope, and gives
//! the handler the token as the [`Actor`] to record. The secret is never
//! logged or echoed.
//!
//! A scope with [`Scope::QUERY_TOKEN`] also takes the secret as `?token=`,
//! for clients that cannot set a header; the header wins when both come.

use std::marker::PhantomData;

use axum::extract::{FromRequestParts, Query};
use axum::http::header;
use axum::http::request::Parts;
use subtle::ConstantTimeEq;
use tocsin_core::config::{Token, TokenScope};
use tocsin_core::incident::Actor;

use super::AppState;
use super::error::ApiError;

/// A scope a route requires, as a type so the route states it once.
pub trait Scope {
    /// The `[[token]]` scope required.
    const SCOPE: TokenScope;
    /// Its name in the configuration, for messages.
    const NAME: &'static str;
    /// Whether the secret may also come as `?token=`. Only for routes a
    /// plain `curl` in a cron job must reach: a URL is more often logged
    /// than a header.
    const QUERY_TOKEN: bool = false;
}

/// The scopes, one marker type each.
pub mod scope {
    use tocsin_core::config::TokenScope;

    /// `incident`: incidents and maintenance.
    pub struct Incident;
    /// `heartbeat`: heartbeat pings.
    pub struct Heartbeat;
    /// `release`: releases and deploys.
    pub struct Release;

    /// `status:read`: reading what is not public.
    pub struct StatusRead;

    impl super::Scope for Incident {
        const SCOPE: TokenScope = TokenScope::Incident;
        const NAME: &'static str = "incident";
    }
    impl super::Scope for Heartbeat {
        const SCOPE: TokenScope = TokenScope::Heartbeat;
        const NAME: &'static str = "heartbeat";
        const QUERY_TOKEN: bool = true;
    }
    impl super::Scope for Release {
        const SCOPE: TokenScope = TokenScope::Release;
        const NAME: &'static str = "release";
    }
    impl super::Scope for StatusRead {
        const SCOPE: TokenScope = TokenScope::StatusRead;
        const NAME: &'static str = "status:read";
    }
}

/// A request from a writer holding scope `S`.
pub struct Authorized<S> {
    /// Who is writing, to record with the change.
    pub actor: Actor,
    scope: PhantomData<fn() -> S>,
}

impl<S: Scope> FromRequestParts<AppState> for Authorized<S> {
    type Rejection = ApiError;

    // Nothing to wait for: the answer is ready at once.
    fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> impl Future<Output = Result<Self, ApiError>> + Send {
        std::future::ready(authorize(parts, state))
    }
}

/// 401 without a known token, 403 without scope `S`.
fn authorize<S: Scope>(parts: &Parts, state: &AppState) -> Result<Authorized<S>, ApiError> {
    let secret = match bearer(parts) {
        Some(secret) => secret.to_owned(),
        None if S::QUERY_TOKEN => query_token(parts).ok_or_else(|| {
            ApiError::unauthorized(
                "send a token as `Authorization: Bearer <token>` or as `?token=<token>`",
            )
        })?,
        None => {
            return Err(ApiError::unauthorized(
                "send a token as `Authorization: Bearer <token>`",
            ));
        }
    };
    let token = find_token(&state.config.tokens, secret.as_bytes())
        .ok_or_else(|| ApiError::unauthorized("unknown token"))?;
    if !token.scopes.contains(&S::SCOPE) {
        tracing::warn!(token = %token.id, scope = S::NAME, "token lacks the scope");
        return Err(ApiError::forbidden(format!(
            "token {} does not have the `{}` scope",
            token.id.as_str(),
            S::NAME
        )));
    }
    Ok(Authorized {
        actor: Actor::token(token.id.as_str()),
        scope: PhantomData,
    })
}

/// Who is reading a route whose answer depends on it.
///
/// No `Authorization` header is the public view. A header is checked like
/// a write's: 401 for an unknown token, 403 for one without `status:read`,
/// so a mistyped secret is an error rather than a quietly smaller answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Viewer {
    /// Anyone: sees public projects only.
    Public,
    /// A token with `status:read`: sees every project.
    // TODO(TOC-48): the private status page admits the same viewer.
    Trusted,
}

impl FromRequestParts<AppState> for Viewer {
    type Rejection = ApiError;

    fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> impl Future<Output = Result<Self, ApiError>> + Send {
        std::future::ready(if parts.headers.contains_key(header::AUTHORIZATION) {
            authorize::<scope::StatusRead>(parts, state).map(|_| Viewer::Trusted)
        } else {
            Ok(Viewer::Public)
        })
    }
}

/// The credentials of an `Authorization: Bearer` header; the scheme is
/// case-insensitive. `None` if absent, of another scheme, or empty.
fn bearer(parts: &Parts) -> Option<&str> {
    let value = parts.headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, credentials) = value.split_once(' ')?;
    let credentials = credentials.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !credentials.is_empty()).then_some(credentials)
}

/// The `token` query parameter, decoded as a form value: `%XX` escapes are
/// undone and a bare `+` is a space. A secret holding `+`, `&`, `=` or `%`
/// must therefore be sent percent-encoded (`curl -G --data-urlencode
/// token=...`); sent raw, it reads as a different, unknown token. `None` if
/// absent, empty, or the query does not parse.
fn query_token(parts: &Parts) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct TokenQuery {
        token: Option<String>,
    }
    let Query(query) = Query::<TokenQuery>::try_from_uri(&parts.uri).ok()?;
    query.token.filter(|token| !token.is_empty())
}

/// The token whose secret is `presented`.
///
/// Every token is compared, in constant time and without stopping at a
/// match, so the response time says nothing about which secrets exist or
/// how much of one was guessed; only a secret's length can show. Secrets
/// are unique (`config check` enforces it), so at most one matches.
fn find_token<'a>(tokens: &'a [Token], presented: &[u8]) -> Option<&'a Token> {
    let mut found = None;
    for token in tokens {
        if bool::from(token.secret.expose().as_bytes().ct_eq(presented)) {
            found = Some(token);
        }
    }
    found
}
