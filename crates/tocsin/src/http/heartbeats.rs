// SPDX-License-Identifier: AGPL-3.0-only

//! `/api/v1/heartbeat/{id}`: pings from scheduled jobs.
//!
//! A job pings when it finishes; the engine marks the heartbeat down once
//! `interval + grace` passes without one. A ping is authenticated like any
//! write, except that the token may also come as `?token=` for clients that
//! cannot set a header. `HEAD` is refused rather than served by the `GET`
//! handler, so fetching a ping URL to preview it does not ping.
//! Authentication comes first, so without a valid token every id answers
//! 401 alike; a valid `heartbeat` token learns which ids exist from 404s.

use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::head;
use serde::{Deserialize, Serialize};
use tocsin_core::Health;
use tocsin_core::config::Id;
use tocsin_core::incident::{FieldError, Timestamp};
use tocsin_engine::{PING_MESSAGE_LIMIT, Ping, PingError};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::AppState;
use super::auth::{Authorized, scope};
use super::error::{ApiError, ErrorBody};

/// The OpenAPI tag for these routes.
pub const TAG: &str = "heartbeats";

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(ping_post, ping_get))
        .route(PING_PATH, head(ping_head))
}

const PING_PATH: &str = "/api/v1/heartbeat/{id}";

/// `HEAD` records nothing. axum would otherwise answer it with the `GET`
/// handler, and link checkers and unfurlers send `HEAD` to any URL they
/// see, so a ping URL pasted into chat would ping.
async fn ping_head() -> impl IntoResponse {
    (
        [(header::ALLOW, "GET, POST")],
        ApiError::method_not_allowed(),
    )
}

/// `?message=`, described with its limit.
const MESSAGE: &str = concat!(
    "A short note stored with the result, such as what failed; at most ",
    tocsin_engine::ping_message_limit!(),
    " bytes."
);

/// The 422 a ping can get.
const INVALID: &str = concat!(
    "A bad `status`, or a `message` over ",
    tocsin_engine::ping_message_limit!(),
    " bytes"
);

/// What a job reports about its own run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, ToSchema, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum PingStatus {
    /// The job ran and succeeded.
    Up,
    /// The job ran and failed: the heartbeat is down at once.
    Down,
}

impl PingStatus {
    /// Its name in the API.
    pub const fn as_str(self) -> &'static str {
        match self {
            PingStatus::Up => "up",
            PingStatus::Down => "down",
        }
    }
}

impl From<PingStatus> for Health {
    fn from(status: PingStatus) -> Self {
        match status {
            PingStatus::Up => Health::Up,
            PingStatus::Down => Health::Down,
        }
    }
}

/// `?status=&message=&token=` on a ping.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct PingQuery {
    /// `up` (the default) or `down`.
    #[param(inline)]
    status: Option<PingStatus>,
    /// A short note stored with the result; see [`MESSAGE`]. Described in
    /// each path's `params`, because a derived description is only ever a
    /// doc comment, and a doc comment cannot spell the limit from its
    /// constant.
    #[param(ignore)]
    message: Option<String>,
    /// A `[[token]]` secret with the `heartbeat` scope, for clients that
    /// cannot send `Authorization: Bearer`, percent-encoded (a bare `+`
    /// reads as a space). The header wins when both are sent. A URL carrying
    /// its token is itself a secret: it may be logged on its way here, and a
    /// link checker or chat unfurler that fetches it pings. Prefer the
    /// header.
    #[allow(dead_code, reason = "read by the `Authorized` extractor")]
    token: Option<String>,
}

/// A ping as recorded.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct PingReceipt {
    /// The heartbeat.
    pub id: String,
    /// The health recorded.
    pub status: PingStatus,
    /// When the ping was recorded; the next is due by this plus the
    /// heartbeat's `interval + grace`.
    pub received_at: Timestamp,
}

