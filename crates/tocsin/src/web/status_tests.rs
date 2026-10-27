// SPDX-License-Identifier: AGPL-3.0-only

//! The public status page: what `/` serves without JavaScript, how it
//! wires its sections to the event stream, an incident reaching an open
//! page over a real socket, cache validators, the structural
//! accessibility audit and the stylesheet's contrast, uptime bars from
//! rollups, the archive's paging, JSON-LD, and display order.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use html5ever::tendril::StrTendril;
use html5ever::tokenizer::{
    BufferQueue, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts,
};
use regex::Regex;
use serde_json::{Value, json};
use tocsin_core::Health;
use tocsin_core::config::{DARK, Environment, LIGHT, Source};
use tocsin_core::incident::{
    Actor, Impact, IncidentKind, IncidentStatus, NewIncident, NewUpdate, Timestamp,
};
use tocsin_core::release::NewRelease;
use tocsin_core::state::ComponentStatus;
use tocsin_engine::{ComponentSnapshot, HeartbeatHandle, Snapshot};
use tocsin_store::{Day, NewCheckResult, NewOutage, Store, SubjectKind};
use tower::ServiceExt;

use super::status::{FRAGMENT_CACHE, PAGE_CACHE};
use crate::http::auth::IncidentWriter;
use crate::http::events::tests::every_event;
use crate::http::events::{Event, EventBus, RESYNC};
use crate::http::{self, AppState, incidents, now, releases};

pub(super) const CONFIG: &str = r#"
[server]
public_url = "https://status.example.com/"

[page]
title = "Example status"
description = "Live status of Example's services."
homepage_url = "https://example.com/"
groups = ["edge", "core"]
components = ["jobs", "api"]
timezone = "Asia/Kolkata"

[page.logo]
url = "/brand/logo.svg"
alt = "Example"

[[page.link]]
name = "Support"
url = "https://example.com/support"

[[group]]
id = "core"
name = "Core"
description = "What everything else runs on"

[[group]]
id = "edge"
name = "Edge"

[[component]]
id = "api"
name = "API"
description = "The public API"
group = "core"

[[component]]
id = "web"
name = "Web"
group = "edge"

[[component]]
id = "docs"
name = "Docs"

[[component]]
id = "jobs"
name = "Jobs"
group = "core"

[[check]]
id = "api-https"
component = "api"
type = "http"
target = "https://api.internal.example.com/health"
interval = "30s"

[[token]]
id = "oncall"
secret = "${ENV:ONCALL_TOKEN}"
scopes = ["incident"]
"#;

/// A public project, so `/` shows releases.
pub(super) const PUBLIC_PROJECT: &str = r#"
[[project]]
id = "app"
name = "Example app"
public = true
components = ["web"]
"#;

const ONCALL: &str = "oncall-secret-0123456789";

/// The engine's view: the API down, the web degraded, jobs fine, and no
/// state yet for docs.
fn snapshot() -> Snapshot {
    let component = |id: &str, health: Option<Health>, status| ComponentSnapshot {
        id: id.into(),
        health,
        status,
    };
    Snapshot {
        checks: Vec::new(),
        components: vec![
            component("api", Some(Health::Down), ComponentStatus::Down),
            component("web", Some(Health::Degraded), ComponentStatus::Degraded),
            component("jobs", Some(Health::Up), ComponentStatus::Operational),
        ],
    }
}

/// A store, the state around it, and the router serving it.
pub(super) struct Site {
    _dir: tempfile::TempDir,
    pub(super) state: AppState,
    router: Router,
}

/// A site configured with [`CONFIG`] and `more`.
pub(super) fn site(more: &str) -> Site {
    site_from(&format!("{CONFIG}{more}"))
}

/// A site configured by `text` in full.
fn site_from(text: &str) -> Site {
    let variable = |name: &str| (name == "ONCALL_TOKEN").then(|| ONCALL.to_owned());
    let config = tocsin_core::config::load(
        &[Source {
            name: "tocsin.toml".into(),
            text: Ok(text.to_owned()),
        }],
        &Environment {
            hostname: "test".into(),
            node_id: None,
            variable: &variable,
        },
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join(tocsin_store::DB_FILE)).unwrap();
    let state = AppState {
        store: Arc::new(store),
        config: Arc::new(config),
        events: EventBus::new(),
        admin: None,
        live: Arc::new(snapshot()),
        heartbeats: HeartbeatHandle::default(),
        alert_wake: Arc::default(),
        config_status: crate::reload::steady_status(),
        metrics: Arc::new(crate::metrics::Metrics::new()),
        subscribers: None,
        slack: None,
    };
    Site {
        _dir: dir,
        router: http::router(state.clone()),
        state,
    }
}

impl Site {
    /// Status, headers and body of `GET uri` with `headers`.
    pub(super) async fn get_with(
        &self,
        uri: &str,
        headers: &[(header::HeaderName, &str)],
    ) -> (StatusCode, HeaderMap, String) {
        let mut request = Request::builder().uri(uri);
        for (name, value) in headers {
            request = request.header(name, *value);
        }
        let response = self
            .router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
    }

    /// The page at `uri`, which must be there.
    pub(super) async fn page(&self, uri: &str) -> String {
        let (status, _, body) = self.get_with(uri, &[]).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        body
    }

