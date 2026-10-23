// SPDX-License-Identifier: AGPL-3.0-only

//! The public status page: `/`, one incident at `/incidents/{id}`, the
//! incident archive at `/history`, one component at `/components/{id}`,
//! and the fragments `/` refreshes itself from at `/fragments/{name}`.
//!
//! - **Reads**: what the anonymous API reader sees. Component status comes
//!   from [`status::document`], a component from
//!   [`status::component_detail`], its events from [`timeline::read`],
//!   releases from public projects only. Nothing here works status out a
//!   second way; the uptime bars ([`uptime`]) are the one figure the API
//!   does not serve.
//! - **Live updates**: `/` loads htmx and its SSE extension. Each section
//!   re-fetches its fragment when `/api/v1/events` names an event that
//!   could change it, and every 60 seconds besides in case the stream
//!   drops. A fragment is the section's own markup: the page includes the
//!   same template, so a refreshed section is what a reload would show.
//!   Without JavaScript the page is complete as served.
//! - **Caching**: every response carries a strong `ETag` of its body and
//!   answers a matching `If-None-Match` with 304. Pages let a CDN keep
//!   them [`PAGE_CACHE`]; fragments, which the open page polls, are
//!   revalidated every time ([`FRAGMENT_CACHE`]). The body holds no
//!   timestamp of its own rendering, so an unchanged page keeps its tag.
//! - **Structured data**: each page carries JSON-LD, a `WebPage` and an
//!   `Event` per incident it shows.

use std::fmt::Write as _;
use std::str::FromStr;

use askama::Template;
use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use jiff::tz::TimeZone;
use serde::Deserialize;
use serde_json::{Value, json};
use tocsin_core::config::{Component, Config, Group};
use tocsin_core::incident::{Incident, IncidentKind, IncidentStatus, Timestamp};
use tocsin_core::state::ComponentStatus;
use tocsin_core::status::ComponentState;
use tocsin_store::{Day, Order};

use super::assets;
use super::feeds;
use super::markdown;
use super::page::{HtmlError, Layout};
use super::releases::{Stamp, href as release_href, project_name, versions};
use super::timeline::{Row, Rows, row};
use super::uptime::{self, Bar, Tallies};
use crate::http::auth::Viewer;
use crate::http::timeline::{self, Window, component_scope};
use crate::http::{AppState, now, status};

/// `Cache-Control` on the pages: a CDN may serve a copy for 15 seconds
/// and a stale one for 30 more while it refetches; a browser revalidates.
pub const PAGE_CACHE: &str = "public, max-age=0, s-maxage=15, stale-while-revalidate=30";
/// `Cache-Control` on the fragments: revalidated on every request, which
/// the `ETag` makes a 304 when nothing changed.
pub const FRAGMENT_CACHE: &str = "public, no-cache";
/// Where the page's event stream is.
const EVENTS: &str = "/api/v1/events";
/// Incidents on a `/history` page.
const HISTORY_PAGE: u32 = 25;
/// Resolved incidents on `/`.
const RECENT_RESOLVED: usize = 5;
/// Incidents read to find [`RECENT_RESOLVED`] resolved ones.
const RECENT_SCAN: u32 = 50;
/// Releases on `/`.
const RECENT_RELEASES: u32 = 5;
/// Events on a component's page.
const COMPONENT_EVENTS: u32 = 50;

/// An hour, which a component page's timeline window starts on.
const HOUR_MS: i64 = 3_600_000;

/// The status page routes, always on.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(index))
        .route("/history", get(history))
        .route("/incidents/{id}", get(incident))
        .route("/components/{id}", get(component))
        .route("/fragments/{name}", get(fragment))
}

/// What every status page shows around its content.
struct Chrome {
    /// `[page] title`.
    site: String,
    /// `[page] description`.
    description: Option<String>,
    /// Where the logo and title link: `[page] homepage_url`, or `/`.
    home: String,
    logo: Option<Logo>,
    links: Vec<Link>,
    /// The page's JSON-LD, escaped for a `<script>` element.
    json_ld: String,
    /// The scripts the page loads, in order; only `/` loads any.
    scripts: Vec<&'static str>,
}

