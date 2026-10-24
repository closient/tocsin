// SPDX-License-Identifier: AGPL-3.0-only

//! The Atom feeds: each one checked against what RFC 4287 requires (the
//! rules the W3C feed validator enforces), entry ids that hold across
//! updates, edits and configuration, escaping, what stays private, the
//! discovery links, and absolute links without `public_url`.

use std::collections::HashSet;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use regex::Regex;
use tocsin_core::incident::{
    Actor, Impact, IncidentEdit, IncidentKind, IncidentStatus, NewIncident, NewUpdate, Timestamp,
};
use tocsin_core::release::NewRelease;
use tower::ServiceExt;

use super::status::PAGE_CACHE;
use super::status_tests::{PUBLIC_PROJECT, Site, site};
use crate::http::{self, AppState, incidents, now, releases};

const ATOM: &str = "http://www.w3.org/2005/Atom";

/// A project the public never sees.
const PRIVATE_PROJECT: &str = r#"
[[project]]
id = "internal"
name = "Internal tool"
public = false
"#;

// --- The check ---------------------------------------------------------------

/// Where `xml` breaks what RFC 4287 requires of a feed; empty when it is
/// valid. These are the errors the W3C feed validator reports: one `id`,
/// `title` and `updated` on the feed and on every entry; an author on the
/// feed or on every entry; a self link of the feed's own type; content or
/// an alternate link on every entry; unique entry ids; ids and links that
/// are absolute IRIs; and RFC 3339 dates. And two of its warnings: an
/// entry without text, and one whose content is blank.
fn atom_problems(xml: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if !xml.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n") {
        problems.push("the XML declaration is not first".to_owned());
    }
    let document = match roxmltree::Document::parse(xml) {
        Ok(document) => document,
        Err(error) => {
            problems.push(format!("not XML: {error}"));
            return problems;
        }
    };
    let feed = document.root_element();
    if !feed.has_tag_name((ATOM, "feed")) {
        problems.push(format!("the root is {:?}", feed.tag_name()));
        return problems;
    }
    check_common(feed, "the feed", &mut problems);
    let selves: Vec<_> = children(feed, "link")
        .into_iter()
        .filter(|link| link.attribute("rel") == Some("self"))
        .collect();
    if selves.len() != 1 || selves[0].attribute("type") != Some("application/atom+xml") {
        problems.push("the feed has no one self link of type application/atom+xml".to_owned());
    }
    let feed_author = children(feed, "author")
        .iter()
        .any(|author| !children(*author, "name").is_empty());
    let mut ids = HashSet::new();
    for (index, entry) in children(feed, "entry").into_iter().enumerate() {
        let what = format!("entry {index}");
        check_common(entry, &what, &mut problems);
        if !feed_author && children(entry, "author").is_empty() {
            problems.push(format!("{what} has no author, nor has the feed"));
        }
        let alternate = children(entry, "link")
            .iter()
            .any(|link| link.attribute("rel").unwrap_or("alternate") == "alternate");
        if children(entry, "content").is_empty() && !alternate {
            problems.push(format!("{what} has neither content nor an alternate link"));
        }
        // The validator's warnings: a reader shows an empty body.
        if children(entry, "content")
            .iter()
            .any(|content| content.text().unwrap_or_default().trim().is_empty())
        {
            problems.push(format!("{what} has blank content"));
        }
        if children(entry, "content").is_empty() && children(entry, "summary").is_empty() {
            problems.push(format!("{what} has no textual content"));
        }
        if let Some(id) = children(entry, "id")
            .first()
            .and_then(roxmltree::Node::text)
            && !ids.insert(id.to_owned())
        {
            problems.push(format!("{what} repeats id {id}"));
        }
    }
    problems
}

/// `node`'s Atom children called `name`.
fn children<'a, 'input>(
    node: roxmltree::Node<'a, 'input>,
    name: &str,
) -> Vec<roxmltree::Node<'a, 'input>> {
    node.children()
        .filter(|child| child.has_tag_name((ATOM, name)))
        .collect()
}

