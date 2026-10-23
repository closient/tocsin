// SPDX-License-Identifier: AGPL-3.0-only

//! Atom feeds (RFC 4287) of what the status page and the release log show:
//!
//! | Path | Entries |
//! |---|---|
//! | `/feeds/incidents.atom` | incidents and maintenance |
//! | `/feeds/components/{id}.atom` | those naming one component |
//! | `/feeds/releases.atom` | public projects' versions |
//! | `/feeds/releases/{project}.atom` | one public project's versions |
//!
//! `/releases.atom[?project=]` serves the release feeds as well, from
//! before they moved under `/feeds/`.
//!
//! - **Visibility**: what the anonymous page shows. Every component and
//!   incident is public; releases come through
//!   [`crate::http::releases::visible_projects`], so an unknown component
//!   and a private or unknown project are the same 404.
//! - **Ids** never change once an entry is out: `urn:tocsin:incident:{id}`
//!   from the incident's row id, which is never reused, and
//!   `urn:tocsin:release:{project}:{version}` per version. An update or
//!   edit moves the entry's `<updated>`, never its id. They depend on no
//!   configuration, so neither a reload nor a new `public_url` changes them.
//! - **Links** are absolute: from `[server] public_url`, or else from the
//!   request's `Host`.
//! - **Escaping**: askama escapes every value as XML; content is HTML
//!   (sanitized Markdown for messages and notes) escaped once more as
//!   text; a version without notes shows its title, so no entry is
//!   empty. Characters XML 1.0 cannot carry at all, such as U+0001 in a
//!   message, are dropped.
//! - **Caching**: like the pages, a strong `ETag` and [`PAGE_CACHE`]; an
//!   empty feed's `<updated>` is the epoch, so the body stays the same
//!   until something in it changes.

use std::str::FromStr;

use askama::Template;
use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::uri::Authority;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use jiff::tz::TimeZone;
use tocsin_core::config::Config;
use tocsin_core::incident::{Incident, IncidentKind, Timestamp};
use tocsin_core::release::Release;

use super::markdown;
use super::page::{FeedLink, HtmlError, when_in};
use super::releases::{ProjectFilter, Stamp, href as release_href, project_name, versions};
use super::status::{PAGE_CACHE, cached, component_name, status_text, zone};
use crate::http::AppState;
use crate::http::incidents::{self, ListQuery};

/// Entries in a feed.
pub const FEED_SIZE: u32 = 20;
/// The media type every feed is served as.
const CONTENT_TYPE: &str = "application/atom+xml; charset=utf-8";
/// The suffix of every feed's path.
const SUFFIX: &str = ".atom";

/// Where the incident feed is.
pub const INCIDENTS: &str = "/feeds/incidents.atom";
/// Where the release feed is.
pub const RELEASES: &str = "/feeds/releases.atom";

/// The feed routes, always on.
pub fn router() -> Router<AppState> {
    Router::new()
        .route(INCIDENTS, get(incident_feed))
        .route("/feeds/components/{file}", get(component_feed))
        .route(RELEASES, get(release_feed))
        .route("/feeds/releases/{file}", get(project_feed))
        .route("/releases.atom", get(legacy_release_feed))
}

/// Component `id`'s feed.
pub fn component_href(id: &str) -> String {
    format!("/feeds/components/{id}{SUFFIX}")
}

/// Project `id`'s release feed.
pub fn project_href(id: &str) -> String {
    format!("/feeds/releases/{id}{SUFFIX}")
}

/// The feeds every public page advertises: incidents, and releases when a
/// project is public.
pub fn site(config: &Config) -> Vec<FeedLink> {
    let mut feeds = vec![FeedLink {
        title: format!("{} incidents", config.page.title),
        href: INCIDENTS.to_owned(),
    }];
    if config.projects.iter().any(|project| project.public) {
        feeds.push(FeedLink {
            title: format!("{} releases", config.page.title),
            href: RELEASES.to_owned(),
        });
    }
    feeds
}

/// [`site`], after component `id`'s feed.
pub fn for_component(config: &Config, id: &str, name: &str) -> Vec<FeedLink> {
    let mut feeds = vec![FeedLink {
        title: format!("{name} incidents"),
        href: component_href(id),
    }];
    feeds.extend(site(config));
    feeds
}

/// [`site`], after project `id`'s release feed.
pub fn for_project(state: &AppState, id: &str) -> Vec<FeedLink> {
    let mut feeds = vec![FeedLink {
        title: format!("{} releases", project_name(&state.config, id)),
        href: project_href(id),
    }];
    feeds.extend(site(&state.config));
    feeds
}

#[derive(Template)]
#[template(path = "feeds/atom.xml")]
struct Feed {
    id: String,
    title: String,
    subtitle: Option<String>,
    updated: String,
    self_link: String,
    alternate: String,
    /// `[page] title`: who publishes every entry.
    author: String,
    /// `[page] homepage_url`, or else the site.
    author_uri: Option<String>,
    version: &'static str,
    entries: Vec<Entry>,
}