struct Logo {
    url: String,
    alt: String,
}

struct Link {
    name: String,
    url: String,
}

impl Chrome {
    fn new(config: &Config, json_ld: &Value) -> Self {
        let page = &config.page;
        Chrome {
            site: page.title.clone(),
            description: page.description.clone(),
            home: page.homepage_url.clone().unwrap_or_else(|| "/".to_owned()),
            logo: page.logo.as_ref().map(|logo| Logo {
                url: logo.url.clone(),
                alt: logo.alt.clone(),
            }),
            links: page
                .links
                .iter()
                .map(|link| Link {
                    name: link.name.clone(),
                    url: link.url.clone(),
                })
                .collect(),
            json_ld: script_json(json_ld),
            scripts: Vec::new(),
        }
    }

    /// The same, loading htmx and its SSE extension.
    fn live(mut self) -> Self {
        self.scripts = vec![
            assets::url("htmx.min.js"),
            assets::url("htmx-ext-sse.min.js"),
        ];
        self
    }
}

/// `value` as JSON safe inside `<script>`: `<`, `>` and `&` escaped, so
/// no string in it can close the element.
fn script_json(value: &Value) -> String {
    value
        .to_string()
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
}

/// A status in words, with a glyph beside it so it never rests on colour.
struct StatusLabel {
    /// The CSS class: [`ComponentStatus::as_str`].
    class: &'static str,
    /// Hidden from screen readers; the text says the same.
    glyph: &'static str,
    text: &'static str,
}

impl StatusLabel {
    /// A component's status.
    const fn component(status: ComponentStatus) -> Self {
        let (glyph, text) = match status {
            ComponentStatus::Operational => ("✓", "Operational"),
            ComponentStatus::Degraded => ("▲", "Degraded"),
            ComponentStatus::Down => ("✕", "Down"),
            ComponentStatus::Maintenance => ("◆", "Maintenance"),
            ComponentStatus::Unknown => ("?", "Unknown"),
        };
        StatusLabel {
            class: status.as_str(),
            glyph,
            text,
        }
    }

    /// The whole page's status, for the banner.
    const fn overall(status: ComponentStatus) -> Self {
        let text = match status {
            ComponentStatus::Operational => "All systems operational",
            ComponentStatus::Degraded => "Some systems are degraded",
            ComponentStatus::Down => "Some systems are down",
            ComponentStatus::Maintenance => "Maintenance in progress",
            ComponentStatus::Unknown => "Status unknown",
        };
        StatusLabel {
            text,
            ..StatusLabel::component(status)
        }
    }
}

/// One component in its group.
struct ComponentRow {
    href: String,
    name: String,
    description: Option<String>,
    status: StatusLabel,
}

/// A group of components, or the components in none, which come first.
struct Section {
    name: Option<String>,
    description: Option<String>,
    components: Vec<ComponentRow>,
}

/// One component's uptime bar.
struct UptimeRow {
    href: String,
    name: String,
    bar: Bar,
}

/// An incident or maintenance as a list shows it.
struct Card {
    href: String,
    title: String,
    /// The CSS class: [`IncidentStatus::as_str`].
    status: &'static str,
    /// The status in words: `in progress`.
    status_text: String,
    /// The impact's CSS class and words, for an incident; `None` for
    /// maintenance.
    impact: Option<&'static str>,
    /// The components it names, by name.
    components: String,
    started: Stamp,
    /// When it was resolved, or completed.
    ended: Option<Stamp>,
    /// A maintenance window: `2026-10-23 14:00 UTC to 2026-10-23 16:00 UTC`.
    window: Option<String>,
    /// The newest update's message, rendered.
    latest_html: Option<String>,
}

/// One update on an incident's page.
struct UpdateView {
    status: &'static str,
    status_text: String,
    at: Stamp,
    message_html: String,
}