/// What a feed and an entry both need: one `id`, `title` and `updated`,
/// RFC 3339 dates, an absolute IRI for an id, and absolute links.
fn check_common(node: roxmltree::Node<'_, '_>, what: &str, problems: &mut Vec<String>) {
    let rfc3339 =
        Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})$").unwrap();
    for name in ["id", "title", "updated"] {
        let found = children(node, name);
        if found.len() != 1 {
            problems.push(format!("{what} has {} <{name}>", found.len()));
        }
    }
    for name in ["published", "updated"] {
        for date in children(node, name) {
            let text = date.text().unwrap_or_default();
            if !rfc3339.is_match(text) || text.parse::<jiff::Timestamp>().is_err() {
                problems.push(format!("{what} <{name}> {text:?} is not RFC 3339"));
            }
        }
    }
    for id in children(node, "id") {
        let text = id.text().unwrap_or_default();
        if url::Url::parse(text).is_err() {
            problems.push(format!("{what} <id> {text:?} is not an absolute IRI"));
        }
    }
    for link in children(node, "link") {
        let href = link.attribute("href").unwrap_or_default();
        if url::Url::parse(href).is_err() {
            problems.push(format!("{what} link {href:?} is not absolute"));
        }
    }
}

/// A minimal valid feed with `entries` spliced in.
fn sample(head: &str, entries: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <feed xmlns=\"http://www.w3.org/2005/Atom\">{head}{entries}</feed>"
    )
}

const HEAD: &str = "<id>urn:x:feed</id><title>T</title>\
    <updated>2026-10-23T10:00:00.000Z</updated>\
    <link rel=\"self\" type=\"application/atom+xml\" href=\"https://x.example/f.atom\"/>\
    <author><name>X</name></author>";

const ENTRY: &str = "<entry><id>urn:x:1</id><title>One</title>\
    <updated>2026-10-23T10:00:00.000Z</updated>\
    <link href=\"https://x.example/1\"/><summary>The first.</summary></entry>";

#[test]
fn the_feed_check_finds_what_it_looks_for() {
    assert_eq!(atom_problems(&sample(HEAD, ENTRY)), Vec::<String>::new());
    let broken = [
        (
            sample(
                &HEAD.replace("<updated>2026-10-23T10:00:00.000Z</updated>", ""),
                ENTRY,
            ),
            "the feed has 0 <updated>",
        ),
        (
            sample(HEAD, &format!("{ENTRY}{ENTRY}")),
            "repeats id urn:x:1",
        ),
        (
            sample(HEAD, &ENTRY.replace("urn:x:1", "/incidents/1")),
            "is not an absolute IRI",
        ),
        (
            sample(HEAD, &ENTRY.replace("https://x.example/1", "/incidents/1")),
            "is not absolute",
        ),
        (
            sample(
                HEAD,
                &ENTRY.replace("2026-10-23T10:00:00.000Z", "2026-10-23 10:00"),
            ),
            "is not RFC 3339",
        ),
        (
            sample(&HEAD.replace("<author><name>X</name></author>", ""), ENTRY),
            "has no author",
        ),
        (
            sample(&HEAD.replace("application/atom+xml", "text/xml"), ENTRY),
            "self link",
        ),
        (
            sample(
                HEAD,
                &ENTRY.replace("<link href=\"https://x.example/1\"/>", ""),
            ),
            "neither content nor an alternate link",
        ),
        (
            sample(
                HEAD,
                &ENTRY.replace("</entry>", "<content type=\"html\"> </content></entry>"),
            ),
            "blank content",
        ),
        (
            sample(HEAD, &ENTRY.replace("<summary>The first.</summary>", "")),
            "no textual content",
        ),
        (sample(HEAD, &ENTRY.replace("One", "One\u{1}")), "not XML"),
        (
            sample(HEAD, ENTRY).replace("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n", ""),
            "XML declaration",
        ),
    ];
    for (xml, expected) in broken {
        let problems = atom_problems(&xml);
        assert!(
            problems.iter().any(|problem| problem.contains(expected)),
            "{expected:?} not in {problems:?}"
        );
    }
}

// --- Fetching ----------------------------------------------------------------

/// Status, content type and body of `GET uri` from `router`, with `Host:
/// host` if given.
async fn fetch(router: &Router, uri: &str, host: Option<&str>) -> (StatusCode, String, String) {
    let mut request = Request::builder().uri(uri);
    if let Some(host) = host {
        request = request.header(header::HOST, host);
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|value| value.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        content_type,
        String::from_utf8(bytes.to_vec()).unwrap(),
    )
}

impl Site {
    /// The feed at `uri`, which must be there and be valid.
    async fn feed(&self, uri: &str) -> String {
        let (status, headers, body) = self.get_with(uri, &[]).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        assert_eq!(
            headers[header::CONTENT_TYPE],
            "application/atom+xml; charset=utf-8",
            "{uri}"
        );
        assert_eq!(atom_problems(&body), Vec::<String>::new(), "{uri}:\n{body}");
        body
    }
}

