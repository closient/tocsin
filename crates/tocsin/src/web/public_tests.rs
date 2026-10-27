// SPDX-License-Identifier: AGPL-3.0-only

//! Checks on no component stay off every public surface (TOC-69), and so
//! does a component that is not public, with its group, its checks and
//! their outages (TOC-92), and so do draft incidents (TOC-93) and every
//! incident's internal note and external key (TOC-94).
//!
//! The routes come from the router itself, not from a list kept by hand:
//! a route added without a line in [`ROUTES`] fails
//! [`every_route_is_classified`]. Each public route is then read with no
//! token and with a token lacking `status:read`, and must name none of the
//! internal check, the internal heartbeat, the component that is not
//! public, its group and its checks, and the draft; the event stream and
//! the MCP tools are read the same way. The internal board and a
//! `status:read` token are the controls: they see them all, so an absence
//! on a public surface is a filter at work and not a missing seed.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use futures_util::StreamExt;
use regex::Regex;
use serde_json::json;
use tocsin_core::Health::{Down, Up};
use tocsin_core::incident::{Actor, Impact, IncidentKind, NewIncident, Timestamp};
use tocsin_core::release::NewRelease;
use tocsin_core::state::ComponentStatus;
use tocsin_engine::{CheckChange, ComponentChange, ComponentSnapshot, EngineEvent, MemberKind};
use tocsin_store::{NewOutage, SubjectKind};

use super::assets;
use super::identity::AdminAuth;
use super::internal_tests::{
    Board, EMAIL_HEADER, ONCALL, READER, TRUSTED_HEADER, config, parts, serve,
};
use super::status_tests::PUBLIC_PROJECT;
use super::tests::{ALICE, OUTSIDER, PROXY, from, send};
use crate::http::auth::IncidentWriter;
use crate::http::events::Event;
use crate::http::{self, OPENAPI_PATH, incidents, now, releases};

/// What names the internal check and heartbeat, the component that is
/// not public and the draft incident: their ids, names and the checks'
/// targets, the component's group, a removed check whose outage recorded
/// the component, the draft's title and message, and the internal note and
/// external key of both the draft and the published incident.
const HIDDEN: [&str; 18] = [
    "backup-disk",
    "Backup disk",
    "nightly-dump",
    "Nightly dump",
    "backup.internal.example.com",
    "back-office",
    "Back office",
    "vault-hosts",
    "Vault hosts",
    "vault-ping",
    "vault.internal.example.com",
    "vault-retired",
    DRAFT,
    "Not public yet.",
    PUBLISHED_NOTE,
    PUBLISHED_KEY,
    DRAFT_NOTE,
    DRAFT_KEY,
];

/// The published incident's internal note and external key.
const PUBLISHED_NOTE: &str = "Evidence: db-07 replication lag";
const PUBLISHED_KEY: &str = "sweep-cluster-published";
/// The draft's internal note and external key.
const DRAFT_NOTE: &str = "Evidence: db-07 disk at 97 percent";
const DRAFT_KEY: &str = "sweep-cluster-draft";

/// The seeded draft incident's title.
const DRAFT: &str = "Drafted trouble";

/// A group whose one component is not public, with a check on it.
const NOT_PUBLIC: &str = r#"
[[group]]
id = "back-office"
name = "Back office"

[[component]]
id = "vault-hosts"
name = "Vault hosts"
group = "back-office"
public = false

