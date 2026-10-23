// SPDX-License-Identifier: AGPL-3.0-only

//! `/admin/timeline`: outages, incidents and deploys in one list, every
//! project included, for the signed-in operator. Reads through
//! [`crate::http::timeline::read`], the same as the API.

use askama::Template;
use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use serde::Deserialize;
use tocsin_core::release::{Release, TimelineEvent};

use super::identity::Admin;
use super::page::{HtmlError, Layout, render};
use super::releases::Stamp;
use crate::http::AppState;
use crate::http::auth::Viewer;
use crate::http::timeline::{self, MAX_LIMIT};

/// The admin timeline route. Merged only when `[server.auth]` turns the
/// admin pages on.
pub fn router() -> Router<AppState> {
    Router::new().route("/admin/timeline", get(page))
}

/// One event, ready to show.
struct Row {
    at: Stamp,
    kind: &'static str,
    /// A badge class: the event's severity or state.
    tone: String,
    text: String,
    href: Option<String>,
    detail: String,
}

/// One component in the filter.
struct ComponentChoice {
    id: String,
    name: String,
    selected: bool,
}

#[derive(Template)]
#[template(path = "admin/timeline.html")]
struct TimelinePage {
    layout: Layout,
    components: Vec<ComponentChoice>,
    since: Stamp,
    events: Vec<Row>,
    truncated: bool,
}

#[derive(Deserialize)]
struct Filter {
    component: Option<String>,
}

async fn page(
    State(state): State<AppState>,
    admin: Admin,
    query: Result<Query<Filter>, QueryRejection>,
) -> Result<Response, HtmlError> {
    let Query(filter) = query
        .map_err(|_| HtmlError::new(StatusCode::BAD_REQUEST, "`component` must be one value."))?;
    let component = filter.component.filter(|id| !id.is_empty());
    let since = timeline::default_since();
    let found = timeline::read(
        &state,
        Viewer::Trusted,
        component.as_deref(),
        since,
        MAX_LIMIT,
    )
    .await?;
    let page = TimelinePage {
        layout: Layout::admin("Timeline", &admin),
        components: state
            .config
            .components
            .iter()
            .map(|candidate| ComponentChoice {
                id: candidate.id.as_str().to_owned(),
                name: candidate.name.clone(),
                selected: component.as_deref() == Some(candidate.id.as_str()),
            })
            .collect(),
        since: Stamp::new(found.since),
        // Newest first, as the incident list reads.
        events: found.events.iter().rev().map(row).collect(),
        truncated: found.truncated,
    };
    Ok(render(StatusCode::OK, &page))
}

fn row(event: &TimelineEvent) -> Row {
    match event {
        TimelineEvent::Outage {
            at,
            node_id,
            check_id,
            component_id,
            health,
            ended_at,
            ..
        } => Row {
            at: Stamp::new(*at),
            kind: "outage",
            tone: health.to_string(),
            text: format!("{check_id} {health}"),
            href: None,
            detail: format!(
                "on {node_id}{}{}",
                component_id
                    .as_deref()
                    .map_or_else(String::new, |component| format!(" · {component}")),
                ended_at.map_or_else(
                    || " · ongoing".to_owned(),
                    |ended| format!(" · ended {}", super::page::when(ended))
                ),
            ),
        },
        TimelineEvent::Incident {
            at,
            id,
            incident_kind,
            title,
            impact,
            status,
            components,
        } => Row {
            at: Stamp::new(*at),
            kind: incident_kind.as_str(),
            tone: impact.as_str().to_owned(),
            text: title.clone(),
            href: Some(format!("/admin/incidents/{id}")),
            detail: format!("{status} · {}", components.join(", ")),
        },
        TimelineEvent::Deploy { at, release, .. } => release_row(*at, "deploy", release),
        TimelineEvent::Release { at, release, .. } => release_row(*at, "release", release),
    }
}

fn release_row(at: tocsin_core::incident::Timestamp, kind: &'static str, release: &Release) -> Row {
    Row {
        at: Stamp::new(at),
        kind,
        tone: String::new(),
        text: format!(
            "{} {} to {}",
            release.project, release.version, release.environment
        ),
        href: None,
        detail: if release.released_at.is_some() {
            release.display_title().to_owned()
        } else {
            "deployed before it was published".to_owned()
        },
    }
}