/// One entry as a reader sees it.
#[derive(Debug, Clone, PartialEq)]
struct Read {
    id: String,
    title: String,
    updated: String,
    content: String,
}

/// The feed's own id, its `updated`, and its entries, in order.
fn read(xml: &str) -> (String, String, Vec<Read>) {
    let document = roxmltree::Document::parse(xml).unwrap();
    let text = |node: roxmltree::Node<'_, '_>, name: &str| {
        node.children()
            .find(|child| child.has_tag_name((ATOM, name)))
            .and_then(|child| child.text())
            .unwrap_or_default()
            .to_owned()
    };
    let feed = document.root_element();
    let entries = feed
        .children()
        .filter(|child| child.has_tag_name((ATOM, "entry")))
        .map(|entry| Read {
            id: text(entry, "id"),
            title: text(entry, "title"),
            updated: text(entry, "updated"),
            content: text(entry, "content"),
        })
        .collect();
    (text(feed, "id"), text(feed, "updated"), entries)
}

/// The `href` of every `<link rel="alternate" type="application/atom+xml">`
/// in `page`, in order.
fn advertised(page: &str) -> Vec<String> {
    Regex::new(
        r#"<link rel="alternate" type="application/atom\+xml" title="[^"]*" href="([^"]*)">"#,
    )
    .unwrap()
    .captures_iter(page)
    .map(|captures| captures[1].to_owned())
    .collect()
}

// --- The feeds ---------------------------------------------------------------

#[tokio::test]
async fn every_feed_is_valid_atom() {
    let site = site(PUBLIC_PROJECT);
    site.furnish().await;
    for (uri, entries) in [
        ("/feeds/incidents.atom", 3),
        ("/feeds/components/api.atom", 1),
        ("/feeds/components/docs.atom", 0),
        ("/feeds/releases.atom", 1),
        ("/feeds/releases/app.atom", 1),
        ("/releases.atom", 1),
        ("/releases.atom?project=app", 1),
    ] {
        let body = site.feed(uri).await;
        let (_, updated, read) = read(&body);
        assert_eq!(read.len(), entries, "{uri}");
        // The feed is as new as its newest entry; with none, the epoch,
        // so an empty feed's body and tag hold still.
        let newest = read.iter().map(|entry| entry.updated.clone()).max();
        assert_eq!(
            updated,
            newest.unwrap_or_else(|| Timestamp::from_millis(0).to_string()),
            "{uri}"
        );
    }
    let body = site.feed("/feeds/incidents.atom").await;
    for expected in [
        "<id>urn:tocsin:incidents</id>",
        "<title>Example status incidents</title>",
        "<subtitle>Live status of Example&#39;s services.</subtitle>",
        r#"<link rel="self" type="application/atom+xml" href="https://status.example.com/feeds/incidents.atom"/>"#,
        r#"<link rel="alternate" type="text/html" href="https://status.example.com/history"/>"#,
        "<name>Example status</name>",
        "<uri>https://example.com/</uri>",
        r#"<category term="maintenance"/>"#,
    ] {
        assert!(body.contains(expected), "{expected} not in {body}");
    }
    let body = site.feed("/feeds/components/api.atom").await;
    assert!(body.contains("<id>urn:tocsin:incidents:api</id>"), "{body}");
    assert!(
        body.contains("<title>API incidents · Example status</title>"),
        "{body}"
    );
    assert!(body.contains("Elevated API errors"), "{body}");
    assert!(!body.contains("Slow page loads"), "{body}");
    let body = site.feed("/feeds/releases/app.atom").await;
    assert!(body.contains("<id>urn:tocsin:releases:app</id>"), "{body}");
    assert!(
        body.contains("<id>urn:tocsin:release:app:1.2.0</id>"),
        "{body}"
    );
}

#[tokio::test]
async fn the_old_release_feed_path_serves_the_same_feeds() {
    let site = site(PUBLIC_PROJECT);
    site.release("1.2.0", "Faster search").await;
    assert_eq!(
        site.feed("/releases.atom").await,
        site.feed("/feeds/releases.atom").await
    );
    assert_eq!(
        site.feed("/releases.atom?project=app").await,
        site.feed("/feeds/releases/app.atom").await
    );
}

