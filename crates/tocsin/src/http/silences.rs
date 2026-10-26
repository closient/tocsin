// SPDX-License-Identifier: AGPL-3.0-only

//! `/api/v1/silences`: holding alert messages back for a while.
//!
//! A silence is history, not configuration: who made it is the token the
//! server authenticated, never a field of the request, and it ends at the
//! `expires_at` it was made with, at most `[server] max_silence` away. It
//! can be ended early, never extended. The delivery worker consults it as
//! each message comes due; see `tocsin_alert`.

use axum::Json;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use serde::Deserialize;
use tocsin_core::incident::{Actor, FieldError, Timestamp};
use tocsin_core::silence::{Known, NewSilence, Silence, SilenceList};
use tocsin_core::status::CheckKind;
use utoipa::IntoParams;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::auth::{Authorized, scope};
use super::error::{ApiError, ApiJson, ErrorBody};
use super::{AppState, now};

/// The OpenAPI tag for these routes.
pub const TAG: &str = "silences";

/// Page size when `limit` is not given.
pub const DEFAULT_LIMIT: u32 = 20;
/// Largest page.
pub const MAX_LIMIT: u32 = 100;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_silences, create_silence))
        .routes(routes!(get_silence))
        .routes(routes!(expire_silence))
}

/// `?before=&limit=` on the list.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct ListQuery {
    /// Only silences older than the previous page's last: its
    /// `next_before`.
    pub before: Option<i64>,
    /// Page size, 1 to 100; default 20.
    pub limit: Option<u32>,
}

/// Silences, newest first, active or over, each with the messages it held
/// back.
#[utoipa::path(
    get,
    path = "/api/v1/silences",
    tag = TAG,
    params(ListQuery),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Newest first", body = SilenceList),
        (status = 401, description = "No token, or an unknown one", body = ErrorBody),
        (status = 403, description = "The token lacks the `silence` scope", body = ErrorBody),
        (status = 422, description = "A bad `before` or `limit`", body = ErrorBody),
    ),
)]
pub async fn list_silences(
    State(state): State<AppState>,
    _auth: Authorized<scope::Silence>,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Result<Json<SilenceList>, ApiError> {
    let Query(query) = query
        .map_err(|rejection| ApiError::invalid(vec![FieldError::new("", rejection.body_text())]))?;
    list(&state, query).await.map(Json)
}

/// One page of silences, newest first: the read behind `GET
/// /api/v1/silences`.
///
/// # Errors
///
/// 422 for a bad `limit`; 500 if the store fails.
pub async fn list(state: &AppState, query: ListQuery) -> Result<SilenceList, ApiError> {
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(ApiError::invalid(vec![FieldError::new(
            "limit",
            format!("must be 1 to {MAX_LIMIT}"),
        )]));
    }
    let mut silences = state.store.silences(query.before, limit + 1, now()).await?;
    let more = silences.len() > limit as usize;
    silences.truncate(limit as usize);
    // The last page carries no cursor, even when it is exactly full.
    let next_before = silences.last().filter(|_| more).map(|last| last.id);
    Ok(SilenceList {
        silences,
        next_before,
    })
}

/// One silence, with the newest messages it held back.
#[utoipa::path(
    get,
    path = "/api/v1/silences/{id}",
    tag = TAG,
    params(("id" = i64, Path, description = "Silence id")),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "The silence", body = Silence),
        (status = 401, description = "No token, or an unknown one", body = ErrorBody),
        (status = 403, description = "The token lacks the `silence` scope", body = ErrorBody),
        (status = 404, description = "No silence has that id", body = ErrorBody),
    ),
)]
pub async fn get_silence(
    State(state): State<AppState>,
    _auth: Authorized<scope::Silence>,
    id: Result<Path<i64>, PathRejection>,
) -> Result<Json<Silence>, ApiError> {
    let id = silence_id(id)?;
    get(&state, id).await.map(Json)
}

/// Silence `id` as of now.
///
/// # Errors
///
/// 404 if no silence has that id; 500 if the store fails.
pub async fn get(state: &AppState, id: i64) -> Result<Silence, ApiError> {
    state
        .store
        .silence(id, now())
        .await?
        .ok_or_else(|| missing(id))
}

