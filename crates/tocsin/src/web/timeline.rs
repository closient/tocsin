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
use jiff::tz::TimeZone;
use serde::Deserialize;
use tocsin_core::config::{Config, QUORUM_NODE};
use tocsin_core::release::{Release, TimelineEvent};
use tocsin_core::status::OutageEnding;
use tocsin_store::Order;

use super::identity::Admin;
use super::page::{HtmlError, Layout, render};
use super::releases::{Stamp, project_name};
use super::status::component_name;
use crate::http::AppState;
use crate::http::auth::Viewer;
use crate::http::timeline::{self, MAX_LIMIT, Window};

/// The admin timeline route. Merged only when `[server.auth]` turns the
/// admin pages on.
pub fn router() -> Router<AppState> {
    Router::new().route("/admin/timeline", get(page))
}

/// One event, ready to show. The status page's component page shows the
/// same rows.
pub(super) struct Row {
    pub at: Stamp,
    pub kind: &'static str,
    /// A badge class: the event's severity or state.
    pub tone: String,
    pub text: String,
    pub href: Option<String>,
    pub detail: String,
}

/// Where a row links and how it shows instants.
pub(super) struct Rows<'a> {
    /// Incident links: `{incidents}/{id}`.
    pub incidents: &'a str,
    /// The zone instants are shown in.
    pub zone: &'a TimeZone,
    /// Where checks, components and projects get their display names.
    pub config: &'a Config,
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
    // The newest page: what happened most recently is what an operator
    // opened this for.
    let window = Window {
        component: component.as_deref(),
        since,
        after: None,
        order: Order::NewestFirst,
        limit: MAX_LIMIT,
    };
    let found = timeline::read(&state, Viewer::Trusted, window).await?;
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
        events: found
            .events
            .iter()
            .map(|event| {
                row(
                    event,
                    &Rows {
                        incidents: "/admin/incidents",
                        zone: &TimeZone::UTC,
                        config: &state.config,
                    },
                )
            })
            .collect(),
        truncated: found.truncated,
    };
    Ok(render(StatusCode::OK, &page))
}

pub(super) fn row(event: &TimelineEvent, rows: &Rows<'_>) -> Row {
    match event {
        TimelineEvent::Outage {
            at,
            node_id,
            check_id,
            component_id,
            health,
            ended_at,
            ending,
            ..
        } => Row {
            at: Stamp::zoned(*at, rows.zone),
            kind: "outage",
            tone: health.to_string(),
            text: format!("{} {health}", check_name(rows.config, check_id)),
            href: None,
            detail: format!(
                "on {}{}{}",
                if node_id == QUORUM_NODE {
                    "a quorum of nodes"
                } else {
                    node_id
                },
                component_id
                    .as_deref()
                    .map_or_else(String::new, |component| {
                        format!(" · {}", component_name(rows.config, component))
                    }),
                match (ended_at, ending) {
                    (None, _) => " · ongoing".to_owned(),
                    // Removed from the configuration while out: nobody saw
                    // it recover.
                    (Some(ended), Some(OutageEnding::Withdrawn)) => {
                        format!(" · withdrawn {}", super::page::when_in(*ended, rows.zone))
                    }
                    (Some(ended), _) => {
                        format!(" · ended {}", super::page::when_in(*ended, rows.zone))
                    }
                },
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
            at: Stamp::zoned(*at, rows.zone),
            kind: incident_kind.as_str(),
            tone: impact.as_str().to_owned(),
            text: title.clone(),
            href: Some(format!("{}/{id}", rows.incidents)),
            detail: format!(
                "{status} · {}",
                components
                    .iter()
                    .map(|component| component_name(rows.config, component))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        },
        TimelineEvent::Deploy { at, release, .. } => {
            release_row(Stamp::zoned(*at, rows.zone), "deploy", release, rows.config)
        }
        TimelineEvent::Release { at, release, .. } => release_row(
            Stamp::zoned(*at, rows.zone),
            "release",
            release,
            rows.config,
        ),
    }
}

/// `[[check]]` or `[[heartbeat]]` `id`'s display name; the id if it has
/// none, or is no longer configured.
fn check_name(config: &Config, id: &str) -> String {
    config
        .checks
        .iter()
        .find(|check| check.id.as_str() == id)
        .map(|check| check.name.as_ref())
        .or_else(|| {
            config
                .heartbeats
                .iter()
                .find(|heartbeat| heartbeat.id.as_str() == id)
                .map(|heartbeat| heartbeat.name.as_ref())
        })
        .flatten()
        .map_or_else(|| id.to_owned(), Clone::clone)
}

fn release_row(at: Stamp, kind: &'static str, release: &Release, config: &Config) -> Row {
    Row {
        at,
        kind,
        tone: String::new(),
        text: format!(
            "{} {} to {}",
            project_name(config, &release.project),
            release.version,
            release.environment
        ),
        href: None,
        detail: if release.released_at.is_some() {
            release.display_title().to_owned()
        } else {
            "deployed before it was published".to_owned()
        },
    }
}