/// One release on `/`.
struct ReleaseRow {
    href: String,
    project: String,
    version: String,
    title: String,
    released_at: Option<Stamp>,
}

#[derive(Template)]
#[template(path = "status/index.html")]
struct IndexPage {
    layout: Layout,
    chrome: Chrome,
    events: &'static str,
    banner: StatusLabel,
    sections: Vec<Section>,
    uptime: Vec<UptimeRow>,
    active: Vec<Card>,
    resolved: Vec<Card>,
    releases: Option<Vec<ReleaseRow>>,
}

#[derive(Template)]
#[template(path = "status/incident.html")]
struct IncidentPage {
    layout: Layout,
    chrome: Chrome,
    card: Card,
    updates: Vec<UpdateView>,
}

#[derive(Template)]
#[template(path = "status/history.html")]
struct HistoryPage {
    layout: Layout,
    chrome: Chrome,
    cards: Vec<Card>,
    older: Option<String>,
}

#[derive(Template)]
#[template(path = "status/component.html")]
struct ComponentPage {
    layout: Layout,
    chrome: Chrome,
    name: String,
    description: Option<String>,
    group: Option<String>,
    status: StatusLabel,
    bar: Bar,
    active: Vec<Card>,
    timeline: Vec<Row>,
    since: Stamp,
    truncated: bool,
}

#[derive(Template)]
#[template(path = "status/fragments/banner.html")]
struct BannerFragment {
    banner: StatusLabel,
}

#[derive(Template)]
#[template(path = "status/fragments/components.html")]
struct ComponentsFragment {
    sections: Vec<Section>,
}

#[derive(Template)]
#[template(path = "status/fragments/uptime.html")]
struct UptimeFragment {
    uptime: Vec<UptimeRow>,
}

#[derive(Template)]
#[template(path = "status/fragments/incidents.html")]
struct IncidentsFragment {
    active: Vec<Card>,
}

#[derive(Template)]
#[template(path = "status/fragments/resolved.html")]
struct ResolvedFragment {
    resolved: Vec<Card>,
}

#[derive(Template)]
#[template(path = "status/fragments/releases.html")]
struct ReleasesFragment {
    releases: Vec<ReleaseRow>,
}

/// The sections of `/` that refresh themselves, by their path under
/// `/fragments/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fragment {
    Banner,
    Components,
    Uptime,
    Incidents,
    Resolved,
    Releases,
}

impl FromStr for Fragment {
    type Err = ();

    fn from_str(name: &str) -> Result<Self, ()> {
        Ok(match name {
            "banner" => Fragment::Banner,
            "components" => Fragment::Components,
            "uptime" => Fragment::Uptime,
            "incidents" => Fragment::Incidents,
            "resolved" => Fragment::Resolved,
            "releases" => Fragment::Releases,
            _ => return Err(()),
        })
    }
}

/// The time zone the pages show times in. `[page] timezone` was checked
/// when the configuration loaded; UTC if it has since gone from the
/// system's database.
pub(super) fn zone(config: &Config) -> TimeZone {
    TimeZone::get(&config.page.timezone).unwrap_or(TimeZone::UTC)
}

/// `[server] public_url` without its trailing `/`, to make links in
/// structured data absolute; empty when unset.
fn base(config: &Config) -> &str {
    config
        .server
        .public_url
        .as_deref()
        .map_or("", |url| url.trim_end_matches('/'))
}

/// `template` as a cacheable HTML response, or 304 if `request` already
/// holds this body.
fn respond(request: &HeaderMap, cache: &'static str, template: &impl Template) -> Response {
    match template.render() {
        Ok(html) => cached(request, cache, "text/html; charset=utf-8", html),
        Err(error) => HtmlError::internal(&error).into_response(),
    }
}

