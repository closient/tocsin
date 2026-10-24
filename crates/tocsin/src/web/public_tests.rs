// SPDX-License-Identifier: AGPL-3.0-only

//! Checks on no component stay off every public surface (TOC-69).
//!
//! The routes come from the router itself, not from a list kept by hand:
//! a route added without a line in [`ROUTES`] fails
//! [`every_route_is_classified`]. Each public route is then read with no
//! token and with a token lacking `status:read`, and must name neither the
//! internal check nor the internal heartbeat; the event stream and the MCP
//! tools are read the same way. The internal board and a `status:read`
//! token are the controls: they see both, so an absence on a public
//! surface is a filter at work and not a missing seed.

use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use futures_util::StreamExt;
use regex::Regex;
use serde_json::json;
use tocsin_core::Health::{Down, Up};
use tocsin_core::incident::{Actor, Impact, IncidentKind, NewIncident, Timestamp};
use tocsin_core::release::NewRelease;
use tocsin_engine::{CheckChange, EngineEvent, MemberKind};
use tocsin_store::{NewOutage, SubjectKind};

use super::assets;
use super::identity::AdminAuth;
use super::internal_tests::{
    Board, EMAIL_HEADER, ONCALL, READER, TRUSTED_HEADER, config, parts, serve,
};
use super::status_tests::PUBLIC_PROJECT;
use super::tests::{ALICE, OUTSIDER, PROXY, from, send};
use crate::http::events::Event;
use crate::http::{OPENAPI_PATH, incidents, now, releases};

/// What names the internal check and heartbeat: their ids, names and the
/// check's target.
const HIDDEN: [&str; 5] = [
    "backup-disk",
    "Backup disk",
    "nightly-dump",
    "Nightly dump",
    "backup.internal.example.com",
];

/// [`CONFIG`](super::status_tests::CONFIG)'s check on the API component.
const SHOWN: &str = "api-https";

/// A token with `status:read`, beside the `oncall` token that lacks it.
const READER_TOKEN: &str = r#"
[[token]]
id = "reader"
secret = "${ENV:READER_TOKEN}"
scopes = ["status:read"]
"#;

/// What an anonymous `GET` of a route may answer.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Class {
    /// 200, naming nothing internal.
    Public,
    /// Compiled into the binary; whatever the status, nothing internal.
    Asset,
    /// 401, 403, 404 or 405: nothing to read without a token or sign-in.
    Refused,
    /// A redirect to another route, which is classified in its own right.
    Redirect,
    /// The public event stream; [`the_event_stream_leaves_out_internal_changes`].
    Stream,
    /// The MCP endpoint; [`mcp_without_status_read_leaves_out_internal_checks`].
    Mcp,
}

