// SPDX-License-Identifier: AGPL-3.0-only

//! `/api/v1/incidents`: incidents and scheduled maintenance.

use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use serde::Deserialize;
use tocsin_core::incident::{
    Actor, FieldError, Incident, IncidentEdit, IncidentList, NewIncident, NewUpdate,
};
use utoipa::IntoParams;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::auth::{Authorized, Viewer, scope};
use super::error::{ApiError, ApiJson, ErrorBody};
use super::events::Event;
use super::{AppState, now};

/// The OpenAPI tag for these routes.
pub const TAG: &str = "incidents";

/// Page size when `limit` is not given.
pub const DEFAULT_LIMIT: u32 = 20;
/// Largest page.
pub const MAX_LIMIT: u32 = 100;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_incidents, create_incident))
        .routes(routes!(get_incident, edit_incident))
        .routes(routes!(add_update))
}

/// `?before=&limit=` on the list.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct ListQuery {
    /// Only incidents with a smaller id: the previous page's `next_before`.
    pub before: Option<i64>,
    /// Page size, 1 to 100; default 20.
    pub limit: Option<u32>,
}

/// List incidents and maintenance, newest first.
#[utoipa::path(
    get,
    path = "/api/v1/incidents",
    tag = TAG,
    params(ListQuery),
    security((), ("bearer" = [])),
    responses(
        (status = 200, description = "One page, newest first", body = IncidentList),
        (status = 401, description = "An `Authorization` header with an unknown token", body = ErrorBody),
        (status = 422, description = "A bad `before` or `limit`", body = ErrorBody),
    ),
)]
pub async fn list_incidents(
    State(state): State<AppState>,
    _viewer: Viewer,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Result<Json<IncidentList>, ApiError> {
    let Query(query) = query
        .map_err(|rejection| ApiError::invalid(vec![FieldError::new("", rejection.body_text())]))?;
    list(&state, query).await.map(Json)
}

/// One page of incidents and maintenance, newest first: the read behind
/// `GET /api/v1/incidents`.
///
/// # Errors
///
/// 422 for a `limit` out of range.
pub async fn list(state: &AppState, query: ListQuery) -> Result<IncidentList, ApiError> {
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(ApiError::invalid(vec![FieldError::new(
            "limit",
            format!("must be 1 to {MAX_LIMIT}"),
        )]));
    }
    // One more than the page holds says whether another page follows, so
    // the last page carries no cursor even when it is exactly full.
    let mut incidents = state.store.incidents(query.before, limit + 1).await?;
    let more = incidents.len() > limit as usize;
    incidents.truncate(limit as usize);
    let next_before = incidents
        .last()
        .filter(|_| more)
        .map(|incident| incident.id);
    Ok(IncidentList {
        incidents,
        next_before,
    })
}

/// Up to `limit` incidents and maintenance naming `[[component]]`
/// `component`, newest first: the read behind a component's feed.
///
/// # Errors
///
/// 404 if no public `[[component]]` has that id; 500 if the store fails.
pub async fn naming(
    state: &AppState,
    component: &str,
    limit: u32,
) -> Result<Vec<Incident>, ApiError> {
    if !state.component_is_public(component) {
        return Err(ApiError::not_found(format!("no component {component}")));
    }
    Ok(state.store.incidents_naming(component, limit).await?)
}

/// One incident with every update.
#[utoipa::path(
    get,
    path = "/api/v1/incidents/{id}",
    tag = TAG,
    params(("id" = i64, Path, description = "Incident id")),
    security((), ("bearer" = [])),
    responses(
        (status = 200, description = "The incident", body = Incident),
        (status = 401, description = "An `Authorization` header with an unknown token", body = ErrorBody),
        (status = 404, description = "No incident has that id", body = ErrorBody),
    ),
)]
pub async fn get_incident(
    State(state): State<AppState>,
    _viewer: Viewer,
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
    let incident = open(&state, new, auth.actor).await?;
    let location = format!("/api/v1/incidents/{}", incident.id);
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
    let incident = append(&state, id, update, auth.actor).await?;
    Ok((StatusCode::CREATED, Json(incident)))
}

/// Opens an incident as `actor`: validates, stores, logs, emits
/// [`Event::IncidentCreated`] and, unless `new.notify` is false, announces it
/// to subscribers. The JSON API, the MCP tool and the admin page all write
/// through here.
///
/// # Errors
///
/// 422 for invalid fields; 500 if the store fails.
pub async fn open(
    state: &AppState,
    new: NewIncident,
    actor: Actor,
) -> Result<Arc<Incident>, ApiError> {
    validated(
        new.validate(&|id| state.knows_component(id)),
        state,
        &new.components,
    )?;
    let notify = new.notify;
    let incident = state
        .store
        .create_incident(new, actor.clone(), now())
        .await?;
    tracing::info!(incident = incident.id, kind = %incident.kind, %actor, "incident opened");
    let incident = Arc::new(incident);
    state.events.emit(Event::IncidentCreated {
        incident: Arc::clone(&incident),
    });
    if let Some(subscribers) = state.subscribers.as_ref().filter(|_| notify) {
        subscribers.incident_opened(state, &incident).await;
    }
    Ok(incident)
}

/// Appends an update to incident `id` as `actor`: validates, stores, logs,
/// emits [`Event::IncidentUpdated`] and, unless the update says
/// `notify: false`, announces it to subscribers. The JSON API and the admin
/// page both write through here.
///
/// # Errors
///
/// 404 if there is no such incident; 422 for invalid fields; 500 if the
/// store fails.
pub async fn append(
    state: &AppState,
    id: i64,
    update: NewUpdate,
    actor: Actor,
) -> Result<Arc<Incident>, ApiError> {
    update.validate().map_err(ApiError::invalid)?;
    let status = update.status;
    let notify = update.notify;
    let incident = state
        .store
        .add_incident_update(id, update, actor.clone(), now())
        .await?
        .ok_or_else(|| missing(id))?;
    tracing::info!(incident = id, %status, %actor, "incident updated");
    let incident = Arc::new(incident);
    state.events.emit(Event::IncidentUpdated {
        incident: Arc::clone(&incident),
    });
    if let Some(subscribers) = state.subscribers.as_ref().filter(|_| notify) {
        subscribers.incident_updated(state, &incident).await;
    }
    Ok(incident)
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
) -> Result<Json<Arc<Incident>>, ApiError> {
    let id = incident_id(id)?;
    validated(
        edit.validate(&|component| state.knows_component(component)),
        &state,
        edit.components.as_deref().unwrap_or_default(),
    )?;
    let incident = state
        .store
        .edit_incident(id, edit, auth.actor.clone(), now())
        .await?
        .ok_or_else(|| missing(id))?;
    tracing::info!(incident = id, actor = %auth.actor, "incident edited");
    let incident = Arc::new(incident);
    state.events.emit(Event::IncidentEdited {
        incident: Arc::clone(&incident),
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

/// `checked`, the incident's or edit's own validation, with an error for
/// each of `components` that is configured but not public: whatever an
/// incident says is public, so it may name only what the status page
/// shows.
fn validated(
    checked: Result<(), Vec<FieldError>>,
    state: &AppState,
    components: &[String],
) -> Result<(), ApiError> {
    let mut errors = checked.err().unwrap_or_default();
    for (index, id) in components.iter().enumerate() {
        if state.knows_component(id) && !state.component_is_public(id) {
            errors.push(FieldError::new(
                format!("components[{index}]"),
                format!(
                    "[[component]] {id:?} is not public; an incident or maintenance may name \
                     only a public component"
                ),
            ));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ApiError::invalid(errors))
    }
}