struct Entry {
    id: String,
    title: String,
    link: String,
    published: String,
    updated: String,
    categories: Vec<String>,
    content_html: String,
}

/// One update in an incident's content.
struct UpdateView {
    status: String,
    at: Stamp,
    message_html: String,
}

#[derive(Template)]
#[template(path = "feeds/incident.html")]
struct IncidentContent {
    status: String,
    impact: Option<&'static str>,
    components: String,
    window: Option<String>,
    updates: Vec<UpdateView>,
}

#[derive(Template)]
#[template(path = "feeds/release.html")]
struct ReleaseContent<'a> {
    title: &'a str,
    notes_html: String,
}

/// What every feed of a request shares.
struct Site<'a> {
    config: &'a Config,
    /// The absolute URL links start with, without a trailing `/`.
    base: String,
    zone: TimeZone,
}

impl<'a> Site<'a> {
    fn new(config: &'a Config, request: &HeaderMap) -> Self {
        Site {
            config,
            base: base(config, request),
            zone: zone(config),
        }
    }

    /// The feed, with the site's author and the newest entry's time.
    fn feed(&self, head: Head, entries: Vec<(Timestamp, Entry)>) -> Feed {
        let updated = entries
            .iter()
            .map(|(at, _)| *at)
            .max()
            .unwrap_or(Timestamp::from_millis(0));
        Feed {
            id: head.id,
            title: head.title,
            subtitle: self.config.page.description.clone(),
            updated: updated.to_string(),
            self_link: format!("{}{}", self.base, head.path),
            alternate: format!("{}{}", self.base, head.alternate),
            author: self.config.page.title.clone(),
            author_uri: Some(
                self.config
                    .page
                    .homepage_url
                    .clone()
                    .unwrap_or_else(|| format!("{}/", self.base)),
            )
            .filter(|uri| uri.starts_with("http")),
            version: env!("CARGO_PKG_VERSION"),
            entries: entries.into_iter().map(|(_, entry)| entry).collect(),
        }
    }

    /// `incident` as an entry, dated at its last change.
    fn incident(&self, incident: &Incident) -> askama::Result<(Timestamp, Entry)> {
        let mut updates: Vec<_> = incident.updates.iter().collect();
        updates.sort_by_key(|update| std::cmp::Reverse((update.created_at, update.id)));
        let content = IncidentContent {
            status: status_text(incident.status),
            impact: (incident.kind == IncidentKind::Incident).then(|| incident.impact.as_str()),
            components: incident
                .components
                .iter()
                .map(|id| component_name(self.config, id))
                .collect::<Vec<_>>()
                .join(", "),
            window: match (incident.scheduled_start, incident.scheduled_end) {
                (Some(start), Some(end)) => Some(format!(
                    "{} to {}",
                    when_in(start, &self.zone),
                    when_in(end, &self.zone)
                )),
                _ => None,
            },
            updates: updates
                .into_iter()
                .map(|update| UpdateView {
                    status: status_text(update.status),
                    at: Stamp::zoned(update.created_at, &self.zone),
                    message_html: markdown::render(&update.message),
                })
                .collect(),
        };
        let entry = Entry {
            id: format!("urn:tocsin:incident:{}", incident.id),
            title: incident.title.clone(),
            link: format!("{}/incidents/{}", self.base, incident.id),
            published: incident.created_at.to_string(),
            updated: incident.updated_at.to_string(),
            categories: vec![
                incident.kind.as_str().to_owned(),
                incident.status.as_str().to_owned(),
            ],
            content_html: content.render()?,
        };
        Ok((incident.updated_at, entry))
    }

    /// `release`, standing for its version, as an entry.
    fn release(&self, state: &AppState, release: &Release) -> askama::Result<(Timestamp, Entry)> {
        let entry = Entry {
            // Per version, so a later environment's publish, which may
            // change which release stands for it, is the same entry.
            id: format!("urn:tocsin:release:{}:{}", release.project, release.version),
            title: format!(
                "{} {}: {}",
                project_name(&state.config, &release.project),
                release.version,
                release.display_title()
            ),
            link: format!("{}{}", self.base, release_href(release)),
            published: release
                .released_at
                .unwrap_or(release.created_at)
                .to_string(),
            updated: release.updated_at.to_string(),
            categories: vec![release.project.clone()],
            content_html: ReleaseContent {
                title: release.display_title(),
                notes_html: markdown::render(&release.notes),
            }
            .render()?,
        };
        Ok((release.updated_at, entry))
    }
}

/// A feed's own fields.
struct Head {
    id: String,
    title: String,
    /// The feed's canonical path.
    path: String,
    /// The page it mirrors.
    alternate: String,
}

/// `[server] public_url`, or else `http://` and the request's `Host`,
/// without a trailing `/`; empty, and links relative, when neither is
/// there.
fn base(config: &Config, request: &HeaderMap) -> String {
    if let Some(url) = &config.server.public_url {
        return url.trim_end_matches('/').to_owned();
    }
    request
        .get(header::HOST)
        .and_then(|host| host.to_str().ok())
        .and_then(|host| Authority::from_str(host).ok())
        .filter(|authority| !authority.as_str().contains('@'))
        .map_or_else(String::new, |authority| format!("http://{authority}"))
}

