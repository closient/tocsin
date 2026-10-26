// SPDX-License-Identifier: AGPL-3.0-only

//! `/admin/`: open incidents, post updates and resolve them from a phone;
//! publish or discard drafts.
//!
//! Writes go through [`crate::http::incidents::open`],
//! [`crate::http::incidents::append`], [`crate::http::incidents::publish`]
//! and [`crate::http::incidents::discard`], the same validation, storage
//! and events as the JSON API; the author recorded is the signed-in person,
//! who may always publish. Maintenance stays with the API and CLI, which
//! take its schedule.

use std::str::FromStr;

use askama::Template;
use axum::Router;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use serde::Deserialize;
use tocsin_core::incident::{
    FieldError, Impact, Incident, IncidentKind, IncidentStatus, NewIncident, NewUpdate,
};
use tocsin_store::Drafts;

use super::form::Fields;
use super::identity::{Admin, AdminForm};
use super::markdown;
use super::page::{HtmlError, Layout, render};
use crate::http::AppState;
use crate::http::auth::IncidentWriter;
use crate::http::incidents;

/// Incidents per page of the list.
const PAGE_SIZE: u32 = 25;

/// The admin routes. Merged only when `[server.auth]` turns them on.
///
/// Forms are plain posts: success answers 303 to the page to show next,
/// and invalid input comes back as the page with errors and a 422. Each
/// path a form posts to also answers `GET` with a 303 to the page it
/// belongs to, so a reload, a bookmark or Back on it lands on a page.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/admin", get(|| async { Redirect::permanent("/admin/") }))
        .route("/admin/", get(index))
        .route(
            "/admin/incidents",
            get(|| async { Redirect::to("/admin/") }).post(create),
        )
        .route("/admin/incidents/{id}", get(detail))
        .route(
            "/admin/incidents/{id}/updates",
            get(to_incident).post(update),
        )
        .route(
            "/admin/incidents/{id}/publish",
            get(to_incident).post(publish),
        )
        .route(
            "/admin/incidents/{id}/discard",
            get(to_incident).post(discard),
        )
}

/// One option of a select or checkbox group.
struct Choice {
    value: String,
    label: String,
    selected: bool,
}

/// One incident in the list.
struct Row {
    id: i64,
    title: String,
    kind: &'static str,
    status: &'static str,
    impact: &'static str,
    created_at: String,
    over: bool,
    /// Unpublished: shown here and on the internal board only.
    draft: bool,
    created_by: String,
}

/// What the create form holds when it comes back with errors.
#[derive(Default)]
struct CreateForm {
    title: String,
    message: String,
    /// The switch was unticked; the default form has it ticked.
    quiet: bool,
    /// Save it as a draft rather than publish it.
    draft: bool,
    errors: Vec<FieldError>,
}

#[derive(Template)]
#[template(path = "admin/index.html")]
struct IndexPage {
    layout: Layout,
    csrf: String,
    incidents: Vec<Row>,
    next_before: Option<i64>,
    form: CreateForm,
    impacts: Vec<Choice>,
    statuses: Vec<Choice>,
    components: Vec<Choice>,
    /// Whether `[subscribers]` is on, to link its list.
    subscribers: bool,
    /// Whether `[slack]` is on, to link its connections.
    slack: bool,
}

/// One update in an incident's timeline.
struct UpdateView {
    status: &'static str,
    message_html: String,
    created_at: String,
    created_by: String,
}

/// The form that ends an incident: the terminal status for its kind and a
/// closing message to start from.
struct Resolve {
    status: &'static str,
    message: &'static str,
}

impl Resolve {
    fn for_kind(kind: IncidentKind) -> Self {
        match kind {
            IncidentKind::Incident => Resolve {
                status: IncidentStatus::Resolved.as_str(),
                message: "This incident has been resolved.",
            },
            IncidentKind::Maintenance => Resolve {
                status: IncidentStatus::Completed.as_str(),
                message: "This maintenance has been completed.",
            },
        }
    }
}

#[derive(Template)]
#[template(path = "admin/incident.html")]
struct IncidentPage {
    layout: Layout,
    csrf: String,
    incident: Incident,
    created_at: String,
    updates: Vec<UpdateView>,
    statuses: Vec<Choice>,
    /// How to end this incident, when it is not over.
    resolve: Option<Resolve>,
    message: String,
    errors: Vec<FieldError>,
    /// Whether subscribers exist to be told, so the form offers the switch.
    can_notify: bool,
    /// The internal note as HTML, for the people working the incident.
    note_html: Option<String>,
}

impl IncidentPage {
    /// Whether an update to this incident can notify anyone: never while
    /// it is a draft.
    fn update_notifies(&self) -> bool {
        self.can_notify && !self.incident.is_draft()
    }
}

#[derive(Deserialize)]
struct ListQuery {
    before: Option<i64>,
}