/// Record a ping: the job ran, and says whether it succeeded.
#[utoipa::path(
    post,
    path = "/api/v1/heartbeat/{id}",
    tag = TAG,
    security(("bearer" = [])),
    params(
        ("id" = String, Path, description = "Heartbeat id"),
        PingQuery,
        ("message" = Option<String>, Query, description = MESSAGE),
    ),
    responses(
        (status = 202, description = "Recorded", body = PingReceipt),
        (status = 401, description = "No token, or an unknown one", body = ErrorBody),
        (status = 403, description = "The token lacks the `heartbeat` scope", body = ErrorBody),
        (status = 404, description = "No heartbeat has that id", body = ErrorBody),
        (status = 422, description = INVALID, body = ErrorBody),
        (status = 503, description = "The server is shutting down", body = ErrorBody),
    ),
)]
pub async fn ping_post(
    state: State<AppState>,
    auth: Authorized<scope::Heartbeat>,
    id: Path<String>,
    query: Result<Query<PingQuery>, QueryRejection>,
) -> Result<(StatusCode, Json<PingReceipt>), ApiError> {
    ping(state, auth, id, query).await
}

/// Record a ping, for clients that can only send `GET`; the same as `POST`.
#[utoipa::path(
    get,
    path = "/api/v1/heartbeat/{id}",
    tag = TAG,
    security(("bearer" = [])),
    params(
        ("id" = String, Path, description = "Heartbeat id"),
        PingQuery,
        ("message" = Option<String>, Query, description = MESSAGE),
    ),
    responses(
        (status = 202, description = "Recorded", body = PingReceipt),
        (status = 401, description = "No token, or an unknown one", body = ErrorBody),
        (status = 403, description = "The token lacks the `heartbeat` scope", body = ErrorBody),
        (status = 404, description = "No heartbeat has that id", body = ErrorBody),
        (status = 422, description = INVALID, body = ErrorBody),
        (status = 503, description = "The server is shutting down", body = ErrorBody),
    ),
)]
pub async fn ping_get(
    state: State<AppState>,
    auth: Authorized<scope::Heartbeat>,
    id: Path<String>,
    query: Result<Query<PingQuery>, QueryRejection>,
) -> Result<(StatusCode, Json<PingReceipt>), ApiError> {
    ping(state, auth, id, query).await
}

async fn ping(
    State(state): State<AppState>,
    auth: Authorized<scope::Heartbeat>,
    Path(id): Path<String>,
    query: Result<Query<PingQuery>, QueryRejection>,
) -> Result<(StatusCode, Json<PingReceipt>), ApiError> {
    // Only a configured heartbeat takes pings: a node's liveness heartbeat,
    // whose id no configured one can hold, is pinged by its pushes alone.
    if Id::parse(&id).is_err() || !state.heartbeats.knows(&id) {
        return Err(missing(&id));
    }
    let Query(query) = query
        .map_err(|rejection| ApiError::invalid(vec![FieldError::new("", rejection.body_text())]))?;
    let message = query.message.filter(|message| !message.is_empty());
    if message
        .as_ref()
        .is_some_and(|message| message.len() > PING_MESSAGE_LIMIT)
    {
        return Err(ApiError::invalid(vec![FieldError::new(
            "message",
            format!("must be at most {PING_MESSAGE_LIMIT} bytes"),
        )]));
    }
    let status = query.status.unwrap_or(PingStatus::Up);
    let ping = Ping {
        health: status.into(),
        message,
    };
    let received_at = state
        .heartbeats
        .ping(&id, ping)
        .await
        .map_err(|error| match error {
            PingError::Unknown => missing(&id),
            PingError::Stopped => ApiError::unavailable("the server is shutting down"),
            PingError::NotStored => ApiError::internal(&error),
        })?;
    state.metrics.checks.pinged(&id, received_at);
    tracing::debug!(heartbeat = %id, ?status, actor = %auth.actor, "heartbeat ping");
    Ok((
        StatusCode::ACCEPTED,
        Json(PingReceipt {
            id,
            status,
            received_at: Timestamp::from_millis(received_at),
        }),
    ))
}

fn missing(id: &str) -> ApiError {
    ApiError::not_found(format!("no heartbeat {id}"))
}
