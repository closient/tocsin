// SPDX-License-Identifier: AGPL-3.0-only

//! `/api/v1/incidents`: incidents and scheduled maintenance.

use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use tocsin_core::incident::{
    Actor, FieldError, Incident, IncidentEdit, IncidentList, NewIncident, NewUpdate,
};
use tocsin_store::Drafts;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::auth::{Authorized, IncidentWriter, Viewer, scope};
use super::error::{ApiError, ApiJson, ErrorBody, OptionalJson};
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
        .routes(routes!(get_incident, edit_incident, discard_incident))
        .routes(routes!(add_update))
        .routes(routes!(publish_incident))
}

/// Which incidents `viewer` reads: drafts only when trusted.
pub fn drafts_for(viewer: Viewer) -> Drafts {
    match viewer {
        Viewer::Trusted => Drafts::Include,
        Viewer::Public => Drafts::Exclude,
    }
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

/// List incidents and maintenance, newest first. Drafts are listed only
/// to a `status:read` token.
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
    viewer: Viewer,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Result<Json<IncidentList>, ApiError> {
    let Query(query) = query
        .map_err(|rejection| ApiError::invalid(vec![FieldError::new("", rejection.body_text())]))?;
    list(&state, viewer, query).await.map(Json)
}

/// One page of incidents and maintenance `viewer` may see, newest first:
/// the read behind `GET /api/v1/incidents`.
///
/// # Errors
///
/// 422 for a `limit` out of range.
pub async fn list(
    state: &AppState,
    viewer: Viewer,
    query: ListQuery,
) -> Result<IncidentList, ApiError> {
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(ApiError::invalid(vec![FieldError::new(
            "limit",
            format!("must be 1 to {MAX_LIMIT}"),
        )]));
    }
    // One more than the page holds says whether another page follows, so
    // the last page carries no cursor even when it is exactly full.
    let mut incidents = state
        .store
        .incidents(drafts_for(viewer), query.before, limit + 1)
        .await?;
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
/// `component` that `viewer` may see, newest first: the read behind a
/// component's feed.
///
/// # Errors
///
/// 404 if no public `[[component]]` has that id; 500 if the store fails.
pub async fn naming(
    state: &AppState,
    viewer: Viewer,
    component: &str,
    limit: u32,
) -> Result<Vec<Incident>, ApiError> {
    if !state.component_is_public(component) {
        return Err(ApiError::not_found(format!("no component {component}")));
    }
    Ok(state
        .store
        .incidents_naming(drafts_for(viewer), component, limit)
        .await?)
}

/// The incident with that id if `viewer` may see it: a draft is 404 to
/// anyone but a trusted viewer, as if it did not exist.
///
/// # Errors
///
/// 404 if there is none `viewer` may see; 500 if the store fails.
pub async fn get(state: &AppState, viewer: Viewer, id: i64) -> Result<Incident, ApiError> {
    state
        .store
        .incident(id)
        .await?
        .filter(|incident| !incident.is_draft() || viewer == Viewer::Trusted)
        .ok_or_else(|| missing(id))
}

/// One incident with every update. A draft is shown only to a
/// `status:read` token.
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
    viewer: Viewer,
    id: Result<Path<i64>, PathRejection>,
) -> Result<Json<Incident>, ApiError> {
    let id = incident_id(id)?;
    get(&state, viewer, id).await.map(Json)
}

/// Open an incident or announce scheduled maintenance, published or as a
/// draft. `draft` left out publishes for an `incident` token and drafts
/// for an `incident:draft` one.
#[utoipa::path(
    post,
    path = "/api/v1/incidents",
    tag = TAG,
    security(("bearer" = [])),
    request_body = NewIncident,
    responses(
        (status = 201, description = "Created; `Location` names it", body = Incident),
        (status = 401, description = "No token, or an unknown one", body = ErrorBody),
        (status = 403, description = "The token has neither incident scope, or asks to publish (`\"draft\": false`) with only `incident:draft`", body = ErrorBody),
        (status = 422, description = "Invalid fields, by path", body = ErrorBody),
    ),
)]
pub async fn create_incident(
    State(state): State<AppState>,
    writer: IncidentWriter,
    ApiJson(new): ApiJson<NewIncident>,
) -> Result<impl IntoResponse, ApiError> {
    let incident = open(&state, new, &writer).await?;
    let location = format!("/api/v1/incidents/{}", incident.id);
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, location)],
        Json(incident),
    ))
}