/// Silence the alerts the matchers name until `expires_at`. Every matcher
/// given must equal the alert's; at least one, or an `episode`, is
/// required. With an `episode` (an outage id) the silence also ends when
/// that outage does, and the matchers default to its node and subject.
/// `expires_at` is required and at most `[server] max_silence` away; who
/// made the silence is the token, never a field.
#[utoipa::path(
    post,
    path = "/api/v1/silences",
    tag = TAG,
    security(("bearer" = [])),
    request_body = NewSilence,
    responses(
        (status = 201, description = "Created; `Location` names it", body = Silence),
        (status = 401, description = "No token, or an unknown one", body = ErrorBody),
        (status = 403, description = "The token lacks the `silence` scope", body = ErrorBody),
        (status = 422, description = "Invalid fields, by path: no `expires_at`, one past `max_silence`, an unknown subject, or an episode that has ended", body = ErrorBody),
    ),
)]
pub async fn create_silence(
    State(state): State<AppState>,
    auth: Authorized<scope::Silence>,
    ApiJson(new): ApiJson<NewSilence>,
) -> Result<impl IntoResponse, ApiError> {
    let silence = create(&state, new, auth.actor).await?;
    let location = format!("/api/v1/silences/{}", silence.id);
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, location)],
        Json(silence),
    ))
}

/// Makes `new` as `actor`: scopes it to its episode, validates it against
/// the configuration and `[server] max_silence`, stores and logs it. The
/// JSON API and the MCP tool both make silences through here.
///
/// # Errors
///
/// 422 for invalid fields or an episode that is missing or over; 500 if
/// the store fails.
pub async fn create(
    state: &AppState,
    mut new: NewSilence,
    actor: Actor,
) -> Result<Silence, ApiError> {
    let at = now();
    if let Some(episode) = new.episode {
        let outage = state.store.outage(episode).await?.ok_or_else(|| {
            ApiError::invalid(vec![FieldError::new(
                "episode",
                format!("no outage has id {episode}"),
            )])
        })?;
        new.scope_to(&outage).map_err(ApiError::invalid)?;
    }
    let expires_at = validate(state, &new, at)?;
    let silence = state
        .store
        .create_silence(new, expires_at, actor.clone(), at)
        .await?;
    tracing::info!(
        silence = silence.id,
        episode = silence.episode,
        expires_at = %silence.expires_at,
        reason = %silence.reason,
        %actor,
        "silence created"
    );
    Ok(silence)
}

/// `new` checked against the configuration and `[server] max_silence`,
/// and when it expires, counted from `at`.
fn validate(state: &AppState, new: &NewSilence, at: Timestamp) -> Result<Timestamp, ApiError> {
    let config = &state.config;
    let subject = |id: &str| {
        if config.checks.iter().any(|check| check.id.as_str() == id) {
            Some(CheckKind::Check)
        } else if config.heartbeats.iter().any(|beat| beat.id.as_str() == id) {
            Some(CheckKind::Heartbeat)
        } else {
            None
        }
    };
    let component = |id: &str| state.knows_component(id);
    let notifier = |id: &str| {
        config
            .notifiers
            .iter()
            .any(|notifier| notifier.id.as_str() == id)
    };
    let known = Known {
        subject: &subject,
        component: &component,
        notifier: &notifier,
    };
    new.validate(at, config.server.max_silence, &known)
        .map_err(ApiError::invalid)
}

/// End a silence now, before it expires. One already over is answered as
/// it is; a silence is never extended.
#[utoipa::path(
    post,
    path = "/api/v1/silences/{id}/expire",
    tag = TAG,
    params(("id" = i64, Path, description = "Silence id")),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Ended, or already over", body = Silence),
        (status = 401, description = "No token, or an unknown one", body = ErrorBody),
        (status = 403, description = "The token lacks the `silence` scope", body = ErrorBody),
        (status = 404, description = "No silence has that id", body = ErrorBody),
    ),
)]
pub async fn expire_silence(
    State(state): State<AppState>,
    auth: Authorized<scope::Silence>,
    id: Result<Path<i64>, PathRejection>,
) -> Result<Json<Silence>, ApiError> {
    let id = silence_id(id)?;
    expire(&state, id, auth.actor).await.map(Json)
}

/// Ends silence `id` as `actor`, now.
///
/// # Errors
///
/// 404 if no silence has that id; 500 if the store fails.
pub async fn expire(state: &AppState, id: i64, actor: Actor) -> Result<Silence, ApiError> {
    let silence = state
        .store
        .end_silence(id, actor.clone(), now())
        .await?
        .ok_or_else(|| missing(id))?;
    // An alert it kept quiet may need a fresh fire now; the worker would
    // otherwise see that only on its next idle tick, up to a minute away.
    state.alert_wake.notify_one();
    tracing::info!(silence = silence.id, state = %silence.state, %actor, "silence expired");
    Ok(silence)
}

/// An id that is not a number names no silence.
fn silence_id(id: Result<Path<i64>, PathRejection>) -> Result<i64, ApiError> {
    id.map(|Path(id)| id)
        .map_err(|_| ApiError::not_found("no such silence"))
}

fn missing(id: i64) -> ApiError {
    ApiError::not_found(format!("no silence {id}"))
}
