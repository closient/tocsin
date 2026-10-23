// SPDX-License-Identifier: AGPL-3.0-only

//! `/api/v1/subscribers`: signing up for email, and managing the list.
//!
//! Without `[subscribers]` every route here is 404. Signing up is public
//! and answers 202 whatever the address's state, so it cannot be used to
//! learn who subscribes; listing and removing need `subscribers:admin`.

use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use tocsin_core::incident::{FieldError, Timestamp};
use tocsin_store::Subscriber;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::AppState;
use super::auth::{Authorized, scope};
use super::error::{ApiError, ApiJson, ErrorBody};
use crate::subscribers::{SignupRequest, Subscriptions};

/// The OpenAPI tag for these routes.
pub const TAG: &str = "subscribers";

/// Page size when `limit` is not given.
pub const DEFAULT_LIMIT: u32 = 50;
/// Largest page.
pub const MAX_LIMIT: u32 = 200;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_subscribers, create_subscriber))
        .routes(routes!(delete_subscriber))
}

/// The running subscriber feature; 404 without `[subscribers]`.
pub struct Enabled(pub Arc<Subscriptions>);

impl FromRequestParts<AppState> for Enabled {
    type Rejection = ApiError;

    fn from_request_parts(
        _: &mut Parts,
        state: &AppState,
    ) -> impl Future<Output = Result<Self, ApiError>> + Send {
        std::future::ready(
            state
                .subscribers
                .clone()
                .map(Enabled)
                .ok_or_else(|| ApiError::not_found("no such route")),
        )
    }
}

/// A sign-up.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewSubscriber {
    /// The address to send to. It receives only a confirmation until its
    /// owner follows the link in it.
    pub email: String,
    /// Components whose incidents to hear about, or `["*"]` for every one.
    /// Default `["*"]`.
    #[serde(default = "every")]
    pub components: Vec<String>,
    /// Public projects whose releases to hear about, or `["*"]` for every
    /// one. Default none.
    #[serde(default)]
    pub projects: Vec<String>,
    /// The Cloudflare Turnstile answer, required when the server has
    /// Turnstile on.
    #[serde(default)]
    pub turnstile: Option<String>,
}

fn every() -> Vec<String> {
    vec![tocsin_store::EVERY.to_owned()]
}

/// The answer to a sign-up, the same for every address.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct SignupAccepted {
    /// What happens next.
    pub message: String,
}

/// The message every accepted sign-up answers with.
pub const ACCEPTED: &str =
    "If the address can receive mail, a message is on its way: follow the link in it to confirm.";

/// Sign up for email about incidents and releases. The answer is the same
/// whether the address is new, waiting for confirmation or already
/// subscribed; only the email it then receives differs.
#[utoipa::path(
    post,
    path = "/api/v1/subscribers",
    tag = TAG,
    request_body = NewSubscriber,
    responses(
        (status = 202, description = "Accepted; a confirmation email follows", body = SignupAccepted),
        (status = 404, description = "Subscriptions are off on this server", body = ErrorBody),
        (status = 422, description = "Invalid fields, by path, or a failed Turnstile check", body = ErrorBody),
        (status = 429, description = "Too many sign-ups from this client or for this address; `Retry-After` says when to try again", body = ErrorBody),
        (status = 503, description = "The Turnstile answer could not be checked", body = ErrorBody),
    ),
)]
pub async fn create_subscriber(
    State(state): State<AppState>,
    Enabled(subscriptions): Enabled,
    parts: Parts,
    ApiJson(new): ApiJson<NewSubscriber>,
) -> Result<impl IntoResponse, ApiError> {
    let ip = subscriptions.client_ip(&parts)?;
    subscriptions
        .sign_up(
            &state,
            SignupRequest {
                email: new.email,
                components: new.components,
                projects: new.projects,
                turnstile: new.turnstile,
            },
            ip,
        )
        .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(SignupAccepted {
            message: ACCEPTED.to_owned(),
        }),
    ))
}

/// `?after=&limit=` on the list.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct ListQuery {
    /// Only subscribers with a larger id: the previous page's `next_after`.
    pub after: Option<i64>,
    /// Page size, 1 to 200; default 50.
    pub limit: Option<u32>,
}