/// `body` as a cacheable response of `content_type`, with a strong `ETag`
/// of the body, or 304 if `request` already holds it.
pub(super) fn cached(
    request: &HeaderMap,
    cache: &'static str,
    content_type: &'static str,
    body: String,
) -> Response {
    let digest = ring::digest::digest(&ring::digest::SHA256, body.as_bytes());
    let tag = format!("\"{}\"", assets::hex(&digest.as_ref()[..16]));
    let fresh = request
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|candidate| candidate.trim().trim_start_matches("W/"))
        .any(|candidate| candidate == "*" || candidate == tag);
    let headers = [
        (header::CACHE_CONTROL, HeaderValue::from_static(cache)),
        (
            header::ETAG,
            HeaderValue::from_str(&tag).expect("hex in quotes is a valid header value"),
        ),
    ];
    if fresh {
        (StatusCode::NOT_MODIFIED, headers).into_response()
    } else {
        (
            headers,
            [(header::CONTENT_TYPE, HeaderValue::from_static(content_type))],
            body,
        )
            .into_response()
    }
}

/// Groups and their components in display order: components in no group
/// first, then groups in `[page] groups` order and the rest as defined;
/// within each, `[page] components` order and the rest as defined.
fn ordered(config: &Config) -> Vec<(Option<&Group>, Vec<&Component>)> {
    fn rank(order: &[tocsin_core::config::Id], id: &tocsin_core::config::Id) -> usize {
        order
            .iter()
            .position(|listed| listed == id)
            .unwrap_or(order.len())
    }
    let mut components: Vec<&Component> = config.components.iter().collect();
    // Stable: unlisted components keep their definition order.
    components.sort_by_key(|component| rank(&config.page.components, &component.id));
    let mut groups: Vec<&Group> = config.groups.iter().collect();
    groups.sort_by_key(|group| rank(&config.page.groups, &group.id));
    let members = |group: Option<&Group>| -> Vec<&Component> {
        components
            .iter()
            .copied()
            .filter(|component| component.group.as_ref() == group.map(|group| &group.id))
            .collect()
    };
    std::iter::once((None, members(None)))
        .chain(
            groups
                .into_iter()
                .map(|group| (Some(group), members(Some(group)))),
        )
        .filter(|(_, members)| !members.is_empty())
        .collect()
}

/// The component sections, with each component's state from `states`.
fn sections(config: &Config, states: &[ComponentState]) -> Vec<Section> {
    ordered(config)
        .into_iter()
        .map(|(group, components)| Section {
            name: group.map(|group| group.name.clone()),
            description: group.and_then(|group| group.description.clone()),
            components: components
                .into_iter()
                .map(|component| {
                    let current = states
                        .iter()
                        .find(|state| state.id == component.id.as_str())
                        .map_or(ComponentStatus::Unknown, |state| state.status);
                    ComponentRow {
                        href: format!("/components/{}", component.id.as_str()),
                        name: component.name.clone(),
                        description: component.description.clone(),
                        status: StatusLabel::component(current),
                    }
                })
                .collect(),
        })
        .collect()
}

/// Today, by UTC day.
fn today() -> Day {
    Day::containing(now().as_millis())
}

/// The daily rollups a bar ending `today` reads.
async fn tallies(state: &AppState, today: Day) -> Result<Tallies, HtmlError> {
    let rollups = state
        .store
        .daily_rollups(uptime::first_day(today))
        .await
        .map_err(|error| HtmlError::internal(&error))?;
    Ok(Tallies::new(&rollups))
}

/// Every component's uptime bar, in display order.
async fn uptime_rows(state: &AppState) -> Result<Vec<UptimeRow>, HtmlError> {
    let today = today();
    let tallies = tallies(state, today).await?;
    Ok(ordered(&state.config)
        .into_iter()
        .flat_map(|(_, components)| components)
        .map(|component| UptimeRow {
            href: format!("/components/{}", component.id.as_str()),
            name: component.name.clone(),
            bar: uptime::bar(
                &tallies,
                &component_scope(&state.config, component.id.as_str()).checks,
                today,
            ),
        })
        .collect())
}

/// `[[component]]` `id`'s name; the id if it is no longer configured.
pub(super) fn component_name(config: &Config, id: &str) -> String {
    config
        .components
        .iter()
        .find(|component| component.id.as_str() == id)
        .map_or_else(|| id.to_owned(), |component| component.name.clone())
}

