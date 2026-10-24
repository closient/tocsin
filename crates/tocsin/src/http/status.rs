// SPDX-License-Identifier: AGPL-3.0-only

//! `/api/v1/status.json`, `/api/v1/components/{id}` and `/api/v1/checks`:
//! what is up right now.
//!
//! Components are public, as the status page shows them, and so are the
//! checks and heartbeats that belong to one ([`public`]). One on no
//! component, and any check's `target`, which can name an internal host,
//! only a `status:read` token sees.

use axum::Json;
use axum::extract::{Path, State};
use tocsin_core::config::{Component, Config, Id};
use tocsin_core::incident::{Incident, Timestamp};
use tocsin_core::state::ComponentStatus;
use tocsin_core::status::{
    CheckKind, CheckList, CheckState, ComponentDetail, ComponentState, StatusDocument, overall,
};
use tocsin_engine::Snapshot;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::auth::Viewer;
use super::error::{ApiError, ErrorBody};
use super::public;
use super::{AppState, now};

/// The OpenAPI tag for these routes.
pub const TAG: &str = "status";

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(get_status))
        .routes(routes!(get_component))
        .routes(routes!(list_checks))
}

/// The page's overall status, every component's, and the open incidents.
#[utoipa::path(
    get,
    path = "/api/v1/status.json",
    tag = TAG,
    security((), ("bearer" = [])),
    responses(
        (status = 200, description = "The status now", body = StatusDocument),
        (status = 401, description = "An `Authorization` header with an unknown token", body = ErrorBody),
    ),
)]
pub async fn get_status(
    State(state): State<AppState>,
    _viewer: Viewer,
) -> Result<Json<StatusDocument>, ApiError> {
    document(&state).await.map(Json)
}

/// One component: its status, its checks and heartbeats, and the open
/// incidents naming it.
#[utoipa::path(
    get,
    path = "/api/v1/components/{id}",
    tag = TAG,
    params(("id" = String, Path, description = "Component id")),
    security((), ("bearer" = [])),
    responses(
        (status = 200, description = "The component", body = ComponentDetail),
        (status = 401, description = "An `Authorization` header with an unknown token", body = ErrorBody),
        (status = 404, description = "No `[[component]]` has that id", body = ErrorBody),
    ),
)]
pub async fn get_component(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Json<ComponentDetail>, ApiError> {
    component_detail(&state, viewer, &id).await.map(Json)
}

/// Component `id` as `viewer` may see it: its status, its checks and
/// heartbeats, and the open incidents naming it. The API route and the
/// status page's component page both read through here.
///
/// # Errors
///
/// 404 if no `[[component]]` has that id; 500 if the store fails.
pub async fn component_detail(
    state: &AppState,
    viewer: Viewer,
    id: &str,
) -> Result<ComponentDetail, ApiError> {
    let component = state
        .config
        .components
        .iter()
        .find(|component| component.id.as_str() == id)
        .ok_or_else(|| ApiError::not_found(format!("no component {id}")))?;
    let snapshot = state.live.snapshot();
    let incidents = state
        .store
        .unresolved_incidents()
        .await?
        .into_iter()
        .filter(|incident| incident.components.iter().any(|named| named == id))
        .collect();
    Ok(ComponentDetail {
        component: component_state(component, &snapshot),
        checks: checks(&state.config, &snapshot, viewer)
            .into_iter()
            .filter(|check| check.component.as_deref() == Some(id))
            .collect(),
        incidents,
    })
}

/// Every check and heartbeat on a public component with its current
/// state; with a `status:read` token, those on no component too.
#[utoipa::path(
    get,
    path = "/api/v1/checks",
    tag = TAG,
    security((), ("bearer" = [])),
    responses(
        (status = 200, description = "Checks, then heartbeats", body = CheckList),
        (status = 401, description = "An `Authorization` header with an unknown token", body = ErrorBody),
    ),
)]
pub async fn list_checks(State(state): State<AppState>, viewer: Viewer) -> Json<CheckList> {
    let snapshot = state.live.snapshot();
    Json(CheckList {
        checks: checks(&state.config, &snapshot, viewer),
    })
}

