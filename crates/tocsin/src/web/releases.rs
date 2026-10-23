// SPDX-License-Identifier: AGPL-3.0-only

//! The public release log: `/releases` and one release at
//! `/releases/{project}/{version}`. Their Atom feeds are in [`super::feeds`].
//!
//! These pages show what an anonymous reader of the API sees: public
//! projects only, through [`crate::http::releases::visible_projects`]. The
//! list has one entry per version, dated at its first
//! publish; the version's page shows every environment it reached. They
//! load no script. Notes are Markdown, shown only through
//! [`markdown::render`]; titles and every other value are escaped by
//! askama.

use askama::Template;
use axum::Router;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use serde::Deserialize;
use tocsin_core::config::Config;
use tocsin_core::incident::Timestamp;
use tocsin_core::release::Release;
use tocsin_store::ReleaseQuery;

use super::page::{HtmlError, Layout, render, when, when_in};
use super::{feeds, markdown};
use crate::http::AppState;
use crate::http::auth::Viewer;
use crate::http::releases::visible_projects;

/// Releases on the list page.
const PAGE_SIZE: u32 = 50;

/// The public release routes, always on.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/releases", get(list))
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

    /// `at` shown in `zone`; the exact instant stays UTC.
    pub fn zoned(at: Timestamp, zone: &jiff::tz::TimeZone) -> Self {
        Stamp {
            text: when_in(at, zone),
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

/// One version in the list.
struct Row {
    href: String,
    project: String,
    version: String,
    title: String,
    released_at: Option<Stamp>,
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

/// `?project=` on the list and the feed; empty means every project.
#[derive(Deserialize)]
pub(super) struct ProjectFilter {
    project: Option<String>,
}

impl ProjectFilter {
    /// Project `id`.
    pub(super) fn new(id: &str) -> Self {
        ProjectFilter {
            project: Some(id.to_owned()),
        }
    }

    /// The project asked for, if any. One that is not configured and
    /// public is a 404, so a private project looks like no project.
    pub(super) fn project(self, state: &AppState) -> Result<Option<String>, HtmlError> {
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

/// The public view of `project`'s published versions, or every public
/// project's, one release standing for each, newest first; whether more
/// matched than `limit`.
pub(super) async fn versions(
    state: &AppState,
    project: Option<String>,
    limit: u32,
) -> Result<(Vec<Release>, bool), HtmlError> {
    state
        .store
        .release_versions(ReleaseQuery {
            visible: visible_projects(&state.config, Viewer::Public),
            project,
            version: None,
            since: None,
            before: None,
            limit,
        })
        .await
        .map_err(|error| HtmlError::internal(&error))
}

/// `[[project]]` `id`'s display name; the id if it has none configured.
pub(super) fn project_name(config: &Config, id: &str) -> String {
    config
        .projects
        .iter()
        .find(|project| project.id.as_str() == id)
        .map_or_else(|| id.to_owned(), |project| project.name.clone())
}

pub(super) fn href(release: &Release) -> String {
    format!("/releases/{}/{}", release.project, release.version)
}

async fn list(
    State(state): State<AppState>,
    query: Result<Query<ProjectFilter>, QueryRejection>,
) -> Result<Response, HtmlError> {
    let Query(filter) = query
        .map_err(|_| HtmlError::new(StatusCode::BAD_REQUEST, "`project` must be one value."))?;
    let project = filter.project(&state)?;
    let (releases, truncated) = versions(&state, project.clone(), PAGE_SIZE).await?;
    let page = ListPage {
        layout: Layout::public(project.as_deref().map_or_else(
            || "Releases".to_owned(),
            |id| format!("{} releases", project_name(&state.config, id)),
        ))
        .with_feeds(project.as_deref().map_or_else(
            || feeds::site(&state.config),
            |id| feeds::for_project(&state, id),
        ))
        .themed(&state.config.page),
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
        feed: project
            .as_deref()
            .map_or_else(|| feeds::RELEASES.to_owned(), feeds::project_href),
        releases: releases
            .iter()
            .map(|release| Row {
                href: href(release),
                project: project_name(&state.config, &release.project),
                version: release.version.clone(),
                title: release.display_title().to_owned(),
                released_at: release.released_at.map(Stamp::new),
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
    // Every environment this version reached, stamped-only ones included,
    // production first.
    let releases = state
        .store
        .release_environments(
            visible_projects(&state.config, Viewer::Public),
            project.clone(),
            version.clone(),
        )
        .await
        .map_err(|error| HtmlError::internal(&error))?;
    // The production release, or else the first published, supplies the
    // title and notes; a version never published has no page.
    let Some(release) = releases
        .iter()
        .find(|release| release.released_at.is_some())
    else {
        return Err(HtmlError::not_found());
    };
    let first_released = releases
        .iter()
        .filter_map(|release| release.released_at)
        .min();
    let name = project_name(&state.config, &project);
    let page = DetailPage {
        layout: Layout::public(format!("{name} {version}"))
            .with_feeds(feeds::for_project(&state, &project))
            .themed(&state.config.page),
        project_href: format!("/releases?project={project}"),
        project: name,
        version: version.clone(),
        title: release.display_title().to_owned(),
        released_at: first_released.map(Stamp::new),
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