/// An incident status in words.
pub(super) fn status_text(status: IncidentStatus) -> String {
    status.as_str().replace('_', " ")
}

/// `incident` as a list shows it.
fn card(config: &Config, zone: &TimeZone, incident: &Incident) -> Card {
    let latest = incident
        .updates
        .iter()
        .max_by_key(|update| (update.created_at, update.id));
    Card {
        href: format!("/incidents/{}", incident.id),
        title: incident.title.clone(),
        status: incident.status.as_str(),
        status_text: status_text(incident.status),
        impact: (incident.kind == IncidentKind::Incident).then(|| incident.impact.as_str()),
        components: incident
            .components
            .iter()
            .map(|id| component_name(config, id))
            .collect::<Vec<_>>()
            .join(", "),
        started: Stamp::zoned(incident.created_at, zone),
        ended: incident.resolved_at.map(|at| Stamp::zoned(at, zone)),
        window: match (incident.scheduled_start, incident.scheduled_end) {
            (Some(start), Some(end)) => Some(format!(
                "{} to {}",
                super::page::when_in(start, zone),
                super::page::when_in(end, zone)
            )),
            (Some(start), None) => Some(format!("from {}", super::page::when_in(start, zone))),
            _ => None,
        },
        latest_html: latest.map(|update| markdown::render(&update.message)),
    }
}

/// The incidents and maintenance resolved most recently, newest first.
async fn recently_resolved(state: &AppState, zone: &TimeZone) -> Result<Vec<Card>, HtmlError> {
    Ok(state
        .store
        .incidents(None, RECENT_SCAN)
        .await
        .map_err(|error| HtmlError::internal(&error))?
        .iter()
        .filter(|incident| incident.status.is_terminal())
        .take(RECENT_RESOLVED)
        .map(|incident| card(&state.config, zone, incident))
        .collect())
}

/// The newest public releases, or `None` when no project is public.
async fn recent_releases(
    state: &AppState,
    zone: &TimeZone,
) -> Result<Option<Vec<ReleaseRow>>, HtmlError> {
    if !state.config.projects.iter().any(|project| project.public) {
        return Ok(None);
    }
    let (releases, _) = versions(state, None, RECENT_RELEASES).await?;
    Ok(Some(
        releases
            .iter()
            .map(|release| ReleaseRow {
                href: release_href(release),
                project: project_name(&state.config, &release.project),
                version: release.version.clone(),
                title: release.display_title().to_owned(),
                released_at: release.released_at.map(|at| Stamp::zoned(at, zone)),
            })
            .collect(),
    ))
}

/// The `WebPage` node for `path`.
fn web_page(config: &Config, name: &str, path: &str) -> Value {
    let mut page = json!({
        "@type": "WebPage",
        "name": name,
        "url": format!("{}{path}", base(config)),
    });
    if let Some(description) = &config.page.description {
        page["description"] = json!(description);
    }
    page
}

/// `incident` as a schema.org `Event`.
fn event(config: &Config, incident: &Incident) -> Value {
    let url = format!("{}/incidents/{}", base(config), incident.id);
    let mut event = json!({
        "@type": "Event",
        "name": incident.title,
        "startDate": incident.scheduled_start.unwrap_or(incident.created_at).to_string(),
        "eventStatus": "https://schema.org/EventScheduled",
        "eventAttendanceMode": "https://schema.org/OnlineEventAttendanceMode",
        "location": { "@type": "VirtualLocation", "url": url },
        "url": url,
    });
    if let Some(end) = incident.resolved_at.or(incident.scheduled_end) {
        event["endDate"] = json!(end.to_string());
    }
    if let Some(update) = incident
        .updates
        .iter()
        .max_by_key(|update| (update.created_at, update.id))
    {
        event["description"] = json!(update.message);
    }
    event
}