/// The status document: every component in configuration order joined
/// with the live state, and the incidents not yet resolved.
///
/// # Errors
///
/// 500 if the store fails.
pub async fn document(state: &AppState) -> Result<StatusDocument, ApiError> {
    let snapshot = state.live.snapshot();
    let components: Vec<ComponentState> = state
        .config
        .components
        .iter()
        .map(|component| component_state(component, &snapshot))
        .collect();
    let incidents: Vec<Incident> = state.store.unresolved_incidents().await?;
    Ok(StatusDocument {
        title: state.config.page.title.clone(),
        status: overall(components.iter().map(|component| component.status)),
        generated_at: now(),
        components,
        incidents,
    })
}

/// `component` as the snapshot has it; unknown when the engine has no
/// state for it (not started, or stopped).
fn component_state(component: &Component, snapshot: &Snapshot) -> ComponentState {
    let live = snapshot
        .components
        .iter()
        .find(|live| live.id == component.id.as_str());
    ComponentState {
        id: component.id.as_str().to_owned(),
        name: component.name.clone(),
        description: component.description.clone(),
        group: component
            .group
            .as_ref()
            .map(|group| group.as_str().to_owned()),
        status: live.map_or(ComponentStatus::Unknown, |live| live.status),
        health: live.and_then(|live| live.health),
    }
}

/// Every check, then every heartbeat, in configuration order, with what
/// the snapshot knows of each. Only a [`Viewer::Trusted`] sees targets,
/// and those on no component ([`public::may_see`]).
pub(crate) fn checks(config: &Config, snapshot: &Snapshot, viewer: Viewer) -> Vec<CheckState> {
    let trusted = viewer == Viewer::Trusted;
    let probed = config.checks.iter().map(|check| Member {
        id: &check.id,
        kind: CheckKind::Check,
        name: check.name.as_deref(),
        component: check.component.as_ref(),
        check_type: Some(check.check_type.as_str()),
        interval: check.interval.to_string(),
        target: trusted.then(|| check.target.clone()),
    });
    let pinged = config.heartbeats.iter().map(|heartbeat| Member {
        id: &heartbeat.id,
        kind: CheckKind::Heartbeat,
        name: heartbeat.name.as_deref(),
        component: heartbeat.component.as_ref(),
        check_type: None,
        interval: heartbeat.interval.to_string(),
        target: None,
    });
    probed
        .chain(pinged)
        .filter(|member| public::may_see(viewer, member.component.map(Id::as_str)))
        .map(|member| member.state(snapshot))
        .collect()
}

/// A check or heartbeat as configured.
struct Member<'a> {
    id: &'a Id,
    kind: CheckKind,
    name: Option<&'a str>,
    component: Option<&'a Id>,
    check_type: Option<&'static str>,
    interval: String,
    target: Option<String>,
}

impl Member<'_> {
    /// This member joined with its live state, if the snapshot has one.
    fn state(self, snapshot: &Snapshot) -> CheckState {
        let id = self.id.as_str();
        let live = snapshot.checks.iter().find(|live| live.id == id);
        let confirmed = live.and_then(|live| live.state);
        let last = live.and_then(|live| live.last);
        CheckState {
            id: id.to_owned(),
            kind: self.kind,
            name: self.name.unwrap_or(id).to_owned(),
            component: self
                .component
                .map(|component| component.as_str().to_owned()),
            check_type: self.check_type.map(str::to_owned),
            interval: self.interval,
            target: self.target,
            health: confirmed.map(|state| state.health),
            since: confirmed.map(|state| Timestamp::from_millis(state.since_ms)),
            last_health: last.map(|last| last.health),
            last_result_at: last.map(|last| Timestamp::from_millis(last.started_at_ms)),
        }
    }
}

/// Whether `[[check]]` or `[[heartbeat]]` `id` is configured.
pub fn knows_check(config: &Config, id: &str) -> bool {
    config.checks.iter().any(|check| check.id.as_str() == id)
        || config
            .heartbeats
            .iter()
            .any(|heartbeat| heartbeat.id.as_str() == id)
}
