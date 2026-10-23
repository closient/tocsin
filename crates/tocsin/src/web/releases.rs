// SPDX-License-Identifier: AGPL-3.0-only

//! The public release log: `/releases`, one release at
//! `/releases/{project}/{version}`, and the Atom feed at `/releases.atom`.
//!
//! These pages show what an anonymous reader of the API sees: public
//! projects only, through [`crate::http::releases::visible_projects`]. They
//! load no script. Notes are Markdown, shown only through
//! [`markdown::render`]; titles and every other value are escaped by
//! askama.

use askama::Template;
use axum::Router;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;
use tocsin_core::incident::Timestamp;
use tocsin_core::release::{DEFAULT_ENVIRONMENT, Release};
use tocsin_store::ReleaseQuery;

use super::markdown;
use super::page::{HtmlError, Layout, render, when};
use crate::http::AppState;
use crate::http::auth::Viewer;
use crate::http::releases::visible_projects;

/// Releases on the list page.
const PAGE_SIZE: u32 = 50;
/// Entries in the feed.
const FEED_SIZE: u32 = 20;

/// The public release routes, always on.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/releases", get(list))
        .route("/releases.atom", get(feed))
        .route("/releases/{project}/{version}", get(detail))
}

/// An instant as a page shows it: for a person, and exact.
pub struct Stamp {
    /// `2026-10-23 14:05 UTC`.
    pub text: String,
    /// RFC 3339, for `<time datetime>`.
    pub iso: String,
}

impl Stamp {
    pub fn new(at: Timestamp) -> Self {
        Stamp {
            text: when(at),
            iso: at.to_string(),
        }
    }
}

/// One project in the filter.
struct ProjectChoice {
    id: String,
    name: String,
    selected: bool,
}

/// One release in the list.
struct Row {
    href: String,
    project: String,
    version: String,
    title: String,
    environment: Option<String>,
    released_at: Option<Stamp>,
    deployed_at: Option<Stamp>,
}

#[derive(Template)]
#[template(path = "releases/list.html")]
struct ListPage {
    layout: Layout,
    projects: Vec<ProjectChoice>,
    feed: String,
    releases: Vec<Row>,
    truncated: bool,
}

/// One environment a release went to.
struct Deploy {
    environment: String,
    deployed_at: Option<Stamp>,
}

#[derive(Template)]
#[template(path = "releases/detail.html")]
struct DetailPage {
    layout: Layout,
    project: String,
    project_href: String,
    version: String,
    title: String,
    released_at: Option<Stamp>,
    url: Option<String>,
    notes_html: String,
    deploys: Vec<Deploy>,
}

/// One entry in the feed.
struct Entry {
    id: String,
    link: String,
    title: String,
    updated: String,
    published: String,
    notes_html: String,
}

#[derive(Template)]
#[template(path = "releases/feed.xml")]
struct Feed {
    id: String,
    title: String,
    self_link: String,
    alternate: String,
    updated: String,
    entries: Vec<Entry>,
}

/// `?project=` on the list and the feed; empty means every project.
#[derive(Deserialize)]
struct ProjectFilter {
    project: Option<String>,
}

impl ProjectFilter {
    /// The project asked for, if any. One that is not configured and
    /// public is a 404, so a private project looks like no project.
    fn project(self, state: &AppState) -> Result<Option<String>, HtmlError> {
        let project = self.project.filter(|project| !project.is_empty());
        if project
            .as_deref()
            .is_some_and(|id| !state.project_is_public(id))
        {
            return Err(HtmlError::not_found());
        }
        Ok(project)
    }
}

/// The public view of `project`'s published releases, or every public
/// project's, newest first; whether more matched than `limit`.
async fn visible(
    state: &AppState,
    project: Option<String>,
    version: Option<String>,
    limit: u32,
) -> Result<(Vec<Release>, bool), HtmlError> {
    state
        .store
        .releases(ReleaseQuery {
            visible: visible_projects(&state.config, Viewer::Public),
            project,
            version,
            since: None,
            limit,
        })
        .await
        .map_err(|error| HtmlError::internal(&error))
}

/// `[[project]]` `id`'s display name; the id if it has none configured.
fn project_name(state: &AppState, id: &str) -> String {
    state
        .config
        .projects
        .iter()
        .find(|project| project.id.as_str() == id)
        .map_or_else(|| id.to_owned(), |project| project.name.clone())
}

fn href(release: &Release) -> String {
    format!("/releases/{}/{}", release.project, release.version)
}

fn filter_query(project: Option<&str>) -> String {
    project.map_or_else(String::new, |project| format!("?project={project}"))
}