/// A page's JSON-LD: its `WebPage` and the incidents it shows.
fn graph(page: Value, incidents: impl IntoIterator<Item = Value>) -> Value {
    json!({
        "@context": "https://schema.org",
        "@graph": std::iter::once(page).chain(incidents).collect::<Vec<_>>(),
    })
}

async fn index(State(state): State<AppState>, request: HeaderMap) -> Result<Response, HtmlError> {
    let config = &state.config;
    let zone = zone(config);
    let document = status::document(&state).await?;
    let json_ld = graph(
        web_page(config, &config.page.title, "/"),
        document
            .incidents
            .iter()
            .map(|incident| event(config, incident)),
    );
    let page = IndexPage {
        layout: Layout::public(config.page.title.clone())
            .with_feeds(feeds::site(config))
            .themed(&config.page),
        chrome: Chrome::new(config, &json_ld).live(),
        events: EVENTS,
        banner: StatusLabel::overall(document.status),
        sections: sections(config, &document.components),
        uptime: uptime_rows(&state).await?,
        active: document
            .incidents
            .iter()
            .map(|incident| card(config, &zone, incident))
            .collect(),
        resolved: recently_resolved(&state, &zone).await?,
        releases: recent_releases(&state, &zone).await?,
    };
    Ok(respond(&request, PAGE_CACHE, &page))
}

async fn fragment(
    State(state): State<AppState>,
    Path(name): Path<String>,
    request: HeaderMap,
) -> Result<Response, HtmlError> {
    let fragment = Fragment::from_str(&name).map_err(|()| HtmlError::not_found())?;
    let config = &state.config;
    let zone = zone(config);
    Ok(match fragment {
        Fragment::Banner => {
            let document = status::document(&state).await?;
            let banner = StatusLabel::overall(document.status);
            respond(&request, FRAGMENT_CACHE, &BannerFragment { banner })
        }
        Fragment::Components => {
            let document = status::document(&state).await?;
            let sections = sections(config, &document.components);
            respond(&request, FRAGMENT_CACHE, &ComponentsFragment { sections })
        }
        Fragment::Uptime => {
            let uptime = uptime_rows(&state).await?;
            respond(&request, FRAGMENT_CACHE, &UptimeFragment { uptime })
        }
        Fragment::Incidents => {
            let active = state
                .store
                .unresolved_incidents()
                .await
                .map_err(|error| HtmlError::internal(&error))?
                .iter()
                .map(|incident| card(config, &zone, incident))
                .collect();
            respond(&request, FRAGMENT_CACHE, &IncidentsFragment { active })
        }
        Fragment::Resolved => {
            let resolved = recently_resolved(&state, &zone).await?;
            respond(&request, FRAGMENT_CACHE, &ResolvedFragment { resolved })
        }
        Fragment::Releases => {
            let releases = recent_releases(&state, &zone)
                .await?
                .ok_or_else(HtmlError::not_found)?;
            respond(&request, FRAGMENT_CACHE, &ReleasesFragment { releases })
        }
    })
}

async fn incident(
    State(state): State<AppState>,
    Path(id): Path<String>,
    request: HeaderMap,
) -> Result<Response, HtmlError> {
    let id: i64 = id.parse().map_err(|_| HtmlError::not_found())?;
    let incident = state
        .store
        .incident(id)
        .await
        .map_err(|error| HtmlError::internal(&error))?
        .ok_or_else(HtmlError::not_found)?;
    let config = &state.config;
    let zone = zone(config);
    let json_ld = graph(
        web_page(config, &incident.title, &format!("/incidents/{id}")),
        [event(config, &incident)],
    );
    let mut updates: Vec<_> = incident.updates.iter().collect();
    updates.sort_by_key(|update| std::cmp::Reverse((update.created_at, update.id)));
    let page = IncidentPage {
        layout: Layout::public(format!("{} · {}", incident.title, config.page.title))
            .with_feeds(feeds::site(config))
            .themed(&config.page),
        chrome: Chrome::new(config, &json_ld),
        updates: updates
            .into_iter()
            .map(|update| UpdateView {
                status: update.status.as_str(),
                status_text: status_text(update.status),
                at: Stamp::zoned(update.created_at, &zone),
                message_html: markdown::render(&update.message),
            })
            .collect(),
        card: card(config, &zone, &incident),
    };
    Ok(respond(&request, PAGE_CACHE, &page))
}