[[check]]
id = "vault-ping"
component = "vault-hosts"
type = "tcp"
target = "vault.internal.example.com:22"
interval = "1m"
"#;

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
    // The draft's own page and record are not found.
    (
        "/incidents/{id}",
        Class::Refused,
        &["/incidents/{draft}", "/api/v1/incidents/{draft}"],
    ),
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
    // Finding an incident by its external key needs `status:read`.
    (
        "/api/v1/incidents",
        Class::Refused,
        &[
            "/api/v1/incidents?external_key=sweep-cluster-draft",
            "/api/v1/incidents?external_key=sweep-cluster-published",
        ],
    ),
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
    // The alert history names internal checks, notifiers and labels.
    (
        "/api/v1/alerts/history",
        Class::Refused,
        &["/api/v1/alerts/history"],
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
        "/api/v1/incidents/{id}/publish",
        Class::Refused,
        &["/api/v1/incidents/{draft}/publish"],
    ),
    (
        "/api/v1/releases/{project}/{version}/deployed",
        Class::Refused,
        &["/api/v1/releases/app/1.0.0/deployed"],
    ),
    // Subscribers: 404 without `[subscribers]`, and the list and removal
    // need `subscribers:admin`.
    (
        "/api/v1/subscribers",
        Class::Refused,
        &["/api/v1/subscribers"],
    ),
    (
        "/api/v1/subscribers/{id}",
        Class::Refused,
        &["/api/v1/subscribers/1"],
    ),
    // Silences: every route, reads included, needs the `silence` scope.
    ("/api/v1/silences", Class::Refused, &["/api/v1/silences"]),
    (
        "/api/v1/silences/{id}",
        Class::Refused,
        &["/api/v1/silences/1"],
    ),
    (
        "/api/v1/silences/{id}/expire",
        Class::Refused,
        &["/api/v1/silences/1/expire"],
    ),
    // Federation: a push must be signed by a configured node, and the
    // node list needs `status:read`.
    (
        "/api/v1/federation/batch",
        Class::Refused,
        &["/api/v1/federation/batch"],
    ),
    (
        "/api/v1/federation/nodes",
        Class::Refused,
        &["/api/v1/federation/nodes"],
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
    (
        "/admin/incidents/{id}/publish",
        Class::Redirect,
        &["/admin/incidents/{draft}/publish"],
    ),
    (
        "/admin/incidents/{id}/discard",
        Class::Redirect,
        &["/admin/incidents/{draft}/discard"],
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

/// A board behind a trusted proxy with a public project, a component that
/// is not public and a `status:read` token, seeded with results, outages
/// of every check and of a removed one, an incident, a draft incident on a
/// public component and a deployed release. Returns the published
/// incident's id.
async fn seeded() -> (Board, String) {
    let config = config(&format!(
        "{TRUSTED_HEADER}{PUBLIC_PROJECT}{READER_TOKEN}{NOT_PUBLIC}"
    ));
    let admin = AdminAuth::from_config(&config).unwrap();
    let board = serve(config, admin);
    board.furnish().await;
    let started = now().as_millis() - 3_600_000;
    for (check_id, subject_kind, component_id) in [
        (SHOWN, SubjectKind::Check, Some("api")),
        ("backup-disk", SubjectKind::Check, None),
        ("nightly-dump", SubjectKind::Heartbeat, None),
        ("vault-ping", SubjectKind::Check, Some("vault-hosts")),
        ("vault-retired", SubjectKind::Check, Some("vault-hosts")),
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
            notify: true,
            draft: None,
            internal_note: Some(PUBLISHED_NOTE.to_owned()),
            external_key: Some(PUBLISHED_KEY.to_owned()),
        },
        &IncidentWriter::publishing(Actor::person(ALICE)),
    )
    .await
    .unwrap()
    .0;
    incidents::open(
        &board.state,
        NewIncident {
            kind: IncidentKind::Incident,
            title: DRAFT.to_owned(),
            impact: Impact::Major,
            components: vec!["api".to_owned()],
            status: None,
            message: "Not public yet.".to_owned(),
            scheduled_start: None,
            scheduled_end: None,
            notify: true,
            draft: Some(true),
            internal_note: Some(DRAFT_NOTE.to_owned()),
            external_key: Some(DRAFT_KEY.to_owned()),
        },
        &IncidentWriter::publishing(Actor::person(ALICE)),
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
    classified.dedup();
    assert_eq!(
        served, classified,
        "a route was added or removed: classify it in `ROUTES` (left: served, right: classified)"
    );
}

#[tokio::test]
async fn no_anonymous_route_names_an_internal_check() {
    let (board, incident) = seeded().await;
    let draft = draft_id(&board).await;
    for &(pattern, class, uris) in ROUTES {
        if matches!(class, Class::Stream | Class::Mcp) {
            continue;
        }
        for uri in uris {
            let uri = uri
                .replace("{incident}", &incident)
                .replace("{draft}", &draft)
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
        let removed: &[&str] = if uri == "/api/v1/checks" {
            &[]
        } else {
            &["vault-retired"]
        };
        for id in [SHOWN, "backup-disk", "nightly-dump", "vault-ping"]
            .iter()
            .chain(removed)
        {
            assert!(
                full.contains(id),
                "{id} missing from {uri} with status:read: {full}"
            );
        }
    }
}

#[tokio::test]
async fn a_reader_sees_the_draft() {
    let (board, _) = seeded().await;
    let draft = draft_id(&board).await;
    for uri in [
        "/api/v1/incidents".to_owned(),
        format!("/api/v1/incidents/{draft}"),
        "/api/v1/timeline".to_owned(),
    ] {
        let (status, full) = read(&board, &uri, Some(READER)).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert!(full.contains(DRAFT), "{DRAFT} missing from {uri}: {full}");
    }
}

/// The controls for the internals: a `status:read` token and the admin
/// page read them, so their absence elsewhere is the filter at work.
#[tokio::test]
async fn a_reader_and_the_admin_see_the_internals() {
    let (board, incident) = seeded().await;
    let draft = draft_id(&board).await;
    let (status, full) = read(&board, "/api/v1/incidents", Some(READER)).await;
    assert_eq!(status, StatusCode::OK);
    for shown in [PUBLISHED_NOTE, PUBLISHED_KEY, DRAFT_NOTE, DRAFT_KEY] {
        assert!(
            full.contains(shown),
            "{shown} missing from the list: {full}"
        );
    }
    for (id, note, key) in [
        (&incident, PUBLISHED_NOTE, PUBLISHED_KEY),
        (&draft, DRAFT_NOTE, DRAFT_KEY),
    ] {
        let (status, one) = read(&board, &format!("/api/v1/incidents/{id}"), Some(READER)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(one.contains(note) && one.contains(key), "{one}");
        let (status, keyed) = read(
            &board,
            &format!("/api/v1/incidents?external_key={key}"),
            Some(READER),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(keyed.contains(note), "{keyed}");
        let page = board.page(&format!("/admin/incidents/{id}")).await;
        assert!(page.contains(note) && page.contains(key), "{page}");
    }
}

#[tokio::test]
async fn the_internal_board_shows_them_all() {
    let (board, _) = seeded().await;
    let page = board.page("/internal/").await;
    for shown in [
        SHOWN,
        "Backup disk",
        "Nightly dump",
        "Back office",
        "Vault hosts",
        "vault-ping",
        DRAFT,
    ] {
        assert!(page.contains(shown), "{shown} missing from the board");
    }
}

/// Every route naming the component that is not public answers as it would
/// for one never configured, word for word but the id; a `status:read`
/// token reads it like any other.
#[tokio::test]
async fn a_component_that_is_not_public_is_unknown_to_the_public() {
    let (board, _) = seeded().await;
    for uri in [
        "/components/{id}",
        "/api/v1/components/{id}",
        "/feeds/components/{id}.atom",
        "/api/v1/timeline?component={id}",
    ] {
        for token in [None, Some(ONCALL)] {
            let (unknown_status, unknown) = read(&board, &uri.replace("{id}", "nope"), token).await;
            let (status, body) = read(&board, &uri.replace("{id}", "vault-hosts"), token).await;
            assert!(
                unknown_status.is_client_error(),
                "{uri} is {unknown_status}"
            );
            assert_eq!(status, unknown_status, "{uri} (token {token:?}): {body}");
            assert_eq!(
                body.replace("vault-hosts", "nope"),
                unknown,
                "{uri} (token {token:?})"
            );
        }
    }
    for uri in [
        "/api/v1/components/vault-hosts",
        "/api/v1/timeline?component=vault-hosts",
        "/api/v1/status.json",
    ] {
        let (status, body) = read(&board, uri, Some(READER)).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        assert!(body.contains("vault-hosts"), "{uri}: {body}");
    }
}

/// The overall status is the worst of the components a viewer sees, so a
/// component that is not public cannot leak through it while it is down.
#[tokio::test]
async fn the_overall_status_ignores_a_component_that_is_not_public() {
    let (mut board, _) = seeded().await;
    let mut snapshot = board.state.live.snapshot();
    snapshot.components.push(ComponentSnapshot {
        id: "vault-hosts".into(),
        health: Some(Down),
        status: ComponentStatus::Down,
    });
    board.state.live = Arc::new(snapshot);
    board.router = http::router(board.state.clone());
    for (token, expected) in [
        (None, "unknown"),
        (Some(ONCALL), "unknown"),
        (Some(READER), "down"),
    ] {
        let (status, body) = read(&board, "/api/v1/status.json", token).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let document: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(document["status"], expected, "token {token:?}: {body}");
    }
}

/// An incident or maintenance is public, so it may not name a component
/// that is not.
#[tokio::test]
async fn an_incident_may_not_name_a_component_that_is_not_public() {
    let (board, _) = seeded().await;
    let request = from(OUTSIDER, Method::POST, "/api/v1/incidents")
        .header(header::AUTHORIZATION, format!("Bearer {ONCALL}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({
                "title": "Vault down",
                "impact": "major",
                "components": ["api", "vault-hosts"],
                "message": "Looking into it.",
            })
            .to_string(),
        ))
        .unwrap();
    let (status, _, body) = parts(send(&board.router, request).await.1).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body.contains("components[1]") && body.contains("is not public"),
        "{body}"
    );
}

/// The seeded draft's id.
async fn draft_id(board: &Board) -> String {
    let drafts = board.state.store.draft_incidents().await.unwrap();
    assert_eq!(drafts.len(), 1, "{drafts:?}");
    drafts[0].id.to_string()
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

/// What `request`'s event stream carries once the component that is not
/// public and then every check change, up to the change of `last`, which
/// comes after the others.
async fn stream(board: &Board, request: Request<Body>, last: &str) -> String {
    let (status, response) = send(&board.router, request).await;
    assert_eq!(status, StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    board.state.events.emit(
        EngineEvent::ComponentChanged(ComponentChange {
            component_id: "vault-hosts".into(),
            from: ComponentStatus::Operational,
            to: ComponentStatus::Down,
            health: Some(Down),
            at_ms: now().as_millis(),
        })
        .into(),
    );
    let changes = [
        ("backup-disk", MemberKind::Check, None),
        ("nightly-dump", MemberKind::Heartbeat, None),
        ("vault-ping", MemberKind::Check, Some("vault-hosts")),
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

/// A note and key written while a `status:read` reader listens: the
/// events name the incident but never its internals, even to a reader
/// who may fetch them from the API.
#[tokio::test]
async fn the_event_stream_never_carries_the_internals() {
    const NOTE: &str = "Evidence: production-01 swap at 100 percent";
    const KEY: &str = "sweep-cluster-live";
    let (board, incident) = seeded().await;
    let request = from(OUTSIDER, Method::GET, "/api/v1/events")
        .header(header::AUTHORIZATION, format!("Bearer {READER}"))
        .body(Body::empty())
        .unwrap();
    let (status, response) = send(&board.router, request).await;
    assert_eq!(status, StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    incidents::open(
        &board.state,
        NewIncident {
            kind: IncidentKind::Incident,
            title: "Checkout errors".to_owned(),
            impact: Impact::Major,
            components: vec!["api".to_owned()],
            status: None,
            message: "Looking into it.".to_owned(),
            scheduled_start: None,
            scheduled_end: None,
            notify: false,
            draft: None,
            internal_note: Some(NOTE.to_owned()),
            external_key: Some(KEY.to_owned()),
        },
        &IncidentWriter::publishing(Actor::person(ALICE)),
    )
    .await
    .unwrap();
    let edit = from(
        OUTSIDER,
        Method::PATCH,
        &format!("/api/v1/incidents/{incident}"),
    )
    .header(header::AUTHORIZATION, format!("Bearer {ONCALL}"))
    .header(header::CONTENT_TYPE, "application/json")
    .body(Body::from(json!({ "internal_note": NOTE }).to_string()))
    .unwrap();
    assert_eq!(send(&board.router, edit).await.0, StatusCode::OK);
    let mut seen = String::new();
    while !(seen.contains("Checkout errors") && seen.contains("incident_edited")) {
        let frame = tokio::time::timeout(Duration::from_secs(5), body.next())
            .await
            .unwrap_or_else(|_| panic!("no incident events within 5s: {seen}"))
            .expect("the stream is open")
            .unwrap();
        seen.push_str(&String::from_utf8_lossy(&frame));
    }
    for hidden in [NOTE, KEY, "production-01", "internal_note", "external_key"] {
        assert!(
            !seen.contains(hidden),
            "{hidden} on the event stream: {seen}"
        );
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
    for id in [
        SHOWN,
        "backup-disk",
        "vault-ping",
        "nightly-dump",
        "\"type\":\"component_changed\",\"component_id\":\"vault-hosts\"",
    ] {
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
                full.contains("backup-disk")
                    && full.contains("nightly-dump")
                    && full.contains("vault-ping"),
                "MCP {tool} with status:read: {full}"
            );
        }
    }
}
