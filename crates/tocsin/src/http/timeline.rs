// SPDX-License-Identifier: AGPL-3.0-only

//! `/api/v1/timeline`: outages, incidents and deploys in one stream.
//!
//! [`read`] is the one way in: the API route, the admin timeline and the
//! status page's component history all call it, so the window bound, the
//! limit and the visibility rule are the same everywhere.

use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use serde::Deserialize;
use tocsin_core::config::Config;
use tocsin_core::incident::{FieldError, Timestamp};
use tocsin_core::release::Timeline;
use tocsin_store::{ComponentScope, TimelineQuery};
use utoipa::IntoParams;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::auth::Viewer;
use super::error::{ApiError, ErrorBody};
use super::releases::visible_projects;
use super::{AppState, now};

/// The OpenAPI tag for this route.
pub const TAG: &str = "timeline";

/// How far back `since` reaches when it is not given: 30 days.
pub const DEFAULT_WINDOW_MS: i64 = 30 * DAY_MS;
/// How far back `since` may reach: 366 days.
pub const MAX_WINDOW_MS: i64 = 366 * DAY_MS;
/// Events when `limit` is not given.
pub const DEFAULT_LIMIT: u32 = 100;
/// The most events one read returns.
pub const MAX_LIMIT: u32 = 500;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(get_timeline))
}

/// `?component=&since=&limit=` on the timeline.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct TimelineParams {
    /// Only events touching this `[[component]]`: outages of its checks,
    /// incidents naming it, deploys of projects that list it.
    component: Option<String>,
    /// The start of the window (RFC 3339): at most 366 days ago; default
    /// 30 days ago.
    since: Option<Timestamp>,
    /// Events, 1 to 500; default 100.
    limit: Option<u32>,
}

/// Outages, incidents and deploys, oldest first, read in one snapshot.
/// Ties on time order by kind (outage, incident, deploy, release), then id.
/// Without a `status:read` token only public projects' deploys appear.
#[utoipa::path(
    get,
    path = "/api/v1/timeline",
    tag = TAG,
    params(TimelineParams),
    security((), ("bearer" = [])),
    responses(
        (status = 200, description = "Oldest first", body = Timeline),
        (status = 401, description = "An `Authorization` header with an unknown token", body = ErrorBody),
        (status = 403, description = "A token without the `status:read` scope", body = ErrorBody),
        (status = 422, description = "An unknown component, or a bad `since` or `limit`", body = ErrorBody),
    ),
)]
pub async fn get_timeline(
    State(state): State<AppState>,
    viewer: Viewer,
    query: Result<Query<TimelineParams>, QueryRejection>,
) -> Result<Json<Timeline>, ApiError> {
    let Query(query) = query
        .map_err(|rejection| ApiError::invalid(vec![FieldError::new("", rejection.body_text())]))?;
    let since = query.since.unwrap_or_else(default_since);
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    read(&state, viewer, query.component.as_deref(), since, limit)
        .await
        .map(Json)
}

/// The default window's start: [`DEFAULT_WINDOW_MS`] ago.
pub fn default_since() -> Timestamp {
    Timestamp::from_millis(now().as_millis() - DEFAULT_WINDOW_MS)
}

/// The timeline as `viewer` sees it: events at or after `since`, at most
/// `limit`, touching `component` when one is given.
///
/// # Errors
///
/// 422 for an unknown component, a `since` more than [`MAX_WINDOW_MS`]
/// ago, or a `limit` outside 1 to [`MAX_LIMIT`]; 500 if the store fails.
pub async fn read(
    state: &AppState,
    viewer: Viewer,
    component: Option<&str>,
    since: Timestamp,
    limit: u32,
) -> Result<Timeline, ApiError> {
    let mut errors = Vec::new();
    let earliest = now().as_millis() - MAX_WINDOW_MS;
    if since.as_millis() < earliest {
        errors.push(FieldError::new(
            "since",
            format!(
                "must be within the last {} days, so at or after {}",
                MAX_WINDOW_MS / DAY_MS,
                Timestamp::from_millis(earliest)
            ),
        ));
    }
    if !(1..=MAX_LIMIT).contains(&limit) {
        errors.push(FieldError::new(
            "limit",
            format!("must be 1 to {MAX_LIMIT}"),
        ));
    }
    let component = match component {
        Some(id) if !state.knows_component(id) => {
            errors.push(FieldError::new(
                "component",
                format!("no [[component]] has id {id:?}"),
            ));
            None
        }
        Some(id) => Some(component_scope(&state.config, id)),
        None => None,
    };
    if !errors.is_empty() {
        return Err(ApiError::invalid(errors));
    }
    Ok(state
        .store
        .timeline(TimelineQuery {
            visible: visible_projects(&state.config, viewer),
            since,
            limit,
            component,
        })
        .await?)
}

/// What belongs to component `id` in the configuration: the checks and
/// heartbeats that set its health, and the projects that list it.
pub fn component_scope(config: &Config, id: &str) -> ComponentScope {
    let owns = |component: Option<&tocsin_core::config::Id>| {
        component.is_some_and(|component| component.as_str() == id)
    };
    ComponentScope {
        id: id.to_owned(),
        checks: config
            .checks
            .iter()
            .filter(|check| owns(check.component.as_ref()))
            .map(|check| check.id.as_str().to_owned())
            .chain(
                config
                    .heartbeats
                    .iter()
                    .filter(|heartbeat| owns(heartbeat.component.as_ref()))
                    .map(|heartbeat| heartbeat.id.as_str().to_owned()),
            )
            .collect(),
        projects: config
            .projects
            .iter()
            .filter(|project| project.components.iter().any(|c| c.as_str() == id))
            .map(|project| project.id.as_str().to_owned())
            .collect(),
    }
}