/// `/history?before=` : the incident id the page starts below.
#[derive(Deserialize)]
struct HistoryQuery {
    before: Option<i64>,
}

async fn history(
    State(state): State<AppState>,
    query: Result<Query<HistoryQuery>, QueryRejection>,
    request: HeaderMap,
) -> Result<Response, HtmlError> {
    let Query(HistoryQuery { before }) = query
        .map_err(|_| HtmlError::new(StatusCode::BAD_REQUEST, "`before` must be an incident id."))?;
    let config = &state.config;
    let zone = zone(config);
    let mut incidents = state
        .store
        .incidents(before, HISTORY_PAGE + 1)
        .await
        .map_err(|error| HtmlError::internal(&error))?;
    let more = incidents.len() > HISTORY_PAGE as usize;
    incidents.truncate(HISTORY_PAGE as usize);
    let older = more
        .then(|| incidents.last())
        .flatten()
        .map(|last| format!("/history?before={}", last.id));
    let mut path = "/history".to_owned();
    if let Some(before) = before {
        let _ = write!(path, "?before={before}");
    }
    let json_ld = graph(web_page(config, "Incident history", &path), []);
    let page = HistoryPage {
        layout: Layout::public(format!("Incident history · {}", config.page.title))
            .with_feeds(feeds::site(config))
            .themed(&config.page),
        chrome: Chrome::new(config, &json_ld),
        cards: incidents
            .iter()
            .map(|incident| card(config, &zone, incident))
            .collect(),
        older,
    };
    Ok(respond(&request, PAGE_CACHE, &page))
}

async fn component(
    State(state): State<AppState>,
    Path(id): Path<String>,
    request: HeaderMap,
) -> Result<Response, HtmlError> {
    let detail = status::component_detail(&state, Viewer::Public, &id).await?;
    let config = &state.config;
    let zone = zone(config);
    // The window starts on the hour, so the page and its tag change when
    // its events do rather than on every request.
    let since = timeline::default_since().as_millis();
    let since = Timestamp::from_millis(since - since.rem_euclid(HOUR_MS));
    let events = timeline::read(
        &state,
        Viewer::Public,
        Window {
            component: Some(&id),
            since,
            after: None,
            order: Order::NewestFirst,
            limit: COMPONENT_EVENTS,
        },
    )
    .await?;
    let today = today();
    let bar = uptime::bar(
        &tallies(&state, today).await?,
        &component_scope(config, &id).checks,
        today,
    );
    let name = detail.component.name.clone();
    let json_ld = graph(
        web_page(config, &name, &format!("/components/{id}")),
        detail
            .incidents
            .iter()
            .map(|incident| event(config, incident)),
    );
    let rows = Rows {
        incidents: "/incidents",
        zone: &zone,
        config,
    };
    let page = ComponentPage {
        layout: Layout::public(format!("{name} · {}", config.page.title))
            .with_feeds(feeds::for_component(config, &id, &name))
            .themed(&config.page),
        chrome: Chrome::new(config, &json_ld),
        description: detail.component.description.clone(),
        group: detail.component.group.as_deref().map(|group| {
            config
                .groups
                .iter()
                .find(|candidate| candidate.id.as_str() == group)
                .map_or_else(|| group.to_owned(), |candidate| candidate.name.clone())
        }),
        status: StatusLabel::component(detail.component.status),
        bar,
        active: detail
            .incidents
            .iter()
            .map(|incident| card(config, &zone, incident))
            .collect(),
        timeline: events
            .events
            .iter()
            .map(|event| row(event, &rows))
            .collect(),
        since: Stamp::zoned(events.since, &zone),
        truncated: events.truncated,
        name,
    };
    Ok(respond(&request, PAGE_CACHE, &page))
}
