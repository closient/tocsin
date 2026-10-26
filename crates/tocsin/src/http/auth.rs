// SPDX-License-Identifier: AGPL-3.0-only

//! Bearer-token authorization: required for write routes, optional for
//! reads ([`Viewer`]), which show more to a `status:read` token.
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
    /// `incident:draft`: draft incidents only.
    pub struct IncidentDraft;
    /// `heartbeat`: heartbeat pings.
    pub struct Heartbeat;
    /// `release`: releases and deploys.
    pub struct Release;
    /// `silence`: making, reading and ending silences.
    pub struct Silence;

    /// `status:read`: reading what is not public.
    pub struct StatusRead;
    /// `subscribers:admin`: listing and removing email subscribers.
    pub struct SubscribersAdmin;

    impl super::Scope for Incident {
        const SCOPE: TokenScope = TokenScope::Incident;
        const NAME: &'static str = "incident";
    }
    impl super::Scope for IncidentDraft {
        const SCOPE: TokenScope = TokenScope::IncidentDraft;
        const NAME: &'static str = "incident:draft";
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
    impl super::Scope for Silence {
        const SCOPE: TokenScope = TokenScope::Silence;
        const NAME: &'static str = "silence";
    }
    impl super::Scope for StatusRead {
        const SCOPE: TokenScope = TokenScope::StatusRead;
        const NAME: &'static str = "status:read";
    }
    impl super::Scope for SubscribersAdmin {
        const SCOPE: TokenScope = TokenScope::SubscribersAdmin;
        const NAME: &'static str = "subscribers:admin";
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
    Ok(Authorized {
        actor: grant::<S>(token.id.as_str(), &token.scopes)?,
        scope: PhantomData,
    })
}

/// A writer of incidents, and whether it may publish: a token with
/// `incident`, or a person in the admin, may; a token with only
/// `incident:draft` writes drafts and nothing else. The incident routes
/// and MCP tools that take drafts take this instead of one scope.
#[derive(Debug, Clone)]
pub struct IncidentWriter {
    /// Who is writing, to record with the change.
    pub actor: Actor,
    /// Whether it may publish, and change what is published.
    pub publishes: bool,
}

impl IncidentWriter {
    /// A writer who may publish, such as a person in the admin.
    #[must_use]
    pub fn publishing(actor: Actor) -> Self {
        IncidentWriter {
            actor,
            publishes: true,
        }
    }

    /// 403 unless it may publish, worded as a route needing `incident`
    /// words it.
    ///
    /// # Errors
    ///
    /// 403 for an `incident:draft` token.
    pub fn may_publish(&self) -> Result<(), ApiError> {
        if self.publishes {
            Ok(())
        } else {
            grant::<scope::Incident>(&self.actor.id, &[]).map(drop)
        }
    }

    /// The writer the token `id` holding `scopes` is; 403 with neither
    /// incident scope.
    fn holding(id: &str, scopes: &[TokenScope]) -> Result<Self, ApiError> {
        if scopes.contains(&TokenScope::Incident) {
            return Ok(IncidentWriter {
                actor: Actor::token(id),
                publishes: true,
            });
        }
        grant::<scope::IncidentDraft>(id, scopes)
            .map_err(|_| {
                ApiError::forbidden(format!(
                    "token {id} has neither the `incident` nor the `incident:draft` scope"
                ))
            })
            .map(|actor| IncidentWriter {
                actor,
                publishes: false,
            })
    }
}

impl FromRequestParts<AppState> for IncidentWriter {
    type Rejection = ApiError;

    fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> impl Future<Output = Result<Self, ApiError>> + Send {
        std::future::ready(
            authenticate(parts, state)
                .and_then(|token| IncidentWriter::holding(token.id.as_str(), &token.scopes)),
        )
    }
}

/// The actor for a write needing `S` by the token `id` holding `scopes`;
/// 403 without the scope. Every route and MCP tool checks a scope here.
fn grant<S: Scope>(id: &str, scopes: &[TokenScope]) -> Result<Actor, ApiError> {
    if !scopes.contains(&S::SCOPE) {
        tracing::warn!(token = %id, scope = S::NAME, "token lacks the scope");
        return Err(ApiError::forbidden(format!(
            "token {id} does not have the `{}` scope",
            S::NAME
        )));
    }
    Ok(Actor::token(id))
}

/// A known token without its secret: who sent a request to `/mcp`. The
/// request is authenticated once, as on any route; each tool call then
/// checks the scope it needs, as each route does.
#[derive(Debug, Clone)]
pub struct Caller {
    id: String,
    scopes: Vec<TokenScope>,
}

impl Caller {
    /// The token the `Authorization: Bearer` header names.
    ///
    /// # Errors
    ///
    /// 401 when the header is missing, malformed or names no token.
    pub fn authenticate(parts: &Parts, state: &AppState) -> Result<Caller, ApiError> {
        authenticate(parts, state).map(|token| Caller {
            id: token.id.as_str().to_owned(),
            scopes: token.scopes.clone(),
        })
    }

    /// The actor to record for a write needing `S`.
    ///
    /// # Errors
    ///
    /// 403 without the scope, worded as the routes word it.
    pub fn grant<S: Scope>(&self) -> Result<Actor, ApiError> {
        grant::<S>(&self.id, &self.scopes)
    }

    /// The incident writer the token is, as a route taking
    /// [`IncidentWriter`] decides.
    ///
    /// # Errors
    ///
    /// 403 with neither `incident` nor `incident:draft`.
    pub fn incident_writer(&self) -> Result<IncidentWriter, ApiError> {
        IncidentWriter::holding(&self.id, &self.scopes)
    }

    /// What the token may read, as [`Viewer`] decides for a route.
    #[must_use]
    pub fn viewer(&self) -> Viewer {
        Viewer::holding(&self.scopes)
    }
}

/// The `[[token]]` an `Authorization: Bearer` header names: 401 when the
/// header is missing, malformed or holds a secret no token has.
fn authenticate<'a>(parts: &Parts, state: &'a AppState) -> Result<&'a Token, ApiError> {
    let secret = bearer(parts)
        .ok_or_else(|| ApiError::unauthorized("send a token as `Authorization: Bearer <token>`"))?;
    find_token(&state.config.tokens, secret.as_bytes())
        .ok_or_else(|| ApiError::unauthorized("unknown token"))
}

/// Who is reading. Every read route takes it, so one rule holds for all.
///
/// No `Authorization` header is the public view, and so is a known token
/// without `status:read` (an `incident` or `release` token in a script's
/// environment), which reads what anyone may. A header that is malformed
/// or names no token is 401, so a mistyped secret is an error rather than
/// a quietly smaller answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Viewer {
    /// Anyone: sees public projects, and the checks and heartbeats on a
    /// public component, only.
    Public,
    /// A token with `status:read`, or the internal board's operator: sees
    /// every project, every check and heartbeat ([`super::public`]), and
    /// check targets.
    Trusted,
}

impl Viewer {
    /// The viewer a known token holding `scopes` is.
    fn holding(scopes: &[TokenScope]) -> Viewer {
        if scopes.contains(&TokenScope::StatusRead) {
            Viewer::Trusted
        } else {
            Viewer::Public
        }
    }
}

impl FromRequestParts<AppState> for Viewer {
    type Rejection = ApiError;

    fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> impl Future<Output = Result<Self, ApiError>> + Send {
        std::future::ready(if parts.headers.contains_key(header::AUTHORIZATION) {
            authenticate(parts, state).map(|token| Viewer::holding(&token.scopes))
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