/// Append an update, moving the incident to its status. An
/// `incident:draft` token may update only a draft.
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
        (status = 403, description = "The token has neither incident scope, or only `incident:draft` and the incident is published", body = ErrorBody),
        (status = 404, description = "No incident has that id", body = ErrorBody),
        (status = 422, description = "Invalid fields, by path", body = ErrorBody),
    ),
)]
pub async fn add_update(
    State(state): State<AppState>,
    writer: IncidentWriter,
    id: Result<Path<i64>, PathRejection>,
    ApiJson(update): ApiJson<NewUpdate>,
) -> Result<impl IntoResponse, ApiError> {
    let id = incident_id(id)?;
    let incident = append(&state, id, update, &writer).await?;
    Ok((StatusCode::CREATED, Json(incident)))
}

/// Opens an incident as `writer`: decides whether it is a draft, validates,
/// stores, logs, emits [`Event::IncidentCreated`] and, if it is published
/// and `new.notify` is not false, announces it to subscribers. A draft
/// announces nothing: publishing it does. The JSON API, the MCP tool and
/// the admin page all write through here.
///
/// `new.draft` left out is a draft exactly when `writer` may not publish.
///
/// # Errors
///
/// 403 if it would be published and `writer` may not publish; 422 for
/// invalid fields; 500 if the store fails.
pub async fn open(
    state: &AppState,
    mut new: NewIncident,
    writer: &IncidentWriter,
) -> Result<Arc<Incident>, ApiError> {
    let draft = *new.draft.get_or_insert(!writer.publishes);
    if !draft {
        writer.may_publish()?;
    }
    validated(
        new.validate(&|id| state.knows_component(id)),
        state,
        &new.components,
    )?;
    let notify = new.notify && !draft;
    let actor = &writer.actor;
    let incident = state
        .store
        .create_incident(new, actor.clone(), now())
        .await?;
    tracing::info!(incident = incident.id, kind = %incident.kind, %actor, draft, "incident opened");
    let incident = Arc::new(incident);
    state.events.emit(Event::IncidentCreated {
        incident: Arc::clone(&incident),
    });
    if let Some(subscribers) = state.subscribers.as_ref().filter(|_| notify) {
        subscribers.incident_opened(state, &incident).await;
    }
    Ok(incident)
}

/// Appends an update to incident `id` as `writer`: validates, stores,
/// logs, emits [`Event::IncidentUpdated`] and, if the incident is published
/// and the update does not say `notify: false`, announces it to
/// subscribers. The JSON API, the MCP tool and the admin page all write
/// through here.
///
/// # Errors
///
/// 403 if `writer` may not publish and the incident is published; 404 if
/// there is no such incident; 422 for invalid fields; 500 if the store
/// fails.
pub async fn append(
    state: &AppState,
    id: i64,
    update: NewUpdate,
    writer: &IncidentWriter,
) -> Result<Arc<Incident>, ApiError> {
    update.validate().map_err(ApiError::invalid)?;
    let status = update.status;
    let notify = update.notify;
    let actor = &writer.actor;
    let incident = state
        .store
        .add_incident_update(id, update, !writer.publishes, actor.clone(), now())
        .await
        .map_err(|error| refused(writer, error))?
        .ok_or_else(|| missing(id))?;
    tracing::info!(incident = id, %status, %actor, "incident updated");
    let incident = Arc::new(incident);
    state.events.emit(Event::IncidentUpdated {
        incident: Arc::clone(&incident),
    });
    let notify = notify && !incident.is_draft();
    if let Some(subscribers) = state.subscribers.as_ref().filter(|_| notify) {
        subscribers.incident_updated(state, &incident).await;
    }
    Ok(incident)
}

/// `POST /api/v1/incidents/{id}/publish`: whether subscribers hear about
/// it. The body may be left out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Publish {
    /// `false` publishes it quietly: shown publicly, announced to nobody.
    #[serde(default = "notify_by_default")]
    #[schema(default = true)]
    pub notify: bool,
}

impl Default for Publish {
    fn default() -> Self {
        Publish { notify: true }
    }
}

fn notify_by_default() -> bool {
    true
}

/// Publish a draft: show it publicly from now on and announce it to
/// subscribers once, as if it had just been opened. Needs the `incident`
/// scope; `incident:draft` never publishes.
#[utoipa::path(
    post,
    path = "/api/v1/incidents/{id}/publish",
    tag = TAG,
    security(("bearer" = [])),
    params(("id" = i64, Path, description = "Incident id")),
    request_body(content = Option<Publish>, description = "Optional; left out, subscribers are notified"),
    responses(
        (status = 200, description = "Published; the incident with `published_at` and `published_by`", body = Incident),
        (status = 401, description = "No token, or an unknown one", body = ErrorBody),
        (status = 403, description = "The token lacks the `incident` scope", body = ErrorBody),
        (status = 404, description = "No incident has that id", body = ErrorBody),
        (status = 409, description = "It is already published", body = ErrorBody),
    ),
)]
pub async fn publish_incident(
    State(state): State<AppState>,
    auth: Authorized<scope::Incident>,
    id: Result<Path<i64>, PathRejection>,
    OptionalJson(body): OptionalJson<Publish>,
) -> Result<Json<Arc<Incident>>, ApiError> {
    let id = incident_id(id)?;
    publish(&state, id, auth.actor, body.notify).await.map(Json)
}