/// One subscriber, as an operator sees them.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct SubscriberEntry {
    /// The subscriber's id.
    pub id: i64,
    /// Their address.
    pub email: String,
    /// When they signed up.
    pub created_at: Timestamp,
    /// When they confirmed; absent while they have not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirmed_at: Option<Timestamp>,
    /// Components whose incidents they hear about; `*` is every one.
    pub components: Vec<String>,
    /// Projects whose releases they hear about; `*` is every public one.
    pub projects: Vec<String>,
}

impl From<Subscriber> for SubscriberEntry {
    fn from(subscriber: Subscriber) -> Self {
        SubscriberEntry {
            id: subscriber.id,
            email: subscriber.email,
            created_at: Timestamp::from_millis(subscriber.created_at_ms),
            confirmed_at: subscriber.confirmed_at_ms.map(Timestamp::from_millis),
            components: subscriber.preferences.components,
            projects: subscriber.preferences.projects,
        }
    }
}

/// How many subscribers there are.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct SubscriberTotals {
    /// Confirmed, and so receiving mail.
    pub confirmed: i64,
    /// Waiting for confirmation.
    pub unconfirmed: i64,
}

/// One page of subscribers, in id order, with the totals.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct SubscriberList {
    /// This page.
    pub subscribers: Vec<SubscriberEntry>,
    /// Pass as `after` for the next page; absent on the last.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_after: Option<i64>,
    /// Totals across every page.
    pub totals: SubscriberTotals,
}

/// List subscribers, oldest first, with the totals.
#[utoipa::path(
    get,
    path = "/api/v1/subscribers",
    tag = TAG,
    params(ListQuery),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "One page, in id order", body = SubscriberList),
        (status = 401, description = "No token, or an unknown one", body = ErrorBody),
        (status = 403, description = "The token lacks the `subscribers:admin` scope", body = ErrorBody),
        (status = 404, description = "Subscriptions are off on this server", body = ErrorBody),
        (status = 422, description = "A bad `after` or `limit`", body = ErrorBody),
    ),
)]
pub async fn list_subscribers(
    State(state): State<AppState>,
    _enabled: Enabled,
    _auth: Authorized<scope::SubscribersAdmin>,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Result<Json<SubscriberList>, ApiError> {
    let Query(query) = query
        .map_err(|rejection| ApiError::invalid(vec![FieldError::new("", rejection.body_text())]))?;
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(ApiError::invalid(vec![FieldError::new(
            "limit",
            format!("must be 1 to {MAX_LIMIT}"),
        )]));
    }
    Ok(Json(page(&state, query.after, limit).await?))
}

/// A page of `limit` subscribers after `after`, with the totals.
///
/// # Errors
///
/// 500 if the store fails.
pub async fn page(
    state: &AppState,
    after: Option<i64>,
    limit: u32,
) -> Result<SubscriberList, ApiError> {
    let mut subscribers = state.store.subscribers(after, limit + 1).await?;
    let more = subscribers.len() > limit as usize;
    subscribers.truncate(limit as usize);
    let next_after = more
        .then(|| subscribers.last().map(|last| last.id))
        .flatten();
    let counts = state.store.subscriber_counts().await?;
    Ok(SubscriberList {
        subscribers: subscribers.into_iter().map(SubscriberEntry::from).collect(),
        next_after,
        totals: SubscriberTotals {
            confirmed: counts.confirmed,
            unconfirmed: counts.unconfirmed,
        },
    })
}

/// Remove a subscriber at once, with every delivery still queued for them.
#[utoipa::path(
    delete,
    path = "/api/v1/subscribers/{id}",
    tag = TAG,
    security(("bearer" = [])),
    params(("id" = i64, Path, description = "Subscriber id")),
    responses(
        (status = 204, description = "Removed"),
        (status = 401, description = "No token, or an unknown one", body = ErrorBody),
        (status = 403, description = "The token lacks the `subscribers:admin` scope", body = ErrorBody),
        (status = 404, description = "No subscriber has that id, or subscriptions are off", body = ErrorBody),
    ),
)]
pub async fn delete_subscriber(
    State(state): State<AppState>,
    _enabled: Enabled,
    auth: Authorized<scope::SubscribersAdmin>,
    id: Result<Path<i64>, PathRejection>,
) -> Result<StatusCode, ApiError> {
    let Ok(Path(id)) = id else {
        return Err(ApiError::not_found("no such subscriber"));
    };
    if !state.store.remove_subscriber(id).await? {
        return Err(ApiError::not_found(format!("no subscriber {id}")));
    }
    tracing::info!(subscriber = id, actor = %auth.actor, "subscriber removed");
    Ok(StatusCode::NO_CONTENT)
}