    pub(super) async fn open(&self, title: &str, kind: IncidentKind, components: &[&str]) -> i64 {
        let scheduled = (kind == IncidentKind::Maintenance).then(|| {
            let start = now().as_millis() + 86_400_000;
            (
                Timestamp::from_millis(start),
                Timestamp::from_millis(start + 7_200_000),
            )
        });
        incidents::open(
            &self.state,
            NewIncident {
                kind,
                title: title.to_owned(),
                impact: Impact::Major,
                components: components.iter().map(|&id| id.to_owned()).collect(),
                status: None,
                message: format!("Looking into **{title}**."),
                scheduled_start: scheduled.map(|(start, _)| start),
                scheduled_end: scheduled.map(|(_, end)| end),
                notify: true,
                draft: None,
                internal_note: None,
                external_key: None,
            },
            &IncidentWriter::publishing(Actor::person("alice@example.com")),
        )
        .await
        .unwrap()
        .0
        .id
    }

    pub(super) async fn resolve(&self, id: i64) {
        incidents::append(
            &self.state,
            id,
            NewUpdate {
                status: IncidentStatus::Resolved,
                message: "Fixed.".to_owned(),
                notify: true,
            },
            &IncidentWriter::publishing(Actor::person("alice@example.com")),
        )
        .await
        .unwrap();
    }

    pub(super) async fn release(&self, version: &str, title: &str) {
        releases::publish(
            &self.state,
            NewRelease {
                project: "app".to_owned(),
                version: version.to_owned(),
                environment: None,
                title: Some(title.to_owned()),
                notes: String::new(),
                url: None,
                released_at: Some(Timestamp::from_millis(1_760_000_000_000)),
                deployed_at: None,
            },
            Actor::token("deploys"),
        )
        .await
        .unwrap();
    }

    /// One result for `api-https`, `ago` milliseconds back.
    async fn result(&self, ago: i64, health: Health) {
        self.state
            .store
            .append(NewCheckResult {
                node_id: "node-a".into(),
                check_id: "api-https".into(),
                started_at_ms: now().as_millis() - ago,
                duration_ms: 40,
                health,
                detail: None,
                error: None,
                status_code: None,
            })
            .await
            .unwrap();
    }

    /// Everything `/` shows: an incident, a maintenance window, a resolved
    /// incident and a release.
    pub(super) async fn furnish(&self) {
        self.open("Elevated API errors", IncidentKind::Incident, &["api"])
            .await;
        self.open("Database upgrade", IncidentKind::Maintenance, &["jobs"])
            .await;
        let resolved = self
            .open("Slow page loads", IncidentKind::Incident, &["web"])
            .await;
        self.resolve(resolved).await;
        self.release("1.2.0", "Faster search").await;
    }
}

// --- The page as served ----------------------------------------------------

/// `page` with what changes from run to run (dates, times, asset hashes)
/// replaced by placeholders.
fn normalize(page: &str) -> String {
    let rules = [
        (r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?Z", "<instant>"),
        (r"\d{4}-\d{2}-\d{2} \d{2}:\d{2}", "<when>"),
        (r"\d{4}-\d{2}-\d{2}", "<day>"),
        (r"/static/[0-9a-f]+/", "/static/<hash>/"),
    ];
    rules
        .iter()
        .fold(page.to_owned(), |page, (pattern, placeholder)| {
            Regex::new(pattern)
                .unwrap()
                .replace_all(&page, *placeholder)
                .into_owned()
        })
}

#[tokio::test]
async fn the_index_page_renders_every_section() {
    let site = site(PUBLIC_PROJECT);
    site.furnish().await;
    let page = site.page("/").await;
    insta::assert_snapshot!("index", normalize(&page));
}

#[tokio::test]
async fn the_index_page_reads_complete_without_javascript() {
    let site = site(PUBLIC_PROJECT);
    site.furnish().await;
    let elements = elements(&site.page("/").await);
    // What a browser without JavaScript shows: every element's text with
    // the scripts left out.
    let shown: String = elements
        .iter()
        .filter(|element| element.name == "main")
        .map(|element| element.spoken.clone())
        .collect();
    let shown = shown.split_whitespace().collect::<Vec<_>>().join(" ");
    for expected in [
        "Some systems are down",
        "Docs Unknown",
        "Web Degraded",
        "Jobs Operational",
        "API Down",
        "Elevated API errors",
        "Database upgrade",
        "Slow page loads",
        "Example app 1.2.0",
        "Faster search",
        "No data in the last 90 days",
    ] {
        assert!(shown.contains(expected), "{expected:?} is not in {shown}");
    }
}

#[tokio::test]
async fn the_index_page_refreshes_each_section_on_the_events_that_change_it() {
    let site = site(PUBLIC_PROJECT);
    let page = site.page("/").await;
    let elements = elements(&page);
    let stream = elements
        .iter()
        .position(|element| element.attribute("sse-connect") == Some("/api/v1/events"))
        .expect("an element connects to the event stream");
    assert_eq!(elements[stream].attribute("hx-ext"), Some("sse"));
    let wired: HashMap<&str, &str> = elements
        .iter()
        .filter(|element| element.ancestors.contains(&stream))
        .filter_map(|element| {
            Some((
                element.attribute("hx-get")?,
                element.attribute("hx-trigger")?,
            ))
        })
        .collect();
    let expected = [
        (
            "/fragments/banner",
            "sse:component_changed, sse:incident_created, sse:incident_published, sse:incident_updated, sse:incident_edited, sse:resync, every 60s",
        ),
        (
            "/fragments/incidents",
            "sse:incident_created, sse:incident_published, sse:incident_updated, sse:incident_edited, sse:resync, every 60s",
        ),
        (
            "/fragments/components",
            "sse:component_changed, sse:incident_created, sse:incident_published, sse:incident_updated, sse:incident_edited, sse:resync, every 60s",
        ),
        ("/fragments/uptime", "sse:resync, every 600s"),
        (
            "/fragments/resolved",
            "sse:incident_published, sse:incident_updated, sse:incident_edited, sse:resync, every 60s",
        ),
        (
            "/fragments/releases",
            "sse:release_published, sse:release_deployed, sse:resync, every 60s",
        ),
    ];
    assert_eq!(wired, expected.into_iter().collect::<HashMap<_, _>>());
    // Each named event is one the stream sends.
    let sent: Vec<&str> = every_event(true)
        .iter()
        .map(Event::name)
        .chain([RESYNC])
        .collect();
    for trigger in wired.values() {
        for name in trigger
            .split(", ")
            .filter_map(|part| part.strip_prefix("sse:"))
        {
            assert!(sent.contains(&name), "the stream never sends {name}");
        }
    }
    // The scripts that do it load, from this site.
    let scripts: Vec<&str> = elements
        .iter()
        .filter(|element| element.name == "script")
        .filter_map(|element| element.attribute("src"))
        .collect();
    assert_eq!(scripts.len(), 2, "{scripts:?}");
    for script in scripts {
        assert!(script.starts_with("/static/"), "{script}");
        let (status, headers, _) = site.get_with(script, &[]).await;
        assert_eq!(status, StatusCode::OK, "{script}");
        assert_eq!(
            headers[header::CONTENT_TYPE],
            "text/javascript; charset=utf-8"
        );
    }
    // Every fragment a section refreshes from answers.
    for fragment in wired.keys() {
        site.page(fragment).await;
    }
}

/// Reads `response` until its body so far contains `marker`.
async fn read_until(response: &mut reqwest::Response, seen: &mut String, marker: &str) {
    while !seen.contains(marker) {
        let chunk = response
            .chunk()
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("the stream ended before {marker:?}: {seen}"));
        seen.push_str(std::str::from_utf8(&chunk).unwrap());
    }
}