/// Publishes draft `id` as `actor`: stores who and when, logs, emits
/// [`Event::IncidentPublished`] and, with `notify`, announces it to
/// subscribers once, as an opened incident. The JSON API, the admin page
/// and the internal board all publish through here; only a person or an
/// `incident` token reaches it.
///
/// # Errors
///
/// 404 if there is no such incident; 409 if it is already published; 500
/// if the store fails.
pub async fn publish(
    state: &AppState,
    id: i64,
    actor: Actor,
    notify: bool,
) -> Result<Arc<Incident>, ApiError> {
    let incident = state
        .store
        .publish_incident(id, actor.clone(), now())
        .await?
        .ok_or_else(|| missing(id))?;
    tracing::info!(incident = id, %actor, notify, "incident published");
    let incident = Arc::new(incident);
    state.events.emit(Event::IncidentPublished {
        incident: Arc::clone(&incident),
    });
    if let Some(subscribers) = state.subscribers.as_ref().filter(|_| notify) {
        subscribers.incident_opened(state, &incident).await;
    }
    Ok(incident)
}

/// Discard a draft: delete it with its updates. A published incident is
/// never deleted. Answers the draft as it was.
#[utoipa::path(
    delete,
    path = "/api/v1/incidents/{id}",
    tag = TAG,
    security(("bearer" = [])),
    params(("id" = i64, Path, description = "Incident id")),
    responses(
        (status = 200, description = "Discarded; the draft as it was", body = Incident),
        (status = 401, description = "No token, or an unknown one", body = ErrorBody),
        (status = 403, description = "The token has neither incident scope", body = ErrorBody),
        (status = 404, description = "No incident has that id", body = ErrorBody),
        (status = 409, description = "It is published, and stays", body = ErrorBody),
    ),
)]
pub async fn discard_incident(
    State(state): State<AppState>,
    writer: IncidentWriter,
    id: Result<Path<i64>, PathRejection>,
) -> Result<Json<Arc<Incident>>, ApiError> {
    let id = incident_id(id)?;
    discard(&state, id, &writer.actor).await.map(Json)
}

/// Deletes draft `id` as `actor`: logs and emits
/// [`Event::IncidentDiscarded`]. Any incident writer may discard a draft.
///
/// # Errors
///
/// 404 if there is no such incident; 409 if it is published; 500 if the
/// store fails.
pub async fn discard(state: &AppState, id: i64, actor: &Actor) -> Result<Arc<Incident>, ApiError> {
    let incident = state
        .store
        .discard_incident(id)
        .await?
        .ok_or_else(|| missing(id))?;
    tracing::info!(incident = id, %actor, "draft incident discarded");
    let incident = Arc::new(incident);
    state.events.emit(Event::IncidentDiscarded {
        incident: Arc::clone(&incident),
    });
    Ok(incident)
}

/// A drafts-only writer's change to a published incident is 403, as if the
/// route had needed `incident`; any other store error as usual.
fn refused(writer: &IncidentWriter, error: tocsin_store::Error) -> ApiError {
    match error {
        tocsin_store::Error::Published(_) if !writer.publishes => {
            writer.may_publish().err().unwrap_or_else(|| error.into())
        }
        error => error.into(),
    }
}

/// Change the title, impact, components or schedule. Fields left out are
/// unchanged; status changes only through an update. An `incident:draft`
/// token may edit only a draft.
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
        (status = 403, description = "The token has neither incident scope, or only `incident:draft` and the incident is published", body = ErrorBody),
        (status = 404, description = "No incident has that id", body = ErrorBody),
        (status = 422, description = "Invalid fields, by path", body = ErrorBody),
    ),
)]
pub async fn edit_incident(
    State(state): State<AppState>,
    writer: IncidentWriter,
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
        .edit_incident(id, edit, !writer.publishes, writer.actor.clone(), now())
        .await
        .map_err(|error| refused(&writer, error))?
        .ok_or_else(|| missing(id))?;
    tracing::info!(incident = id, actor = %writer.actor, "incident edited");
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
