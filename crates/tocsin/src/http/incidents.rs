// SPDX-License-Identifier: AGPL-3.0-only

//! `/api/v1/incidents`: incidents and scheduled maintenance.

use axum::Json;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use serde::Deserialize;
use tocsin_core::incident::{
    FieldError, Incident, IncidentEdit, IncidentList, NewIncident, NewUpdate, Timestamp,
};
use utoipa::IntoParams;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::AppState;
use super::auth::{Authorized, scope};
use super::error::{ApiError, ApiJson, ErrorBody};
use super::events::Event;

/// The OpenAPI tag for these routes.
pub const TAG: &str = "incidents";

/// Page size when `limit` is not given.
const DEFAULT_LIMIT: u32 = 20;
/// Largest page.
const MAX_LIMIT: u32 = 100;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_incidents, create_incident))
        .routes(routes!(get_incident, edit_incident))
        .routes(routes!(add_update))
}

/// `?before=&limit=` on the list.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct ListQuery {
    /// Only incidents with a smaller id: the previous page's `next_before`.
    before: Option<i64>,
    /// Page size, 1 to 100; default 20.
    limit: Option<u32>,
}

/// List incidents and maintenance, newest first.
#[utoipa::path(
    get,
    path = "/api/v1/incidents",
    tag = TAG,
    params(ListQuery),
    responses(
        (status = 200, description = "One page, newest first", body = IncidentList),
        (status = 422, description = "A bad `before` or `limit`", body = ErrorBody),
    ),
)]
pub async fn list_incidents(
    State(state): State<AppState>,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Result<Json<IncidentList>, ApiError> {
    let Query(query) = query
        .map_err(|rejection| ApiError::invalid(vec![FieldError::new("", rejection.body_text())]))?;
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(ApiError::invalid(vec![FieldError::new(
            "limit",
            format!("must be 1 to {MAX_LIMIT}"),
        )]));
    }
    let incidents = state.store.incidents(query.before, limit).await?;
    let full = incidents.len() == limit as usize;
    let next_before = incidents
        .last()
        .filter(|_| full)
        .map(|incident| incident.id);
    Ok(Json(IncidentList {
        incidents,
        next_before,
    }))
}

/// One incident with every update.
#[utoipa::path(
    get,
    path = "/api/v1/incidents/{id}",
    tag = TAG,
    params(("id" = i64, Path, description = "Incident id")),
    responses(
        (status = 200, description = "The incident", body = Incident),
        (status = 404, description = "No incident has that id", body = ErrorBody),
    ),
)]
pub async fn get_incident(
    State(state): State<AppState>,
    id: Result<Path<i64>, PathRejection>,
) -> Result<Json<Incident>, ApiError> {
    let id = incident_id(id)?;
    let incident = state.store.incident(id).await?.ok_or_else(|| missing(id))?;
    Ok(Json(incident))
}

/// Open an incident or announce scheduled maintenance.
#[utoipa::path(
    post,
    path = "/api/v1/incidents",
    tag = TAG,
    security(("bearer" = [])),
    request_body = NewIncident,
    responses(
        (status = 201, description = "Created; `Location` names it", body = Incident),
        (status = 401, description = "No token, or an unknown one", body = ErrorBody),
        (status = 403, description = "The token lacks the `incident` scope", body = ErrorBody),
        (status = 422, description = "Invalid fields, by path", body = ErrorBody),
    ),
)]
pub async fn create_incident(
    State(state): State<AppState>,
    auth: Authorized<scope::Incident>,
    ApiJson(new): ApiJson<NewIncident>,
) -> Result<impl IntoResponse, ApiError> {
    new.validate(&|id| state.knows_component(id))
        .map_err(ApiError::invalid)?;
    let incident = state
        .store
        .create_incident(new, auth.actor.clone(), now())
        .await?;
    tracing::info!(incident = incident.id, kind = %incident.kind, actor = %auth.actor, "incident opened");
    let location = format!("/api/v1/incidents/{}", incident.id);
    state.events.emit(Event::IncidentCreated {
        incident: incident.clone(),
    });
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, location)],
        Json(incident),
    ))
}

/// Append an update, moving the incident to its status.
#[utoipa::path(
    post,
    path = "/api/v1/incidents/{id}/updates",
    tag = TAG,
    security(("bearer" = [])),
    params(("id" = i64, Path, description = "Incident id")),
    request_body = NewUpdate,
    responses(
        (status = 201, description = "Appended; the incident with the new update last", body = Incident),
        (status = 401, description = "No token, or an unknown one", body = ErrorBody),
        (status = 403, description = "The token lacks the `incident` scope", body = ErrorBody),
        (status = 404, description = "No incident has that id", body = ErrorBody),
        (status = 422, description = "Invalid fields, by path", body = ErrorBody),
    ),
)]
pub async fn add_update(
    State(state): State<AppState>,
    auth: Authorized<scope::Incident>,
    id: Result<Path<i64>, PathRejection>,
    ApiJson(update): ApiJson<NewUpdate>,
) -> Result<impl IntoResponse, ApiError> {
    let id = incident_id(id)?;
    update.validate().map_err(ApiError::invalid)?;
    let status = update.status;
    let incident = state
        .store
        .add_incident_update(id, update, auth.actor.clone(), now())
        .await?
        .ok_or_else(|| missing(id))?;
    tracing::info!(incident = id, %status, actor = %auth.actor, "incident updated");
    state.events.emit(Event::IncidentUpdated {
        incident: incident.clone(),
    });
    Ok((StatusCode::CREATED, Json(incident)))
}

/// Change the title, impact, components or schedule. Fields left out are
/// unchanged; status changes only through an update.
#[utoipa::path(
    patch,
    path = "/api/v1/incidents/{id}",
    tag = TAG,
    security(("bearer" = [])),
    params(("id" = i64, Path, description = "Incident id")),
    request_body = IncidentEdit,
    responses(
        (status = 200, description = "The incident after the edit", body = Incident),
        (status = 401, description = "No token, or an unknown one", body = ErrorBody),
        (status = 403, description = "The token lacks the `incident` scope", body = ErrorBody),
        (status = 404, description = "No incident has that id", body = ErrorBody),
        (status = 422, description = "Invalid fields, by path", body = ErrorBody),
    ),
)]
pub async fn edit_incident(
    State(state): State<AppState>,
    auth: Authorized<scope::Incident>,
    id: Result<Path<i64>, PathRejection>,
    ApiJson(edit): ApiJson<IncidentEdit>,
) -> Result<Json<Incident>, ApiError> {
    let id = incident_id(id)?;
    edit.validate(&|component| state.knows_component(component))
        .map_err(ApiError::invalid)?;
    let incident = state
        .store
        .edit_incident(id, edit, auth.actor.clone(), now())
        .await?
        .ok_or_else(|| missing(id))?;
    tracing::info!(incident = id, actor = %auth.actor, "incident edited");
    state.events.emit(Event::IncidentEdited {
        incident: incident.clone(),
    });
    Ok(Json(incident))
}

/// An id that is not a number names no incident.
fn incident_id(id: Result<Path<i64>, PathRejection>) -> Result<i64, ApiError> {
    id.map(|Path(id)| id)
        .map_err(|_| ApiError::not_found("no such incident"))
}

fn missing(id: i64) -> ApiError {
    ApiError::not_found(format!("no incident {id}"))
}

fn now() -> Timestamp {
    Timestamp::from_millis(tocsin_store::now_ms())
}