#[tokio::test]
async fn an_incident_posted_through_the_api_reaches_an_open_page_within_two_seconds() {
    let site = site("");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = site
        .router
        .clone()
        .into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::new();

    // The open page: its stream connected, and the incidents section as
    // it first showed.
    let mut stream = client
        .get(format!("{base}/api/v1/events"))
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), StatusCode::OK);
    let mut seen = String::new();
    tokio::time::timeout(
        Duration::from_secs(2),
        read_until(&mut stream, &mut seen, "event: resync"),
    )
    .await
    .expect("the stream says resync on connect");
    let before = client
        .get(format!("{base}/fragments/incidents"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        before.contains("No incidents or maintenance in progress."),
        "{before}"
    );

    let posted = Instant::now();
    let response = client
        .post(format!("{base}/api/v1/incidents"))
        .bearer_auth(ONCALL)
        .json(&json!({
            "title": "Checkout is failing",
            "impact": "major",
            "components": ["api"],
            "message": "Payments time out.",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    // What htmx does: the event names the section's trigger, and the
    // section re-fetches its fragment.
    let after = tokio::time::timeout(Duration::from_secs(2), async {
        read_until(&mut stream, &mut seen, "event: incident_created").await;
        client
            .get(format!("{base}/fragments/incidents"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    })
    .await
    .expect("the incident reaches the page within two seconds");
    let elapsed = posted.elapsed();
    assert!(after.contains("Checkout is failing"), "{after}");
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
}

// --- Caching ---------------------------------------------------------------

#[tokio::test]
async fn pages_are_cacheable_and_revalidate_by_etag() {
    let site = site("");
    site.open("Elevated API errors", IncidentKind::Incident, &["api"])
        .await;
    for (uri, cache) in [
        ("/", PAGE_CACHE),
        ("/history", PAGE_CACHE),
        ("/incidents/1", PAGE_CACHE),
        ("/components/api", PAGE_CACHE),
        ("/fragments/incidents", FRAGMENT_CACHE),
        ("/fragments/banner", FRAGMENT_CACHE),
    ] {
        let (status, headers, _) = site.get_with(uri, &[]).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(headers[header::CACHE_CONTROL], cache, "{uri}");
        // The security headers still apply.
        assert!(
            headers.contains_key(header::CONTENT_SECURITY_POLICY),
            "{uri}"
        );
        let tag = headers[header::ETAG].to_str().unwrap().to_owned();
        assert!(tag.starts_with('"') && tag.ends_with('"'), "{tag}");
        let weak = format!("W/{tag}");
        let listed = format!("\"other\", {tag}");
        for matching in [tag.as_str(), weak.as_str(), listed.as_str(), "*"] {
            let (status, headers, body) = site
                .get_with(uri, &[(header::IF_NONE_MATCH, matching)])
                .await;
            assert_eq!(status, StatusCode::NOT_MODIFIED, "{uri} {matching}");
            assert_eq!(headers[header::ETAG], tag.as_str(), "{uri}");
            assert_eq!(headers[header::CACHE_CONTROL], cache, "{uri}");
            assert_eq!(body, "");
        }
        let (status, _, _) = site
            .get_with(uri, &[(header::IF_NONE_MATCH, "\"other\"")])
            .await;
        assert_eq!(status, StatusCode::OK, "{uri}");
    }
    // A new incident changes the page, and so its tag.
    let (_, headers, _) = site.get_with("/", &[]).await;
    let tag = headers[header::ETAG].to_str().unwrap().to_owned();
    site.open("Checkout is failing", IncidentKind::Incident, &["api"])
        .await;
    let (status, headers, body) = site.get_with("/", &[(header::IF_NONE_MATCH, &tag)]).await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(headers[header::ETAG], tag.as_str());
    assert!(body.contains("Checkout is failing"));
}

// --- Accessibility -----------------------------------------------------------

/// One element of a page, as the tokenizer saw it.
#[derive(Debug)]
struct Element {
    name: String,
    attributes: Vec<(String, String)>,
    /// The elements it is inside, outermost first.
    ancestors: Vec<usize>,
    /// Its text as a screen reader has it: everything inside, less what is
    /// `aria-hidden`.
    spoken: String,
}

impl Element {
    fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn has_class(&self, class: &str) -> bool {
        self.attribute("class")
            .is_some_and(|classes| classes.split_whitespace().any(|name| name == class))
    }

    fn words(&self) -> String {
        self.spoken.split_whitespace().collect::<Vec<_>>().join(" ")
    }
}

/// Builds [`Element`]s from tokens.
#[derive(Default)]
struct Elements {
    elements: RefCell<Vec<Element>>,
    /// The open elements, innermost last.
    open: RefCell<Vec<usize>>,
}

/// Elements that never have content or an end tag.
const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "track",
    "wbr",
];

impl TokenSink for Elements {
    type Handle = ();

    fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
        let mut elements = self.elements.borrow_mut();
        let mut open = self.open.borrow_mut();
        match token {
            Token::TagToken(tag) if tag.kind == TagKind::StartTag => {
                let name = tag.name.to_string();
                elements.push(Element {
                    attributes: tag
                        .attrs
                        .iter()
                        .map(|attribute| {
                            (
                                attribute.name.local.to_string(),
                                attribute.value.to_string(),
                            )
                        })
                        .collect(),
                    ancestors: open.clone(),
                    spoken: String::new(),
                    name: name.clone(),
                });
                if !tag.self_closing && !VOID.contains(&name.as_str()) {
                    open.push(elements.len() - 1);
                }
            }
            Token::TagToken(tag) => {
                let name = tag.name.to_string();
                if let Some(at) = open.iter().rposition(|&index| elements[index].name == name) {
                    open.truncate(at);
                }
            }
            Token::CharacterTokens(text) => {
                let hidden = open
                    .iter()
                    .any(|&index| elements[index].attribute("aria-hidden") == Some("true"));
                let script = open.iter().any(|&index| elements[index].name == "script");
                if !hidden && !script {
                    for &index in open.iter() {
                        elements[index].spoken.push_str(&text);
                    }
                }
            }
            _ => {}
        }
        TokenSinkResult::Continue
    }
}

/// `html`'s elements in document order.
fn elements(html: &str) -> Vec<Element> {
    let input = BufferQueue::default();
    input.push_back(StrTendril::from_slice(html));
    let tokenizer = Tokenizer::new(Elements::default(), TokenizerOpts::default());
    let _ = tokenizer.feed(&input);
    tokenizer.end();
    tokenizer.sink.elements.take()
}

/// What a structural accessibility audit finds wrong with `html`.
pub(crate) fn audit(html: &str) -> Vec<String> {
    let elements = elements(html);
    let mut problems = Vec::new();
    audit_landmarks(&elements, &mut problems);
    audit_names(&elements, &mut problems);
    problems
}

/// Landmarks, the document language and title, image and control text.
fn audit_landmarks(elements: &[Element], problems: &mut Vec<String>) {
    let named = |name: &'static str| elements.iter().filter(move |element| element.name == name);
    let count = |name: &'static str| named(name).count();

    if !named("html").all(|html| html.attribute("lang").is_some_and(|lang| !lang.is_empty())) {
        problems.push("<html> has no lang".to_owned());
    }
    if !named("title").all(|title| !title.words().is_empty()) || count("title") != 1 {
        problems.push("no title".to_owned());
    }
    for (landmark, wanted) in [("main", 1), ("h1", 1)] {
        if count(landmark) != wanted {
            problems.push(format!("{} <{landmark}>, not {wanted}", count(landmark)));
        }
    }
    for landmark in ["header", "footer", "nav"] {
        if count(landmark) == 0 {
            problems.push(format!("no <{landmark}>"));
        }
    }
    // Navigation landmarks are told apart by their labels.
    let mut labels = HashSet::new();
    for nav in named("nav") {
        match nav.attribute("aria-label") {
            Some(label) if !label.trim().is_empty() => {
                if !labels.insert(label) {
                    problems.push(format!("two <nav> labelled {label:?}"));
                }
            }
            _ => problems.push("a <nav> has no aria-label".to_owned()),
        }
    }
    // A skip link to the main content comes before the header.
    let main_id = named("main").find_map(|main| main.attribute("id"));
    let skip = elements.iter().position(|element| {
        element.name == "a"
            && main_id.is_some_and(|id| element.attribute("href") == Some(&format!("#{id}")))
    });
    let header = elements.iter().position(|element| element.name == "header");
    if !matches!((skip, header), (Some(skip), Some(header)) if skip < header) {
        problems.push("no skip link to <main> before the header".to_owned());
    }
    for image in named("img") {
        if image
            .attribute("alt")
            .is_none_or(|alt| alt.trim().is_empty())
        {
            problems.push(format!("<img src={:?}> has no alt", image.attribute("src")));
        }
    }
    for control in ["input", "select", "textarea"] {
        for element in named(control) {
            let id = element.attribute("id");
            let labelled = element.attribute("aria-label").is_some()
                || named("label").any(|label| id.is_some() && label.attribute("for") == id);
            if !labelled {
                problems.push(format!("an unlabelled <{control}>"));
            }
        }
    }
}

/// Headings, ids and the references to them, and the words of links and
/// statuses.
fn audit_names(elements: &[Element], problems: &mut Vec<String>) {
    let named = |name: &'static str| elements.iter().filter(move |element| element.name == name);
    // Headings never skip a level, and the first is the h1.
    let mut level = 0;
    for element in elements {
        let Some(next) = element
            .name
            .strip_prefix('h')
            .and_then(|digit| digit.parse::<usize>().ok())
            .filter(|level| (1..=6).contains(level))
        else {
            continue;
        };
        if next > level + 1 {
            problems.push(format!(
                "<{}> {:?} follows an h{level}",
                element.name,
                element.words()
            ));
        }
        if element.words().is_empty() {
            problems.push(format!("an empty <{}>", element.name));
        }
        level = next;
    }
    let mut ids = HashSet::new();
    for id in elements
        .iter()
        .filter_map(|element| element.attribute("id"))
    {
        if !ids.insert(id) {
            problems.push(format!("id {id:?} is used twice"));
        }
    }
    for element in elements {
        if let Some(target) = element.attribute("aria-labelledby")
            && !ids.contains(target)
        {
            problems.push(format!("aria-labelledby {target:?} names no element"));
        }
    }
    for section in named("section") {
        if section.attribute("aria-labelledby").is_none()
            && section.attribute("aria-label").is_none()
        {
            problems.push(format!(
                "<section id={:?}> has no name",
                section.attribute("id")
            ));
        }
    }
    for (index, link) in elements
        .iter()
        .enumerate()
        .filter(|(_, element)| element.name == "a")
    {
        if link.attribute("href").is_none_or(str::is_empty) {
            problems.push(format!("<a> {:?} has no href", link.words()));
        }
        let has_image_text = elements.iter().any(|element| {
            element.name == "img"
                && element.ancestors.contains(&index)
                && element.attribute("alt").is_some_and(|alt| !alt.is_empty())
        });
        if link.words().is_empty() && !has_image_text && link.attribute("aria-label").is_none() {
            problems.push(format!("<a href={:?}> has no text", link.attribute("href")));
        }
    }
    // A status is in words, never its colour or glyph alone.
    for element in elements {
        let words = element.words();
        if (element.has_class("status") || element.has_class("hero") || element.has_class("badge"))
            && !words.chars().any(char::is_alphabetic)
        {
            problems.push(format!(
                "a {:?} status has no words",
                element.attribute("class")
            ));
        }
        if element.has_class("day") && !(words.contains("% up") || words.contains("no data")) {
            problems.push(format!("an uptime day says {words:?}"));
        }
    }
}

#[tokio::test]
async fn every_public_page_passes_the_structural_audit() {
    let site = site(PUBLIC_PROJECT);
    site.furnish().await;
    site.result(1, Health::Down).await;
    site.state
        .store
        .rollup_since(Day::containing(now().as_millis()))
        .await
        .unwrap();
    for uri in [
        "/",
        "/incidents/1",
        "/incidents/2",
        "/history",
        "/components/api",
        "/components/docs",
    ] {
        let page = site.page(uri).await;
        assert_eq!(audit(&page), Vec::<String>::new(), "{uri}");
    }
}

#[test]
fn the_audit_finds_what_it_looks_for() {
    let good = r##"<!doctype html><html lang="en"><head><title>T</title></head><body>
        <a href="#main">Skip</a><header><a href="/"><img src="/l.svg" alt="Logo"></a><nav aria-label="Links"><a href="/x">X</a></nav></header>
        <main id="main"><h1>T</h1><section aria-labelledby="s"><h2 id="s">S</h2>
        <p class="status down"><span aria-hidden="true">✕</span> Down</p>
        <ol><li class="day up"><span class="tip">2026-01-01: 100% up</span></li></ol></section></main>
        <footer><nav aria-label="More"><a href="/y">Y</a></nav></footer></body></html>"##;
    assert_eq!(audit(good), Vec::<String>::new());
    for (broken, finding) in [
        (good.replace(r#" lang="en""#, ""), "<html> has no lang"),
        (
            good.replace(r#" alt="Logo""#, ""),
            "<img src=Some(\"/l.svg\")> has no alt",
        ),
        (
            good.replace(r#"<h2 id="s">S</h2>"#, r#"<h3 id="s">S</h3>"#),
            "<h3> \"S\" follows an h1",
        ),
        (
            good.replace(r##"<a href="#main">Skip</a>"##, ""),
            "no skip link to <main> before the header",
        ),
        (
            good.replace(r#" aria-label="Links""#, ""),
            "a <nav> has no aria-label",
        ),
        (
            good.replace(r#" aria-label="More""#, r#" aria-label="Links""#),
            "two <nav> labelled \"Links\"",
        ),
        (
            good.replace(r#"aria-labelledby="s""#, r#"aria-labelledby="t""#),
            "aria-labelledby \"t\" names no element",
        ),
        (
            good.replace("</span> Down", "</span>"),
            "a Some(\"status down\") status has no words",
        ),
        (
            good.replace("2026-01-01: 100% up", ""),
            "an uptime day says \"\"",
        ),
        (
            good.replace(r#"<a href="/x">X</a>"#, r#"<a href="/x"></a>"#),
            "<a href=Some(\"/x\")> has no text",
        ),
    ] {
        assert!(
            audit(&broken).contains(&finding.to_owned()),
            "{finding}: {:?}",
            audit(&broken)
        );
    }
}

/// `#rrggbb`'s relative luminance (WCAG 2.2).
fn luminance(hex: &str) -> f64 {
    let channel = |at: usize| {
        let value = f64::from(u8::from_str_radix(&hex[at..at + 2], 16).unwrap()) / 255.0;
        if value <= 0.040_45 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(1) + 0.7152 * channel(3) + 0.0722 * channel(5)
}

fn contrast(one: &str, other: &str) -> f64 {
    let (one, other) = (luminance(one), luminance(other));
    (one.max(other) + 0.05) / (one.min(other) + 0.05)
}

/// The colour tokens in `block` of the stylesheet.
fn tokens(block: &str) -> HashMap<String, String> {
    Regex::new(r"--([a-z-]+):\s*(#[0-9a-f]{6});")
        .unwrap()
        .captures_iter(block)
        .map(|capture| (capture[1].to_owned(), capture[2].to_owned()))
        .collect()
}

#[test]
fn the_stylesheet_colours_meet_wcag_aa_in_both_schemes() {
    let css = include_str!("../../static/tocsin.css");
    let light_start = css.find(":root {").unwrap();
    let dark_start = css.find("@media (prefers-color-scheme: dark)").unwrap();
    let light = tokens(&css[light_start..dark_start]);
    let dark = tokens(&css[dark_start..css[dark_start..].find("\n}\n").unwrap() + dark_start]);
    assert_eq!(dark.len(), light.len(), "{dark:?}");
    // Text on either background, including status words in their colour.
    let text = [
        ("fg", "bg"),
        ("fg", "panel"),
        ("muted", "bg"),
        ("muted", "panel"),
        ("ok", "bg"),
        ("ok", "panel"),
        ("warn", "bg"),
        ("warn", "panel"),
        ("bad", "bg"),
        ("bad", "panel"),
        ("accent", "bg"),
        ("accent", "panel"),
        ("accent-fg", "accent"),
        ("bg", "fg"),
    ];
    // Uptime cells and outlines against the page and the surfaces the
    // bars sit on: non-text contrast.
    let graphics = [
        ("ok", "bg"),
        ("warn", "bg"),
        ("bad", "bg"),
        ("muted", "bg"),
        ("ok", "panel"),
        ("warn", "panel"),
        ("bad", "panel"),
        ("muted", "panel"),
    ];
    for (scheme, colours) in [("light", &light), ("dark", &dark)] {
        for (pairs, minimum) in [(&text[..], 4.5), (&graphics[..], 3.0)] {
            for (foreground, background) in pairs {
                let ratio = contrast(&colours[*foreground], &colours[*background]);
                assert!(
                    ratio >= minimum,
                    "{scheme}: --{foreground} on --{background} is {ratio:.2}:1, under {minimum}:1"
                );
            }
        }
    }
    // `[page] accent` is checked against these backgrounds (tocsin-core
    // config/colour.rs), and it keeps --accent-fg: an accent that reads on
    // the light surface reads under white text, and one that reads on the
    // dark background reads under it, by contrast's symmetry.
    for (scheme, colours) in [(LIGHT, &light), (DARK, &dark)] {
        assert_eq!(
            colours["bg"],
            scheme.background.to_string(),
            "{}",
            scheme.name
        );
        assert_eq!(
            colours["panel"],
            scheme.surface.to_string(),
            "{}",
            scheme.name
        );
    }
    assert_eq!(light["accent-fg"], LIGHT.surface.to_string());
    assert_eq!(dark["accent-fg"], DARK.background.to_string());
    // Each day's tone has its own fill, so none rests on colour alone.
    let fills: Vec<&str> = ["up", "degraded", "down", "none"]
        .iter()
        .map(|tone| {
            let rule = format!(".day.{tone} {{");
            let start = css.find(&rule).unwrap_or_else(|| panic!("no {rule}")) + rule.len();
            css[start..start + css[start..].find('}').unwrap()].trim()
        })
        .collect();
    assert_eq!(fills.iter().collect::<HashSet<_>>().len(), 4, "{fills:?}");
    assert!(
        fills[1].contains("repeating-linear-gradient"),
        "{}",
        fills[1]
    );
    assert!(
        fills[2].contains("repeating-linear-gradient"),
        "{}",
        fills[2]
    );
}

/// `[page] accent` reaches the page as a second stylesheet, served under
/// its hash: light as given, dark derived so it still reads.
#[tokio::test]
async fn an_accent_is_served_as_a_stylesheet_the_pages_link() {
    let plain = site("");
    assert!(!plain.page("/").await.contains("/theme/"));
    let (status, _, _) = plain
        .get_with("/theme/0123456789abcdef/accent.css", &[])
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let site = site_from(&CONFIG.replace("timezone = ", "accent = \"#7A1FA2\"\ntimezone = "));
    let accent =
        Regex::new(r#"<link rel="stylesheet" href="(/theme/[0-9a-f]{16}/accent\.css)">"#).unwrap();
    let mut linked = HashSet::new();
    for uri in ["/", "/history", "/components/api", "/releases"] {
        let page = site.page(uri).await;
        let link = accent
            .captures(&page)
            .unwrap_or_else(|| panic!("{uri} links no accent: {page}"));
        linked.insert(link[1].to_owned());
    }
    assert_eq!(linked.len(), 1, "{linked:?}");
    let href = linked.into_iter().next().unwrap();
    let (status, headers, css) = site.get_with(&href, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "text/css; charset=utf-8");
    assert!(
        headers[header::CACHE_CONTROL]
            .to_str()
            .unwrap()
            .contains("immutable")
    );
    let dark = Regex::new(r"prefers-color-scheme: dark\) \{ :root \{ --accent: (#[0-9a-f]{6}); \}")
        .unwrap()
        .captures(&css)
        .unwrap_or_else(|| panic!("{css}"))[1]
        .to_owned();
    assert!(css.contains(":root { --accent: #7a1fa2; }"), "{css}");
    assert_ne!(dark, "#7a1fa2", "{css}");
    for background in [DARK.background, DARK.surface] {
        let ratio = contrast(&dark, &background.to_string());
        assert!(ratio >= 4.5, "{dark} on {background}: {ratio:.2}");
    }
    // A stale hash, as after a reload changed the accent, is gone.
    let (status, _, _) = site
        .get_with(&href.replace("/theme/", "/theme/0"), &[])
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// --- Uptime ----------------------------------------------------------------

#[tokio::test]
async fn the_uptime_bar_shows_today_from_the_rollups() {
    let site = site("");
    let today = Day::containing(now().as_millis());
    for (ago, health) in [
        (1, Health::Up),
        (2, Health::Down),
        (3, Health::Up),
        (4, Health::Up),
    ] {
        site.result(ago, health).await;
    }
    // Rollups are hourly; until one runs the bar has nothing.
    let before = site.page("/fragments/uptime").await;
    assert!(!before.contains("75.00%"), "{before}");
    assert!(
        before.contains(&format!(
            r#"<li class="day none today"><span class="tip">{today}, today: no data yet, the first figure comes within the hour</span></li>"#
        )),
        "{before}"
    );
    site.state.store.rollup_since(today).await.unwrap();
    let after = site.page("/fragments/uptime").await;
    assert!(
        after.contains(&format!(
            r#"<li class="day down today"><span class="tip">{today}, today so far: 75.00% up, worst down</span></li>"#
        )),
        "{after}"
    );
    assert!(
        after.contains("75.00% up over the 1 of the last 90 days with data"),
        "{after}"
    );
    let component = site.page("/components/api").await;
    assert!(
        component.contains(&format!("{today}, today so far: 75.00% up, worst down")),
        "{component}"
    );
    // Only the API has the check.
    let web = site.page("/components/web").await;
    assert!(web.contains("No data in the last 90 days"), "{web}");
}

// --- Archive and pages -------------------------------------------------------

/// A check removed from the configuration while out ends its outage
/// withdrawn: the component page must say so, never that it recovered.
#[tokio::test]
async fn a_withdrawn_outage_never_reads_as_a_recovery() {
    let site = site("");
    let store = &site.state.store;
    let outage = |started_at_ms| NewOutage {
        node_id: "node-a".into(),
        subject_kind: SubjectKind::Check,
        check_id: "api-https".into(),
        component_id: Some("api".into()),
        health: Health::Down,
        started_at_ms,
        alerted: false,
    };
    let start = now().as_millis() - 2 * 3_600_000;
    let recovered = store.open_outage(outage(start)).await.unwrap();
    store.end_outage(recovered, start + 60_000).await.unwrap();
    let withdrawn = store.open_outage(outage(start + 120_000)).await.unwrap();
    store
        .withdraw_outage(withdrawn, start + 180_000)
        .await
        .unwrap();

    let page = site.page("/components/api").await;
    let details: Vec<&str> = page
        .lines()
        .filter(|line| line.contains("on node-a"))
        .collect();
    assert_eq!(details.len(), 2, "{page}");
    // Newest first: the withdrawn outage, then the recovered one.
    assert!(details[0].contains(" · withdrawn "), "{}", details[0]);
    assert!(!details[0].contains(" · ended "), "{}", details[0]);
    assert!(details[1].contains(" · ended "), "{}", details[1]);
}

/// The component page's timeline names checks, components and projects as
/// the rest of the page does, never by their configuration ids.
#[tokio::test]
async fn the_component_timeline_shows_names_not_ids() {
    let site = site(&format!(
        r#"{PUBLIC_PROJECT}
[[check]]
id = "web-https"
name = "Web over HTTPS"
component = "web"
type = "http"
target = "https://www.example.com/"
interval = "30s"
"#
    ));
    site.state
        .store
        .open_outage(NewOutage {
            node_id: "node-a".into(),
            subject_kind: SubjectKind::Check,
            check_id: "web-https".into(),
            component_id: Some("web".into()),
            health: Health::Down,
            started_at_ms: now().as_millis() - 3_600_000,
            alerted: false,
        })
        .await
        .unwrap();
    site.open("Slow pages", IncidentKind::Incident, &["web"])
        .await;
    releases::deployed(
        &site.state,
        "app".into(),
        "v2.0.0".into(),
        tocsin_core::release::DeployStamp::default(),
        Actor::token("deploys"),
    )
    .await
    .unwrap();

    let page = site.page("/components/web").await;
    let timeline = &page[page.find("<h2>Timeline</h2>").unwrap()..];
    for expected in [
        r#"<span class="item-title">Web over HTTPS down</span>"#,
        "· on node-a · Web · ongoing",
        r#"<span class="item-title">Example app v2.0.0 to production</span>"#,
        "· investigating · Web</span>",
    ] {
        assert!(timeline.contains(expected), "{expected:?} in {timeline}");
    }
    assert!(!timeline.contains("web-https"), "{timeline}");
    assert!(!page.contains("web-https"), "{page}");
}

#[tokio::test]
async fn the_history_pages_through_every_incident_newest_first() {
    let site = site("");
    for number in 1..=27 {
        site.open(
            &format!("Incident number {number}"),
            IncidentKind::Incident,
            &["api"],
        )
        .await;
    }
    let titles = |page: &str| -> Vec<String> {
        elements(page)
            .iter()
            .filter(|element| element.has_class("item-title"))
            .map(Element::words)
            .collect()
    };
    let first = site.page("/history").await;
    let expected: Vec<String> = (3..=27)
        .rev()
        .map(|number| format!("Incident number {number}"))
        .collect();
    assert_eq!(titles(&first), expected);
    assert!(
        first.contains(r#"<a href="/history?before=3" rel="next">Older incidents</a>"#),
        "{first}"
    );
    let second = site.page("/history?before=3").await;
    assert_eq!(titles(&second), ["Incident number 2", "Incident number 1"]);
    assert!(!second.contains("Older incidents"), "{second}");
    let (status, _, _) = site.get_with("/history?before=soon", &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn unknown_pages_are_404() {
    let site = site("");
    for uri in [
        "/incidents/999",
        "/incidents/abc",
        "/components/nope",
        "/fragments/nope",
        // No public project, so no releases section to refresh.
        "/fragments/releases",
    ] {
        let (status, _, _) = site.get_with(uri, &[]).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
    assert!(!site.page("/").await.contains("releases-heading"));
}

/// The JSON-LD in `page`.
fn json_ld(page: &str) -> Value {
    let start = page.find(r#"<script type="application/ld+json">"#).unwrap()
        + r#"<script type="application/ld+json">"#.len();
    let end = start + page[start..].find("</script>").unwrap();
    // Nothing a tokenizer reads as markup: not `</script`, and not `<!--`,
    // which would change how it reads the rest of the element.
    let body = &page[start..end];
    assert!(!body.contains(['<', '>', '&']), "{body}");
    serde_json::from_str(body).unwrap()
}

#[tokio::test]
async fn pages_describe_themselves_and_their_incidents_in_json_ld() {
    let site = site("");
    let title = "</script><script>alert(1)</script> & co";
    let id = site.open(title, IncidentKind::Incident, &["api"]).await;
    let index = json_ld(&site.page("/").await);
    assert_eq!(index["@context"], "https://schema.org");
    assert_eq!(
        index["@graph"][0],
        json!({
            "@type": "WebPage",
            "name": "Example status",
            "url": "https://status.example.com/",
            "description": "Live status of Example's services.",
        })
    );
    let event = &index["@graph"][1];
    assert_eq!(event["@type"], "Event");
    assert_eq!(event["name"], title);
    assert_eq!(
        event["url"],
        format!("https://status.example.com/incidents/{id}")
    );
    assert_eq!(event["location"]["@type"], "VirtualLocation");
    assert_eq!(event["description"], format!("Looking into **{title}**."));
    assert!(
        event["startDate"].as_str().unwrap().ends_with('Z'),
        "{event}"
    );
    assert!(event.get("endDate").is_none(), "{event}");

    site.resolve(id).await;
    let page = json_ld(&site.page(&format!("/incidents/{id}")).await);
    assert_eq!(page["@graph"][0]["@type"], "WebPage");
    assert_eq!(page["@graph"][1]["name"], title);
    assert!(page["@graph"][1]["endDate"].is_string(), "{page}");
    assert_eq!(page["@graph"][1]["description"], "Fixed.");
}

#[tokio::test]
async fn components_show_in_the_page_order_ungrouped_first() {
    let site = site("");
    let elements = elements(&site.page("/fragments/components").await);
    let order: Vec<String> = elements
        .iter()
        .filter(|element| {
            element.name == "h3"
                || (element.name == "a"
                    && element
                        .attribute("href")
                        .is_some_and(|href| href.starts_with("/components/")))
        })
        .map(|element| format!("{} {}", element.name, element.words()))
        .collect();
    assert_eq!(
        order,
        ["a Docs", "h3 Edge", "a Web", "h3 Core", "a Jobs", "a API"]
    );
}

#[tokio::test]
async fn times_show_in_the_page_time_zone() {
    let site = site("");
    let id = site
        .open("Elevated API errors", IncidentKind::Incident, &["api"])
        .await;
    let page = site.page(&format!("/incidents/{id}")).await;
    let incident = site.state.store.incident(id).await.unwrap().unwrap();
    let local = super::page::when_in(
        incident.created_at,
        &jiff::tz::TimeZone::get("Asia/Kolkata").unwrap(),
    );
    assert!(local.ends_with("IST"), "{local}");
    assert!(page.contains(&format!(">{local}</time>")), "{page}");
    assert!(
        page.contains(&format!(r#"datetime="{}""#, incident.created_at)),
        "{page}"
    );
}