#[tokio::test]
async fn an_entry_keeps_its_id_through_updates_edits_and_new_configuration() {
    let site = site(PUBLIC_PROJECT);
    let id = site
        .open("Elevated API errors", IncidentKind::Incident, &["api"])
        .await;
    let entry = |xml: &str| {
        let (feed, _, entries) = read(xml);
        assert_eq!(feed, "urn:tocsin:incidents");
        assert_eq!(entries.len(), 1, "{xml}");
        entries[0].clone()
    };
    let opened = entry(&site.feed("/feeds/incidents.atom").await);
    assert_eq!(opened.id, format!("urn:tocsin:incident:{id}"));

    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    incidents::append(
        &site.state,
        id,
        NewUpdate {
            status: IncidentStatus::Identified,
            message: "Found the bad deploy.".to_owned(),
            notify: true,
        },
        Actor::person("alice@example.com"),
    )
    .await
    .unwrap();
    let updated = entry(&site.feed("/feeds/incidents.atom").await);
    assert_eq!(updated.id, opened.id);
    assert!(updated.updated > opened.updated, "{updated:?} {opened:?}");
    assert!(
        updated.content.contains("Found the bad deploy."),
        "{updated:?}"
    );

    site.state
        .store
        .edit_incident(
            id,
            IncidentEdit {
                title: Some("Elevated API error rate".to_owned()),
                ..IncidentEdit::default()
            },
            Actor::person("alice@example.com"),
            Timestamp::from_millis(now().as_millis() + 60_000),
        )
        .await
        .unwrap()
        .unwrap();
    let edited = entry(&site.feed("/feeds/incidents.atom").await);
    assert_eq!(edited.id, opened.id);
    assert_eq!(edited.title, "Elevated API error rate");
    assert!(edited.updated > updated.updated, "{edited:?} {updated:?}");

    // A reload that drops `public_url`, over the same database, changes
    // the links and not one id.
    let mut config = (*site.state.config).clone();
    config.server.public_url = None;
    let reloaded = http::router(AppState {
        config: Arc::new(config),
        ..site.state.clone()
    });
    let (status, _, body) = fetch(&reloaded, "/feeds/incidents.atom", Some("other.example")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(entry(&body).id, opened.id);
    assert!(body.contains("http://other.example/incidents/"), "{body}");
}

#[tokio::test]
async fn text_is_escaped_once_and_reads_back_exactly() {
    let site = site(PUBLIC_PROJECT);
    let title = r#"Tom & Jerry <b>"bold"</b> 'quoted' — ünïcødé ✓"#;
    incidents::open(
        &site.state,
        NewIncident {
            kind: IncidentKind::Incident,
            title: title.to_owned(),
            impact: Impact::Minor,
            components: vec!["api".to_owned()],
            status: None,
            message: "Raw <script>alert(1)</script> & a bell \u{1} here; ✓".to_owned(),
            scheduled_start: None,
            scheduled_end: None,
            notify: true,
        },
        Actor::person("alice@example.com"),
    )
    .await
    .unwrap();
    for uri in ["/feeds/incidents.atom", "/feeds/components/api.atom"] {
        // Valid at all means the U+0001 is gone: XML 1.0 cannot carry it.
        let body = site.feed(uri).await;
        let (_, _, entries) = read(&body);
        assert_eq!(entries[0].title, title, "{body}");
        let content = &entries[0].content;
        assert!(!content.contains("<script"), "{content}");
        assert!(content.contains("&lt;script&gt;"), "{content}");
        assert!(content.contains("&amp; a bell  here; ✓"), "{content}");
        assert!(!body.contains("<script") && !body.contains("<b>"), "{body}");
        assert!(body.contains("ünïcødé ✓"), "{body}");
    }
}

#[tokio::test]
async fn unknown_and_private_feeds_are_404() {
    let site = site(&format!("{PUBLIC_PROJECT}{PRIVATE_PROJECT}"));
    site.release("1.2.0", "Faster search").await;
    releases::publish(
        &site.state,
        NewRelease {
            project: "internal".to_owned(),
            version: "9.9.9".to_owned(),
            environment: None,
            title: Some("Secret thing".to_owned()),
            notes: String::new(),
            url: None,
            released_at: Some(Timestamp::from_millis(1_760_000_000_000)),
            deployed_at: None,
        },
        Actor::token("deploys"),
    )
    .await
    .unwrap();
    for uri in [
        "/feeds/components/nope.atom",
        "/feeds/components/api",
        "/feeds/components/api.rss",
        "/feeds/components/.atom",
        "/feeds/releases/internal.atom",
        "/feeds/releases/nope.atom",
        "/feeds/releases/.atom",
        "/releases.atom?project=internal",
        "/releases.atom?project=nope",
        "/feeds/incidents",
    ] {
        let (status, _, body) = site.get_with(uri, &[]).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}: {body}");
    }
    for uri in ["/feeds/releases.atom", "/releases.atom"] {
        let body = site.feed(uri).await;
        assert!(body.contains("Faster search"), "{body}");
        assert!(
            !body.contains("internal") && !body.contains("Secret thing"),
            "{body}"
        );
    }
    for uri in ["/", "/releases", "/history", "/components/api"] {
        let page = site.page(uri).await;
        assert!(
            advertised(&page)
                .iter()
                .all(|href| !href.contains("internal")),
            "{uri}"
        );
    }
}