/// Every route the router serves, by its path pattern, with what an
/// anonymous reader gets and the concrete paths that exercise it.
/// `{incident}` stands for the seeded incident's id and `{asset}` for the
/// stylesheet's hashed path.
const ROUTES: &[(&str, Class, &[&str])] = &[
    // The status page and its fragments.
    ("/", Class::Public, &["/"]),
    ("/history", Class::Public, &["/history"]),
    ("/components/{id}", Class::Public, &["/components/api"]),
    (
        "/fragments/{name}",
        Class::Public,
        &[
            "/fragments/banner",
            "/fragments/components",
            "/fragments/uptime",
            "/fragments/incidents",
            "/fragments/resolved",
            "/fragments/releases",
        ],
    ),
    ("/incidents/{id}", Class::Public, &["/incidents/{incident}"]),
    ("/releases", Class::Public, &["/releases"]),
    (
        "/releases/{project}/{version}",
        Class::Public,
        &["/releases/app/1.0.0"],
    ),
    // Feeds.
    ("/releases.atom", Class::Public, &["/releases.atom"]),
    (
        "/feeds/incidents.atom",
        Class::Public,
        &["/feeds/incidents.atom"],
    ),
    (
        "/feeds/releases.atom",
        Class::Public,
        &["/feeds/releases.atom"],
    ),
    (
        "/feeds/components/{file}",
        Class::Public,
        &["/feeds/components/api.atom"],
    ),
    (
        "/feeds/releases/{file}",
        Class::Public,
        &["/feeds/releases/app.atom"],
    ),
    // For agents.
    ("/llms.txt", Class::Public, &["/llms.txt"]),
    ("/mcp", Class::Mcp, &["/mcp"]),
    // The read API.
    (
        "/api/v1/status.json",
        Class::Public,
        &["/api/v1/status.json"],
    ),
    (
        "/api/v1/components/{id}",
        Class::Public,
        &["/api/v1/components/api"],
    ),
    ("/api/v1/checks", Class::Public, &["/api/v1/checks"]),
    ("/api/v1/outages", Class::Public, &["/api/v1/outages"]),
    ("/api/v1/incidents", Class::Public, &["/api/v1/incidents"]),
    (
        "/api/v1/incidents/{id}",
        Class::Public,
        &["/api/v1/incidents/{incident}"],
    ),
    ("/api/v1/releases", Class::Public, &["/api/v1/releases"]),
    (
        "/api/v1/timeline",
        Class::Public,
        &["/api/v1/timeline", "/api/v1/timeline?component=api"],
    ),
    ("/api/v1/openapi.json", Class::Public, &[OPENAPI_PATH]),
    ("/api/v1/events", Class::Stream, &["/api/v1/events"]),
    // Reads that need `status:read`.
    (
        "/api/v1/checks/{id}/results",
        Class::Refused,
        &[
            "/api/v1/checks/backup-disk/results",
            "/api/v1/checks/nightly-dump/results",
        ],
    ),
    (
        "/api/v1/config/status",
        Class::Refused,
        &["/api/v1/config/status"],
    ),
    ("/metrics", Class::Refused, &["/metrics"]),
    // Writes.
    (
        "/api/v1/heartbeat/{id}",
        Class::Refused,
        &["/api/v1/heartbeat/nightly-dump"],
    ),
    (
        "/api/v1/incidents/{id}/updates",
        Class::Refused,
        &["/api/v1/incidents/{incident}/updates"],
    ),
    (
        "/api/v1/releases/{project}/{version}/deployed",
        Class::Refused,
        &["/api/v1/releases/app/1.0.0/deployed"],
    ),
    // Behind proxy auth.
    ("/admin", Class::Redirect, &["/admin"]),
    ("/admin/", Class::Refused, &["/admin/"]),
    ("/admin/incidents", Class::Redirect, &["/admin/incidents"]),
    (
        "/admin/incidents/{id}",
        Class::Refused,
        &["/admin/incidents/{incident}"],
    ),
    (
        "/admin/incidents/{id}/updates",
        Class::Redirect,
        &["/admin/incidents/{incident}/updates"],
    ),
    ("/admin/timeline", Class::Refused, &["/admin/timeline"]),
    ("/internal/", Class::Refused, &["/internal/"]),
    (
        "/internal/fragments/board",
        Class::Refused,
        &["/internal/fragments/board"],
    ),
    (
        "/internal/checks/{id}",
        Class::Refused,
        &["/internal/checks/backup-disk"],
    ),
    (
        "/internal/checks/{id}/live",
        Class::Refused,
        &["/internal/checks/backup-disk/live"],
    ),
    ("/internal/events", Class::Refused, &["/internal/events"]),
    // Files.
    ("/static/{hash}/{name}", Class::Asset, &["{asset}"]),
    (
        "/theme/{hash}/accent.css",
        Class::Asset,
        &["/theme/0/accent.css"],
    ),
    // Anything else.
    (
        "/{*__private__axum_fallback}",
        Class::Refused,
        &["/sitemap.xml"],
    ),
];