async fn index(
    State(state): State<AppState>,
    admin: Admin,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Result<Response, HtmlError> {
    let Query(query) =
        query.map_err(|_| HtmlError::new(StatusCode::BAD_REQUEST, "`before` must be a number."))?;
    index_page(
        &state,
        &admin,
        query.before,
        CreateForm::default(),
        None,
        &[],
        StatusCode::OK,
    )
    .await
}

async fn index_page(
    state: &AppState,
    admin: &Admin,
    before: Option<i64>,
    form: CreateForm,
    chosen: Option<(Impact, IncidentStatus)>,
    chosen_components: &[String],
    status: StatusCode,
) -> Result<Response, HtmlError> {
    let mut incidents = state
        .store
        .incidents(Drafts::Include, before, PAGE_SIZE + 1)
        .await
        .map_err(|error| HtmlError::internal(&error))?;
    let more = incidents.len() > PAGE_SIZE as usize;
    incidents.truncate(PAGE_SIZE as usize);
    let next_before = incidents
        .last()
        .filter(|_| more)
        .map(|incident| incident.id);
    let (impact, first_status) = chosen.unwrap_or((
        Impact::Minor,
        IncidentStatus::initial(IncidentKind::Incident),
    ));
    let page = IndexPage {
        layout: Layout::admin("Incidents", admin),
        csrf: admin.csrf_token(),
        incidents: incidents.iter().map(row).collect(),
        next_before,
        form,
        impacts: Impact::ALL
            .iter()
            .map(|value| choice(value.as_str(), value.as_str(), *value == impact))
            .collect(),
        statuses: IncidentStatus::ALL
            .iter()
            .filter(|value| value.allowed_for(IncidentKind::Incident) && !value.is_terminal())
            .map(|value| choice(value.as_str(), value.as_str(), *value == first_status))
            .collect(),
        // An incident may name only what the status page shows.
        components: state
            .config
            .components
            .iter()
            .filter(|component| state.component_is_public(component.id.as_str()))
            .map(|component| {
                choice(
                    component.id.as_str(),
                    &component.name,
                    chosen_components
                        .iter()
                        .any(|id| id == component.id.as_str()),
                )
            })
            .collect(),
        subscribers: state.subscribers.is_some(),
        slack: state.slack.is_some(),
    };
    Ok(render(status, &page))
}

async fn create(
    State(state): State<AppState>,
    AdminForm { admin, fields }: AdminForm,
) -> Result<Response, HtmlError> {
    let mut errors = Vec::new();
    let impact = choose(&fields, "impact", &mut errors).unwrap_or(Impact::Minor);
    let status = choose(&fields, "status", &mut errors)
        .unwrap_or(IncidentStatus::initial(IncidentKind::Incident));
    let components = fields.all("components");
    let form = CreateForm {
        title: fields.text("title"),
        message: fields.text("message"),
        quiet: !wants_notify(&fields),
        draft: fields.get("draft").is_some(),
        errors: Vec::new(),
    };
    if errors.is_empty() {
        let new = NewIncident {
            kind: IncidentKind::Incident,
            title: form.title.clone(),
            impact,
            components: components.clone(),
            status: Some(status),
            message: form.message.clone(),
            scheduled_start: None,
            scheduled_end: None,
            notify: !form.quiet,
            draft: Some(form.draft),
            internal_note: None,
            external_key: None,
        };
        let writer = IncidentWriter::publishing(admin.actor());
        match incidents::open(&state, new, &writer).await {
            Ok((incident, _)) => {
                return Ok(
                    Redirect::to(&format!("/admin/incidents/{}", incident.id)).into_response()
                );
            }
            Err(error) if error.status() == StatusCode::UNPROCESSABLE_ENTITY => {
                errors = error.fields().to_vec();
            }
            Err(error) => return Err(error.into()),
        }
    }
    index_page(
        &state,
        &admin,
        None,
        CreateForm { errors, ..form },
        Some((impact, status)),
        &components,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await
}

async fn detail(
    State(state): State<AppState>,
    admin: Admin,
    id: Result<Path<i64>, PathRejection>,
) -> Result<Response, HtmlError> {
    let id = id.map_err(|_| HtmlError::not_found())?.0;
    incident_page(
        &state,
        &admin,
        id,
        String::new(),
        Vec::new(),
        StatusCode::OK,
    )
    .await
}

/// `GET` on the path the update form posts to: the incident's page.
async fn to_incident(id: Result<Path<i64>, PathRejection>) -> Result<Redirect, HtmlError> {
    let id = id.map_err(|_| HtmlError::not_found())?.0;
    Ok(Redirect::to(&format!("/admin/incidents/{id}")))
}

async fn update(
    State(state): State<AppState>,
    id: Result<Path<i64>, PathRejection>,
    AdminForm { admin, fields }: AdminForm,
) -> Result<Response, HtmlError> {
    let id = id.map_err(|_| HtmlError::not_found())?.0;
    let mut errors = Vec::new();
    let message = fields.text("message");
    if let Some(status) = choose::<IncidentStatus>(&fields, "status", &mut errors) {
        let update = NewUpdate {
            status,
            message: message.clone(),
            notify: wants_notify(&fields),
        };
        let writer = IncidentWriter::publishing(admin.actor());
        match incidents::append(&state, id, update, &writer).await {
            Ok(_) => return Ok(Redirect::to(&format!("/admin/incidents/{id}")).into_response()),
            Err(error) if error.status() == StatusCode::UNPROCESSABLE_ENTITY => {
                errors = error.fields().to_vec();
            }
            Err(error) => return Err(error.into()),
        }
    }
    incident_page(
        &state,
        &admin,
        id,
        message,
        errors,
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await
}

/// Publishes a draft, from its page here or the internal board, and
/// returns to where the button was.
async fn publish(
    State(state): State<AppState>,
    id: Result<Path<i64>, PathRejection>,
    AdminForm { admin, fields }: AdminForm,
) -> Result<Redirect, HtmlError> {
    let id = id.map_err(|_| HtmlError::not_found())?.0;
    incidents::publish(&state, id, admin.actor(), wants_notify(&fields)).await?;
    Ok(Redirect::to(&back(
        &fields,
        &format!("/admin/incidents/{id}"),
    )))
}

/// Deletes a draft, from its page here or the internal board, and returns
/// to the list it was on.
async fn discard(
    State(state): State<AppState>,
    id: Result<Path<i64>, PathRejection>,
    AdminForm { admin, fields }: AdminForm,
) -> Result<Redirect, HtmlError> {
    let id = id.map_err(|_| HtmlError::not_found())?.0;
    incidents::discard(&state, id, &admin.actor()).await?;
    Ok(Redirect::to(&back(&fields, "/admin/")))
}

/// Where a publish or discard form returns to: the internal board when it
/// was posted from there (`back=internal`), `otherwise` if not. Never a
/// path the form names, so it cannot redirect off the site.
fn back(fields: &Fields, otherwise: &str) -> String {
    match fields.get("back") {
        Some("internal") => "/internal/".to_owned(),
        _ => otherwise.to_owned(),
    }
}

/// Whether the update form asks to tell subscribers. An unchecked checkbox
/// sends nothing, so the form also sends `notify_shown`: with it, a missing
/// `notify` means the box was unchecked; without it (a hand-made post, an
/// older page) the default holds and subscribers hear.
fn wants_notify(fields: &Fields) -> bool {
    fields.get("notify_shown").is_none() || fields.get("notify").is_some()
}

async fn incident_page(
    state: &AppState,
    admin: &Admin,
    id: i64,
    message: String,
    errors: Vec<FieldError>,
    status: StatusCode,
) -> Result<Response, HtmlError> {
    let incident = state
        .store
        .incident(id)
        .await
        .map_err(|error| HtmlError::internal(&error))?
        .ok_or_else(HtmlError::not_found)?;
    // Signed-in people work the incident, so they see what the public
    // never does: the internal note and the key it was opened under.
    let incident = match state
        .store
        .incident_internals(vec![id])
        .await
        .map_err(|error| HtmlError::internal(&error))?
        .remove(&id)
    {
        Some(internals) => incident.with_internals(internals),
        None => incident,
    };
    let note_html = incident.internal_note.as_deref().map(markdown::render);
    let resolve = (!incident.status.is_terminal()).then(|| Resolve::for_kind(incident.kind));
    let page = IncidentPage {
        layout: Layout::admin(incident.title.clone(), admin),
        csrf: admin.csrf_token(),
        created_at: incident.created_at.to_string(),
        updates: incident
            .updates
            .iter()
            .rev()
            .map(|update| UpdateView {
                status: update.status.as_str(),
                message_html: markdown::render(&update.message),
                created_at: update.created_at.to_string(),
                created_by: update.created_by.to_string(),
            })
            .collect(),
        statuses: IncidentStatus::ALL
            .iter()
            .filter(|value| value.allowed_for(incident.kind))
            .map(|value| choice(value.as_str(), value.as_str(), *value == incident.status))
            .collect(),
        resolve,
        message,
        errors,
        can_notify: state.subscribers.is_some(),
        note_html,
        incident,
    };
    Ok(render(status, &page))
}

fn row(incident: &Incident) -> Row {
    Row {
        id: incident.id,
        title: incident.title.clone(),
        kind: incident.kind.as_str(),
        status: incident.status.as_str(),
        impact: incident.impact.as_str(),
        created_at: incident.created_at.to_string(),
        over: incident.status.is_terminal(),
        draft: incident.is_draft(),
        created_by: incident.created_by.to_string(),
    }
}

fn choice(value: &str, label: &str, selected: bool) -> Choice {
    Choice {
        value: value.to_owned(),
        label: label.to_owned(),
        selected,
    }
}

/// Field `name` parsed as `T`, or an error at that field.
fn choose<T: FromStr<Err = String>>(
    fields: &Fields,
    name: &str,
    errors: &mut Vec<FieldError>,
) -> Option<T> {
    match fields.get(name).unwrap_or_default().parse() {
        Ok(value) => Some(value),
        Err(message) => {
            errors.push(FieldError::new(name, message));
            None
        }
    }
}