/// `text` without the characters XML 1.0 cannot hold even escaped: the C0
/// controls other than tab, newline and carriage return, and U+FFFE and
/// U+FFFF.
fn xml_safe(text: String) -> String {
    let allowed = |c: char| {
        matches!(c, '\t' | '\n' | '\r') || (c >= ' ' && !matches!(c, '\u{FFFE}' | '\u{FFFF}'))
    };
    if text.chars().all(allowed) {
        text
    } else {
        text.chars().filter(|&c| allowed(c)).collect()
    }
}

/// `feed` as a cacheable Atom response.
fn respond(request: &HeaderMap, feed: &Feed) -> Response {
    match feed.render() {
        Ok(xml) => cached(request, PAGE_CACHE, CONTENT_TYPE, xml_safe(xml)),
        Err(error) => HtmlError::internal(&error).into_response(),
    }
}

/// The id before `.atom` in a feed's last segment; 404 without it.
fn stem(file: &str) -> Result<&str, HtmlError> {
    file.strip_suffix(SUFFIX)
        .filter(|stem| !stem.is_empty())
        .ok_or_else(HtmlError::not_found)
}

async fn incident_feed(
    State(state): State<AppState>,
    request: HeaderMap,
) -> Result<Response, HtmlError> {
    let site = Site::new(&state.config, &request);
    let listed = incidents::list(
        &state,
        ListQuery {
            before: None,
            limit: Some(FEED_SIZE),
        },
    )
    .await?
    .incidents;
    let head = Head {
        id: "urn:tocsin:incidents".to_owned(),
        title: format!("{} incidents", state.config.page.title),
        path: INCIDENTS.to_owned(),
        alternate: "/history".to_owned(),
    };
    let entries = listed
        .iter()
        .map(|incident| site.incident(incident))
        .collect::<askama::Result<_>>()
        .map_err(|error| HtmlError::internal(&error))?;
    Ok(respond(&request, &site.feed(head, entries)))
}

async fn component_feed(
    State(state): State<AppState>,
    Path(file): Path<String>,
    request: HeaderMap,
) -> Result<Response, HtmlError> {
    let id = stem(&file)?;
    let listed = incidents::naming(&state, id, FEED_SIZE).await?;
    let site = Site::new(&state.config, &request);
    let head = Head {
        id: format!("urn:tocsin:incidents:{id}"),
        title: format!(
            "{} incidents · {}",
            component_name(&state.config, id),
            state.config.page.title
        ),
        path: component_href(id),
        alternate: format!("/components/{id}"),
    };
    let entries = listed
        .iter()
        .map(|incident| site.incident(incident))
        .collect::<askama::Result<_>>()
        .map_err(|error| HtmlError::internal(&error))?;
    Ok(respond(&request, &site.feed(head, entries)))
}

async fn release_feed(
    State(state): State<AppState>,
    request: HeaderMap,
) -> Result<Response, HtmlError> {
    releases(&state, &request, None).await
}

async fn project_feed(
    State(state): State<AppState>,
    Path(file): Path<String>,
    request: HeaderMap,
) -> Result<Response, HtmlError> {
    let project = ProjectFilter::new(stem(&file)?).project(&state)?;
    releases(&state, &request, project).await
}

/// `/releases.atom?project=`, where the release feeds were first.
async fn legacy_release_feed(
    State(state): State<AppState>,
    query: Result<Query<ProjectFilter>, QueryRejection>,
    request: HeaderMap,
) -> Result<Response, HtmlError> {
    let Query(filter) = query
        .map_err(|_| HtmlError::new(StatusCode::BAD_REQUEST, "`project` must be one value."))?;
    let project = filter.project(&state)?;
    releases(&state, &request, project).await
}

/// The release feed: `project`'s, or every public project's.
async fn releases(
    state: &AppState,
    request: &HeaderMap,
    project: Option<String>,
) -> Result<Response, HtmlError> {
    let (listed, _) = versions(state, project.clone(), FEED_SIZE).await?;
    let site = Site::new(&state.config, request);
    let head = match project.as_deref() {
        None => Head {
            id: "urn:tocsin:releases".to_owned(),
            title: format!("{} releases", state.config.page.title),
            path: RELEASES.to_owned(),
            alternate: "/releases".to_owned(),
        },
        Some(id) => Head {
            id: format!("urn:tocsin:releases:{id}"),
            title: format!(
                "{} releases · {}",
                project_name(&state.config, id),
                state.config.page.title
            ),
            path: project_href(id),
            alternate: format!("/releases?project={id}"),
        },
    };
    let entries = listed
        .iter()
        .map(|release| site.release(state, release))
        .collect::<askama::Result<_>>()
        .map_err(|error| HtmlError::internal(&error))?;
    Ok(respond(request, &site.feed(head, entries)))
}