/// Every path pattern `router` serves, from its `Debug` output, which
/// lists each route as `RouteId(n): "/path"`.
fn served_routes(router: &axum::Router) -> Vec<String> {
    let pattern = Regex::new(r#"RouteId\(\d+\): "([^"]*)""#).unwrap();
    let mut paths: Vec<String> = pattern
        .captures_iter(&format!("{router:?}"))
        .map(|captures| captures[1].to_owned())
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// A board behind a trusted proxy with a public project and a
/// `status:read` token, seeded with results, outages of all three checks,
/// an incident and a deployed release. Returns the incident's id.
async fn seeded() -> (Board, String) {
    let config = config(&format!("{TRUSTED_HEADER}{PUBLIC_PROJECT}{READER_TOKEN}"));
    let admin = AdminAuth::from_config(&config).unwrap();
    let board = serve(config, admin);
    board.furnish().await;
    let started = now().as_millis() - 3_600_000;
    for (check_id, subject_kind, component_id) in [
        (SHOWN, SubjectKind::Check, Some("api")),
        ("backup-disk", SubjectKind::Check, None),
        ("nightly-dump", SubjectKind::Heartbeat, None),
    ] {
        board
            .state
            .store
            .open_outage(NewOutage {
                node_id: "node-a".into(),
                subject_kind,
                check_id: check_id.into(),
                component_id: component_id.map(Into::into),
                health: Down,
                started_at_ms: started,
                alerted: false,
            })
            .await
            .unwrap();
    }
    let incident = incidents::open(
        &board.state,
        NewIncident {
            kind: IncidentKind::Incident,
            title: "Slow API".to_owned(),
            impact: Impact::Minor,
            components: vec!["api".to_owned()],
            status: None,
            message: "Looking into it.".to_owned(),
            scheduled_start: None,
            scheduled_end: None,
        },
        Actor::person(ALICE),
    )
    .await
    .unwrap();
    releases::publish(
        &board.state,
        NewRelease {
            project: "app".to_owned(),
            version: "1.0.0".to_owned(),
            environment: None,
            title: Some("First".to_owned()),
            notes: String::new(),
            url: None,
            released_at: Some(Timestamp::from_millis(started)),
            deployed_at: Some(Timestamp::from_millis(started)),
        },
        Actor::token("deploys"),
    )
    .await
    .unwrap();
    (board, incident.id.to_string())
}

/// `GET uri` from outside, with `token` as a bearer token if there is one.
async fn read(board: &Board, uri: &str, token: Option<&str>) -> (StatusCode, String) {
    let mut request = from(OUTSIDER, Method::GET, uri);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let (status, _, body) = parts(
        send(&board.router, request.body(Body::empty()).unwrap())
            .await
            .1,
    )
    .await;
    (status, body)
}

fn assert_hidden(body: &str, surface: &str) {
    for hidden in HIDDEN {
        assert!(!body.contains(hidden), "{hidden} on {surface}: {body}");
    }
}

// --- The census ----------------------------------------------------------

#[tokio::test]
async fn every_route_is_classified() {
    let (board, _) = seeded().await;
    let served = served_routes(&board.router);
    assert!(
        served.len() > 30,
        "the router's Debug output changed: {served:?}"
    );
    let mut classified: Vec<String> = ROUTES
        .iter()
        .map(|(pattern, ..)| (*pattern).to_owned())
        .collect();
    classified.sort();
    assert_eq!(
        served, classified,
        "a route was added or removed: classify it in `ROUTES` (left: served, right: classified)"
    );
}

#[tokio::test]
async fn no_anonymous_route_names_an_internal_check() {
    let (board, incident) = seeded().await;
    for &(pattern, class, uris) in ROUTES {
        if matches!(class, Class::Stream | Class::Mcp) {
            continue;
        }
        for uri in uris {
            let uri = uri
                .replace("{incident}", &incident)
                .replace("{asset}", assets::url("tocsin.css"));
            // No token, and a token without `status:read`: both the public.
            for token in [None, Some(ONCALL)] {
                let (status, body) = read(&board, &uri, token).await;
                let surface = format!("{uri} ({pattern}, token {token:?})");
                match class {
                    Class::Public => assert_eq!(status, StatusCode::OK, "{surface}: {body}"),
                    Class::Refused => assert!(
                        [
                            StatusCode::UNAUTHORIZED,
                            StatusCode::FORBIDDEN,
                            StatusCode::NOT_FOUND,
                            StatusCode::METHOD_NOT_ALLOWED,
                        ]
                        .contains(&status),
                        "{surface} is {status}"
                    ),
                    Class::Redirect => {
                        assert!(status.is_redirection(), "{surface} is {status}");
                    }
                    Class::Asset | Class::Stream | Class::Mcp => {}
                }
                assert_hidden(&body, &surface);
            }
        }
    }
}

#[tokio::test]
async fn the_public_sees_the_public_check_and_a_reader_sees_both() {
    let (board, _) = seeded().await;
    for uri in [
        "/api/v1/checks",
        "/api/v1/outages",
        "/api/v1/timeline",
        "/api/v1/components/api",
    ] {
        let (status, public) = read(&board, uri, None).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert!(
            public.contains(SHOWN),
            "{SHOWN} missing from {uri}: {public}"
        );
        if uri == "/api/v1/components/api" {
            continue;
        }
        let (status, full) = read(&board, uri, Some(READER)).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        for id in [SHOWN, "backup-disk", "nightly-dump"] {
            assert!(
                full.contains(id),
                "{id} missing from {uri} with status:read: {full}"
            );
        }
    }
}

#[tokio::test]
async fn the_internal_board_shows_both() {
    let (board, _) = seeded().await;
    let page = board.page("/internal/").await;
    for shown in [SHOWN, "Backup disk", "Nightly dump"] {
        assert!(page.contains(shown), "{shown} missing from the board");
    }
}

// --- The event streams ---------------------------------------------------

/// A change of `check_id`, on `component_id`, to down.
fn changed(check_id: &str, kind: MemberKind, component_id: Option<&str>) -> Event {
    EngineEvent::CheckChanged(CheckChange {
        check_id: check_id.into(),
        kind,
        component_id: component_id.map(Into::into),
        from: Some(Up),
        to: Down,
        since_ms: now().as_millis(),
        confirmed_at_ms: now().as_millis(),
        ended_outage_id: None,
        opened_outage_id: None,
        maintenance: false,
        alert: false,
    })
    .into()
}

/// What `request`'s event stream carries once the three checks change,
/// up to the change of `last`, which comes after the other two.
async fn stream(board: &Board, request: Request<Body>, last: &str) -> String {
    let (status, response) = send(&board.router, request).await;
    assert_eq!(status, StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    let changes = [
        ("backup-disk", MemberKind::Check, None),
        ("nightly-dump", MemberKind::Heartbeat, None),
        (SHOWN, MemberKind::Check, Some("api")),
    ];
    for (check_id, kind, component_id) in changes.iter().filter(|(id, ..)| *id != last) {
        board
            .state
            .events
            .emit(changed(check_id, *kind, *component_id));
    }
    let (check_id, kind, component_id) = changes.iter().find(|(id, ..)| *id == last).unwrap();
    board
        .state
        .events
        .emit(changed(check_id, *kind, *component_id));
    let mut seen = String::new();
    while !seen.contains(&format!("\"check_id\":\"{last}\"")) {
        let frame = tokio::time::timeout(Duration::from_secs(5), body.next())
            .await
            .unwrap_or_else(|_| panic!("no change of {last} within 5s: {seen}"))
            .expect("the stream is open")
            .unwrap();
        seen.push_str(&String::from_utf8_lossy(&frame));
    }
    seen
}

#[tokio::test]
async fn the_event_stream_leaves_out_internal_changes() {
    let (board, _) = seeded().await;
    for token in [None, Some(ONCALL)] {
        let mut request = from(OUTSIDER, Method::GET, "/api/v1/events");
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        // The public check's change comes last, so seeing it means the two
        // before it were left out, not still on their way.
        let seen = stream(&board, request.body(Body::empty()).unwrap(), SHOWN).await;
        assert_hidden(&seen, &format!("/api/v1/events (token {token:?})"));
    }
}

#[tokio::test]
async fn the_board_has_its_own_stream_with_every_change() {
    let (board, _) = seeded().await;
    let (status, _) = read(&board, "/internal/events", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let request = from(PROXY, Method::GET, "/internal/events")
        .header(EMAIL_HEADER, ALICE)
        .body(Body::empty())
        .unwrap();
    let seen = stream(&board, request, "nightly-dump").await;
    for id in [SHOWN, "backup-disk", "nightly-dump"] {
        assert!(
            seen.contains(id),
            "{id} missing from /internal/events: {seen}"
        );
    }
}

// --- MCP -----------------------------------------------------------------

/// `tool` called over `/mcp` with `token`: the JSON-RPC response's text.
async fn call(board: &Board, token: &str, tool: &str) -> String {
    let request = from(OUTSIDER, Method::POST, "/mcp")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-06-18")
        .body(Body::from(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": { "name": tool, "arguments": {} },
            })
            .to_string(),
        ))
        .unwrap();
    let (status, _, body) = parts(send(&board.router, request).await.1).await;
    assert_eq!(status, StatusCode::OK, "{tool}: {body}");
    assert!(!body.contains("\"isError\":true"), "{tool}: {body}");
    body
}

#[tokio::test]
async fn mcp_without_status_read_leaves_out_internal_checks() {
    let (board, _) = seeded().await;
    for tool in [
        "get_status",
        "list_outages",
        "get_timeline",
        "list_incidents",
        "list_releases",
    ] {
        let body = call(&board, ONCALL, tool).await;
        assert_hidden(&body, &format!("MCP {tool}"));
        if tool == "list_outages" || tool == "get_timeline" {
            assert!(
                body.contains(SHOWN),
                "{SHOWN} missing from MCP {tool}: {body}"
            );
            let full = call(&board, READER, tool).await;
            assert!(
                full.contains("backup-disk") && full.contains("nightly-dump"),
                "MCP {tool} with status:read: {full}"
            );
        }
    }
}
