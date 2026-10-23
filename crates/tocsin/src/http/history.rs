// SPDX-License-Identifier: AGPL-3.0-only

//! `/api/v1/checks/{id}/results` and `/api/v1/outages`: what happened.
//!
//! Outages are public, as on the timeline. Raw results carry errors and
//! response details that can name internal hosts, so they need a
//! `status:read` token. Both page newest first with `before`, inside the
//! same window bound as the timeline.

use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use serde::Deserialize;
use tocsin_core::incident::{FieldError, Timestamp};
use tocsin_core::status::{CheckResults, OutageList};
use tocsin_store::HistoryPage;
use utoipa::IntoParams;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::auth::{Authorized, Viewer, scope};
use super::error::{ApiError, ErrorBody};
use super::status::knows_check;
use super::timeline::{DAY_MS, DEFAULT_LIMIT, DEFAULT_WINDOW_MS, check_window};
use super::{AppState, now};

/// The OpenAPI tag for these routes.
pub const TAG: &str = "history";

/// How far back a check's results reach when `since` is not given: one
/// day.
pub const DEFAULT_RESULTS_WINDOW_MS: i64 = DAY_MS;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(get_results))
        .routes(routes!(list_outages))
}

/// `?since=&before=&limit=` on a check's results.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct ResultsParams {
    /// The start of the window (RFC 3339): at most 366 days ago; default
    /// one day ago. Keep it the same while paging.
    pub since: Option<Timestamp>,
    /// Only results after the previous page's last: its `next_before`.
    /// One that names no result of this check (purged since, or from
    /// another read) is 422; start again without it.
    pub before: Option<i64>,
    /// Results, 1 to 500; default 100.
    pub limit: Option<u32>,
}

/// `?since=&before=&limit=` on outages.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct OutageParams {
    /// The start of the window (RFC 3339): outages still open at or after
    /// it. At most 366 days ago; default 30 days ago. Keep it the same
    /// while paging.
    pub since: Option<Timestamp>,
    /// Only outages after the previous page's last: its `next_before`.
    /// One that names no outage (purged since) is 422; start again
    /// without it.
    pub before: Option<i64>,
    /// Outages, 1 to 500; default 100.
    pub limit: Option<u32>,
}

/// One check's or heartbeat's raw results, newest first.
#[utoipa::path(
    get,
    path = "/api/v1/checks/{id}/results",
    tag = TAG,
    params(("id" = String, Path, description = "Check or heartbeat id"), ResultsParams),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "One page, newest first", body = CheckResults),
        (status = 401, description = "No token, or an unknown one", body = ErrorBody),
        (status = 403, description = "The token lacks the `status:read` scope", body = ErrorBody),
        (status = 404, description = "No `[[check]]` or `[[heartbeat]]` has that id", body = ErrorBody),
        (status = 422, description = "A bad `since` or `limit`, or a `before` naming no row of this read", body = ErrorBody),
    ),
)]
pub async fn get_results(
    State(state): State<AppState>,
    _reader: Authorized<scope::StatusRead>,
    Path(id): Path<String>,
    query: Result<Query<ResultsParams>, QueryRejection>,
) -> Result<Json<CheckResults>, ApiError> {
    let Query(query) = query.map_err(|rejection| rejected(&rejection))?;
    results(&state, id, query).await.map(Json)
}

/// One page of check `id`'s results: the read behind
/// `GET /api/v1/checks/{id}/results`.
///
/// # Errors
///
/// 404 for an unknown check, 422 for a bad window.
pub async fn results(
    state: &AppState,
    id: String,
    query: ResultsParams,
) -> Result<CheckResults, ApiError> {
    if !knows_check(&state.config, &id) {
        return Err(ApiError::not_found(format!("no check {id}")));
    }
    let page = page(
        query.since,
        DEFAULT_RESULTS_WINDOW_MS,
        query.before,
        query.limit,
    )?;
    let mut results = state.store.check_results(&id, more(page)).await?;
    let next_before = trim(&mut results, page.limit, |result| result.id);
    Ok(CheckResults {
        check_id: id,
        since: page.since,
        results,
        next_before,
    })
}

/// Outages open at any moment in the window, newest start first.
#[utoipa::path(
    get,
    path = "/api/v1/outages",
    tag = TAG,
    params(OutageParams),
    security((), ("bearer" = [])),
    responses(
        (status = 200, description = "One page, newest first", body = OutageList),
        (status = 401, description = "An `Authorization` header with an unknown token", body = ErrorBody),
        (status = 422, description = "A bad `since` or `limit`, or a `before` naming no row of this read", body = ErrorBody),
    ),
)]
pub async fn list_outages(
    State(state): State<AppState>,
    _viewer: Viewer,
    query: Result<Query<OutageParams>, QueryRejection>,
) -> Result<Json<OutageList>, ApiError> {
    let Query(query) = query.map_err(|rejection| rejected(&rejection))?;
    outages(&state, query).await.map(Json)
}

/// One page of outages: the read behind `GET /api/v1/outages`.
///
/// # Errors
///
/// 422 for a bad window.
pub async fn outages(state: &AppState, query: OutageParams) -> Result<OutageList, ApiError> {
    let page = page(query.since, DEFAULT_WINDOW_MS, query.before, query.limit)?;
    let mut outages = state.store.outages(more(page)).await?;
    let next_before = trim(&mut outages, page.limit, |outage| outage.id);
    Ok(OutageList {
        since: page.since,
        outages,
        next_before,
    })
}

fn rejected(rejection: &QueryRejection) -> ApiError {
    ApiError::invalid(vec![FieldError::new("", rejection.body_text())])
}

/// The page asked for, `since` defaulting to `default_window_ms` ago.
///
/// # Errors
///
/// 422 when [`check_window`] objects.
fn page(
    since: Option<Timestamp>,
    default_window_ms: i64,
    before: Option<i64>,
    limit: Option<u32>,
) -> Result<HistoryPage, ApiError> {
    let since =
        since.unwrap_or_else(|| Timestamp::from_millis(now().as_millis() - default_window_ms));
    let limit = limit.unwrap_or(DEFAULT_LIMIT);
    let errors = check_window(since, limit);
    if !errors.is_empty() {
        return Err(ApiError::invalid(errors));
    }
    Ok(HistoryPage {
        since,
        before,
        limit,
    })
}

/// `page` with one more row than it holds: whether that row comes back
/// says whether another page follows, so the last page carries no cursor
/// even when it is exactly full.
fn more(page: HistoryPage) -> HistoryPage {
    HistoryPage {
        limit: page.limit + 1,
        ..page
    }
}

/// Cuts `rows` to `limit` and returns the cursor for the next page, if
/// one follows.
fn trim<T>(rows: &mut Vec<T>, limit: u32, id: impl Fn(&T) -> i64) -> Option<i64> {
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    rows.last().filter(|_| more).map(id)
}