async fn list(
    State(state): State<AppState>,
    query: Result<Query<ProjectFilter>, QueryRejection>,
) -> Result<Response, HtmlError> {
    let Query(filter) = query
        .map_err(|_| HtmlError::new(StatusCode::BAD_REQUEST, "`project` must be one value."))?;
    let project = filter.project(&state)?;
    let (releases, truncated) = visible(&state, project.clone(), None, PAGE_SIZE).await?;
    let page = ListPage {
        layout: Layout::public(project.as_deref().map_or_else(
            || "Releases".to_owned(),
            |id| format!("{} releases", project_name(&state, id)),
        )),
        projects: state
            .config
            .projects
            .iter()
            .filter(|candidate| candidate.public)
            .map(|candidate| ProjectChoice {
                id: candidate.id.as_str().to_owned(),
                name: candidate.name.clone(),
                selected: project.as_deref() == Some(candidate.id.as_str()),
            })
            .collect(),
        feed: format!("/releases.atom{}", filter_query(project.as_deref())),
        releases: releases
            .iter()
            .map(|release| Row {
                href: href(release),
                project: project_name(&state, &release.project),
                version: release.version.clone(),
                title: release.display_title().to_owned(),
                environment: (release.environment != DEFAULT_ENVIRONMENT)
                    .then(|| release.environment.clone()),
                released_at: release.released_at.map(Stamp::new),
                deployed_at: release.deployed_at.map(Stamp::new),
            })
            .collect(),
        truncated,
    };
    Ok(render(StatusCode::OK, &page))
}

/// `/releases/{project}/{version}` path parameters.
#[derive(Deserialize)]
struct ReleasePath {
    project: String,
    version: String,
}

async fn detail(
    State(state): State<AppState>,
    path: Result<Path<ReleasePath>, PathRejection>,
) -> Result<Response, HtmlError> {
    let Path(ReleasePath { project, version }) = path.map_err(|_| HtmlError::not_found())?;
    // Every environment this version was published to; the production
    // one, or else the first, supplies the title and notes.
    let (mut releases, _) = visible(
        &state,
        Some(project.clone()),
        Some(version.clone()),
        PAGE_SIZE,
    )
    .await?;
    releases.sort_by(|a, b| {
        (a.environment != DEFAULT_ENVIRONMENT, &a.environment)
            .cmp(&(b.environment != DEFAULT_ENVIRONMENT, &b.environment))
    });
    let Some(release) = releases.first() else {
        return Err(HtmlError::not_found());
    };
    let name = project_name(&state, &project);
    let page = DetailPage {
        layout: Layout::public(format!("{name} {version}")),
        project_href: format!("/releases?project={project}"),
        project: name,
        version: version.clone(),
        title: release.display_title().to_owned(),
        released_at: release.released_at.map(Stamp::new),
        url: release.url.clone(),
        notes_html: markdown::render(&release.notes),
        deploys: releases
            .iter()
            .map(|release| Deploy {
                environment: release.environment.clone(),
                deployed_at: release.deployed_at.map(Stamp::new),
            })
            .collect(),
    };
    Ok(render(StatusCode::OK, &page))
}

async fn feed(
    State(state): State<AppState>,
    query: Result<Query<ProjectFilter>, QueryRejection>,
) -> Result<Response, HtmlError> {
    let Query(filter) = query
        .map_err(|_| HtmlError::new(StatusCode::BAD_REQUEST, "`project` must be one value."))?;
    let project = filter.project(&state)?;
    let (releases, _) = visible(&state, project.clone(), None, FEED_SIZE).await?;
    // Links are absolute when `[server] public_url` says where the site
    // is; feed readers resolve relative ones against the feed's URL.
    let base = state
        .config
        .server
        .public_url
        .as_deref()
        .map_or("", |url| url.trim_end_matches('/'));
    let query = filter_query(project.as_deref());
    let updated = releases
        .iter()
        .map(|release| release.updated_at)
        .max()
        .unwrap_or(Timestamp::from_millis(0));
    let page = Feed {
        id: format!(
            "urn:tocsin:releases{}",
            project
                .as_deref()
                .map_or_else(String::new, |p| format!(":{p}"))
        ),
        title: project.as_deref().map_or_else(
            || "Releases".to_owned(),
            |id| format!("{} releases", project_name(&state, id)),
        ),
        self_link: format!("{base}/releases.atom{query}"),
        alternate: format!("{base}/releases{query}"),
        updated: updated.to_string(),
        entries: releases
            .iter()
            .map(|release| Entry {
                id: format!("urn:tocsin:release:{}", release.id),
                link: format!("{base}{}", href(release)),
                title: format!(
                    "{} {}: {}",
                    project_name(&state, &release.project),
                    release.version,
                    release.display_title()
                ),
                updated: release.updated_at.to_string(),
                published: release
                    .released_at
                    .unwrap_or(release.created_at)
                    .to_string(),
                notes_html: markdown::render(&release.notes),
            })
            .collect(),
    };
    match page.render() {
        Ok(xml) => Ok((
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/atom+xml; charset=utf-8"),
            )],
            xml,
        )
            .into_response()),
        Err(error) => Err(HtmlError::internal(&error)),
    }
}