#[tokio::test]
async fn pages_advertise_their_feeds_and_every_one_is_there() {
    let site = site(PUBLIC_PROJECT);
    site.furnish().await;
    let incident = site
        .open("Elevated API errors", IncidentKind::Incident, &["api"])
        .await;
    let both = ["/feeds/incidents.atom", "/feeds/releases.atom"];
    for (uri, expected) in [
        ("/".to_owned(), both.to_vec()),
        ("/history".to_owned(), both.to_vec()),
        (format!("/incidents/{incident}"), both.to_vec()),
        (
            "/components/api".to_owned(),
            vec!["/feeds/components/api.atom", both[0], both[1]],
        ),
        ("/releases".to_owned(), both.to_vec()),
        (
            "/releases?project=app".to_owned(),
            vec!["/feeds/releases/app.atom", both[0], both[1]],
        ),
        (
            "/releases/app/1.2.0".to_owned(),
            vec!["/feeds/releases/app.atom", both[0], both[1]],
        ),
    ] {
        let page = site.page(&uri).await;
        assert_eq!(advertised(&page), expected, "{uri}");
        for href in advertised(&page) {
            site.feed(&href).await;
        }
    }
    let page = site.page("/").await;
    assert!(
        page.contains(r#"<link rel="alternate" type="application/atom+xml" title="Example status incidents" href="/feeds/incidents.atom">"#),
        "{page}"
    );
    let page = site.page("/releases?project=app").await;
    assert!(
        page.contains(r#"href="/feeds/releases/app.atom""#),
        "{page}"
    );

    // Nothing public to release: no release feed to point at.
    let site = super::status_tests::site("");
    assert_eq!(
        advertised(&site.page("/").await),
        vec!["/feeds/incidents.atom"]
    );
}

#[tokio::test]
async fn links_are_absolute_from_the_host_when_there_is_no_public_url() {
    let site = site(PUBLIC_PROJECT);
    site.furnish().await;
    let mut config = (*site.state.config).clone();
    config.server.public_url = None;
    let router = http::router(AppState {
        config: Arc::new(config),
        ..site.state.clone()
    });
    let (status, content_type, body) = fetch(
        &router,
        "/feeds/incidents.atom",
        Some("status.internal:8080"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type, "application/atom+xml; charset=utf-8");
    assert_eq!(atom_problems(&body), Vec::<String>::new(), "{body}");
    assert!(
        body.contains(r#"href="http://status.internal:8080/feeds/incidents.atom""#),
        "{body}"
    );
    // A `Host` no authority could be, or none at all, leaves the links
    // relative rather than pointing anywhere a request chose.
    for host in [Some("user@evil.example"), None] {
        let (status, _, body) = fetch(&router, "/feeds/incidents.atom", host).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains(
                r#"<link rel="self" type="application/atom+xml" href="/feeds/incidents.atom"/>"#
            ),
            "{host:?}: {body}"
        );
        assert!(!body.contains("evil"), "{body}");
    }
}

#[tokio::test]
async fn feeds_are_cacheable_and_revalidate_by_etag() {
    let site = site(PUBLIC_PROJECT);
    site.furnish().await;
    for uri in ["/feeds/incidents.atom", "/feeds/releases/app.atom"] {
        let (status, headers, _) = site.get_with(uri, &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CACHE_CONTROL], PAGE_CACHE);
        let tag = headers[header::ETAG].to_str().unwrap().to_owned();
        let (status, _, body) = site
            .get_with(uri, &[(header::IF_NONE_MATCH, tag.as_str())])
            .await;
        assert_eq!(status, StatusCode::NOT_MODIFIED, "{uri}");
        assert_eq!(body, "");
    }
    let (_, headers, _) = site.get_with("/feeds/incidents.atom", &[]).await;
    let before = headers[header::ETAG].clone();
    site.open("Another", IncidentKind::Incident, &["web"]).await;
    let (_, headers, _) = site.get_with("/feeds/incidents.atom", &[]).await;
    assert_ne!(headers[header::ETAG], before);
}
