// SPDX-License-Identifier: AGPL-3.0-only

//! The internal board: who gets in (nobody without proxy auth, nobody
//! with a forged, expired or disallowed identity), what it shows that `/`
//! does not, the headers that keep it out of caches and search engines,
//! the public surfaces it stays out of, and the structural audit.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::response::Response;
use serde_json::json;
use tocsin_core::Health::{Down, Up};
use tocsin_core::config::{Config, Environment, Source};
use tocsin_core::state::{ComponentStatus, Confirmed};
use tocsin_engine::{
    CheckSnapshot, ComponentSnapshot, HeartbeatHandle, LastResult, MemberKind, Snapshot,
};
use tocsin_store::{NewCheckResult, Store};

use super::access::TOKEN_HEADER;
use super::feeds::{INCIDENTS, RELEASES};
use super::identity::AdminAuth;
use super::status_tests::{CONFIG, audit};
use super::tests::{
    ALICE, AUDIENCE, HOST, KeyServer, OTHER_KEY, OUTSIDER, PROXY, SIGNING_KEY, TEAM, claims, epoch,
    from, jwk, rs256, send, sign, text,
};
use crate::http::events::EventBus;
use crate::http::{self, AppState, OPENAPI_PATH, now};

/// Beside [`CONFIG`]'s `api-https` on the API component: a TCP check and
/// a heartbeat on no component, each with a link.
pub(super) const INTERNAL: &str = r#"
[[check]]
id = "backup-disk"
name = "Backup disk"
type = "tcp"
target = "backup.internal.example.com:22"
interval = "60s"

[[check.link]]
name = "Backup runbook"
url = "https://wiki.example.com/runbooks/backup-disk"

[[heartbeat]]
id = "nightly-dump"
name = "Nightly dump"
interval = "24h"

[[heartbeat.link]]
name = "Dump logs"
url = "https://logs.example.com/nightly-dump"
"#;

/// `[server.auth]` in `trusted_header` mode, trusting 10.0.0.0/8 and
/// admitting only Alice.
pub(super) const TRUSTED_HEADER: &str = r#"
[server.auth]
mode = "trusted_header"
trusted_proxies = ["10.0.0.0/8"]
allowed_emails = ["alice@example.com"]
"#;

pub(super) const EMAIL_HEADER: &str = "x-forwarded-email";

/// The secret of [`CONFIG`]'s `oncall` token, which may write incidents
/// and reads only what the public does.
pub(super) const ONCALL: &str = "oncall-0123456789";

/// The secret of a `status:read` token, for a config that declares one
/// with `${ENV:READER_TOKEN}`.
pub(super) const READER: &str = "reader-0123456789";

/// Every route the board serves, for a check with no component.
const ROUTES: [&str; 4] = [
    "/internal/",
    "/internal/fragments/board",
    "/internal/checks/backup-disk",
    "/internal/checks/backup-disk/live",
];

/// A store, the state around it, and the router serving it.
pub(super) struct Board {
    _dir: tempfile::TempDir,
    pub(super) state: AppState,
    pub(super) router: axum::Router,
}

pub(super) fn config(auth: &str) -> Config {
    let variable = |name: &str| match name {
        "ONCALL_TOKEN" => Some(ONCALL.to_owned()),
        "READER_TOKEN" => Some(READER.to_owned()),
        _ => None,
    };
    tocsin_core::config::load(
        &[Source {
            name: "tocsin.toml".into(),
            text: Ok(format!("{CONFIG}{INTERNAL}{auth}")),
        }],
        &Environment {
            hostname: "test".into(),
            variable: &variable,
        },
    )
    .unwrap()
}

/// The engine's view: the API's check up for two hours, the backup disk
/// down for half an hour, the dump up but with no result since the engine
/// started, as after a restart.
fn snapshot() -> Snapshot {
    let now = now().as_millis();
    let member = |id: &str, kind, component: Option<&str>, health, since_ms| CheckSnapshot {
        id: id.into(),
        kind,
        component_id: component.map(Into::into),
        state: Some(Confirmed { health, since_ms }),
        last: Some(LastResult {
            health,
            started_at_ms: now - 60_000,
        }),
    };
    Snapshot {
        checks: vec![
            member(
                "api-https",
                MemberKind::Check,
                Some("api"),
                Up,
                now - 7_200_000,
            ),
            member(
                "backup-disk",
                MemberKind::Check,
                None,
                Down,
                now - 1_800_000,
            ),
            CheckSnapshot {
                last: None,
                ..member(
                    "nightly-dump",
                    MemberKind::Heartbeat,
                    None,
                    Up,
                    now - 3_600_000,
                )
            },
        ],
        components: vec![ComponentSnapshot {
            id: "api".into(),
            health: Some(Up),
            status: ComponentStatus::Operational,
        }],
    }
}

pub(super) fn serve(config: Config, admin: Option<AdminAuth>) -> Board {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join(tocsin_store::DB_FILE)).unwrap();
    let state = AppState {
        store: Arc::new(store),
        config: Arc::new(config),
        events: EventBus::new(),
        admin: admin.map(Arc::new),
        live: Arc::new(snapshot()),
        heartbeats: HeartbeatHandle::default(),
        config_status: crate::reload::steady_status(),
        metrics: Arc::new(crate::metrics::Metrics::new()),
    };
    Board {
        _dir: dir,
        router: http::router(state.clone()),
        state,
    }
}

/// A board behind a trusted proxy that admits Alice.
pub(super) fn header_board() -> Board {
    let config = config(TRUSTED_HEADER);
    let admin = AdminAuth::from_config(&config).unwrap();
    serve(config, admin)
}

/// A board behind Cloudflare Access, whose keys `server` serves.
fn access_board(server: &KeyServer) -> Board {
    let config = config(&format!(
        "\n[server.auth]\nmode = \"cloudflare_access\"\nteam_domain = \"{TEAM}\"\naudience = \"{AUDIENCE}\"\n"
    ));
    let admin = AdminAuth::with_verifier(server.verifier(), &config);
    serve(config, Some(admin))
}

impl Board {
    /// `GET uri` as `email` through the trusted proxy.
    pub(super) async fn get_as(&self, email: &str, uri: &str) -> (StatusCode, HeaderMap, String) {
        let request = from(PROXY, Method::GET, uri)
            .header(EMAIL_HEADER, email)
            .body(Body::empty())
            .unwrap();
        parts(send(&self.router, request).await.1).await
    }

    /// The page at `uri` as Alice, which must be there.
    pub(super) async fn page(&self, uri: &str) -> String {
        let (status, _, body) = self.get_as(ALICE, uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        body
    }

    /// `GET uri` with no identity at all, as the public would.
    pub(super) async fn anonymous(&self, uri: &str) -> (StatusCode, String) {
        let request = from(OUTSIDER, Method::GET, uri)
            .body(Body::empty())
            .unwrap();
        let (status, _, body) = parts(send(&self.router, request).await.1).await;
        (status, body)
    }

    async fn result(&self, check: &str, ago: i64, health: tocsin_core::Health, more: Extra) {
        self.state
            .store
            .append(NewCheckResult {
                node_id: "node-a".into(),
                check_id: check.into(),
                started_at_ms: now().as_millis() - ago,
                duration_ms: more.duration_ms,
                health,
                detail: more.detail,
                error: more.error,
                status_code: None,
            })
            .await
            .unwrap();
    }

    /// Results for every check: the backup disk failing its connection
    /// five minutes ago after an hour of answering, the API passing its
    /// assertion, the dump's last ping with a message.
    pub(super) async fn furnish(&self) {
        for minutes in [65, 35, 10] {
            self.result("backup-disk", minutes * 60_000, Up, Extra::took(12))
                .await;
        }
        self.result(
            "backup-disk",
            300_000,
            Down,
            Extra {
                error: Some("connection refused".into()),
                ..Extra::took(3)
            },
        )
        .await;
        self.result(
            "api-https",
            60_000,
            Up,
            Extra {
                detail: Some(json!([{
                    "condition": "status_code",
                    "expected": 200,
                    "actual": 200,
                    "passed": true,
                    "on_fail": "down",
                    "implicit": false,
                }])),
                ..Extra::took(41)
            },
        )
        .await;
        self.result(
            "nightly-dump",
            3_600_000,
            Up,
            Extra {
                detail: Some(json!({"message": "dumped 42 tables"})),
                ..Extra::took(0)
            },
        )
        .await;
    }
}

/// The parts of a result [`Board::result`] varies.
struct Extra {
    duration_ms: i64,
    detail: Option<serde_json::Value>,
    error: Option<String>,
}

impl Extra {
    const fn took(duration_ms: i64) -> Self {
        Extra {
            duration_ms,
            detail: None,
            error: None,
        }
    }
}

pub(super) async fn parts(response: Response) -> (StatusCode, HeaderMap, String) {
    let status = response.status();
    let headers = response.headers().clone();
    (status, headers, text(response).await)
}

fn assert_private(headers: &HeaderMap, uri: &str) {
    assert_eq!(headers[header::CACHE_CONTROL], "private, no-store", "{uri}");
    assert_eq!(headers["x-robots-tag"], "noindex", "{uri}");
}

// --- Who gets in ---------------------------------------------------------

#[tokio::test]
async fn with_no_proxy_auth_configured_every_route_is_forbidden() {
    let board = serve(config(""), None);
    for uri in ROUTES {
        // Not even an email header from a would-be proxy gets in: with no
        // `[server.auth]` there is nobody to trust.
        let (status, headers, body) = board.get_as(ALICE, uri).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
        assert_private(&headers, uri);
        assert!(!body.contains("backup.internal.example.com"), "{uri}");
    }
    // Where `/admin/` is simply not there.
    assert_eq!(
        board.get_as(ALICE, "/admin/").await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn behind_a_trusted_proxy_only_an_allowed_email_from_the_proxy_gets_in() {
    let board = header_board();
    for uri in ROUTES {
        let (status, headers, _) = board.get_as(ALICE, uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_private(&headers, uri);

        // No email header.
        let request = from(PROXY, Method::GET, uri).body(Body::empty()).unwrap();
        let (status, headers, body) = parts(send(&board.router, request).await.1).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
        assert_private(&headers, uri);
        assert!(!body.contains("backup.internal.example.com"), "{uri}");

        // Someone not on `allowed_emails`.
        let (status, _, _) = board.get_as("mallory@example.com", uri).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");

        // Alice's email, sent straight to tocsin around the proxy.
        let request = from(OUTSIDER, Method::GET, uri)
            .header(EMAIL_HEADER, ALICE)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            send(&board.router, request).await.0,
            StatusCode::FORBIDDEN,
            "{uri}"
        );
    }
}

#[tokio::test]
async fn behind_access_a_forged_or_expired_token_is_forbidden() {
    let server = KeyServer::start(vec![jwk("k1", SIGNING_KEY)]).await;
    let board = access_board(&server);
    let get = |token: Option<&str>, uri: &str| {
        let mut request = Request::get(uri).header(header::HOST, HOST);
        if let Some(token) = token {
            request = request.header(TOKEN_HEADER, token);
        }
        request.body(Body::empty()).unwrap()
    };
    let valid = sign(SIGNING_KEY, &rs256("k1"), &claims(ALICE));
    // Signed by a key Access never published, under the published kid.
    let forged = sign(OTHER_KEY, &rs256("k1"), &claims(ALICE));
    // Expired well beyond the verifier's leeway.
    let mut stale = claims(ALICE);
    stale["exp"] = json!(epoch() - 600);
    let expired = sign(SIGNING_KEY, &rs256("k1"), &stale);

    for uri in ROUTES {
        let (status, response) = send(&board.router, get(Some(&valid), uri)).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert!(
            text(response).await.contains("backup.internal.example.com"),
            "{uri}"
        );

        for (what, token) in [
            ("no token", None),
            ("a forged token", Some(forged.as_str())),
            ("an expired token", Some(expired.as_str())),
        ] {
            let (status, response) = send(&board.router, get(token, uri)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri} with {what}");
            assert_private(response.headers(), uri);
            assert!(
                !text(response).await.contains("backup.internal.example.com"),
                "{uri} with {what}"
            );
        }
    }
}

#[tokio::test]
async fn an_unknown_check_is_not_found_once_signed_in() {
    let board = header_board();
    for uri in [
        "/internal/checks/nonexistent",
        "/internal/checks/nonexistent/live",
    ] {
        let (status, headers, _) = board.get_as(ALICE, uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert_private(&headers, uri);
        // Signed out, a probe for ids learns nothing.
        assert_eq!(board.anonymous(uri).await.0, StatusCode::FORBIDDEN, "{uri}");
    }
}

// --- What it shows -------------------------------------------------------

#[tokio::test]
async fn a_check_on_no_component_is_on_the_board_and_not_on_the_status_page() {
    let board = header_board();
    board.furnish().await;
    let page = board.page("/internal/").await;

    // Grouped like `/`: the API's check under Core, the rest after.
    let core = page.find(">Core</h2>").expect("the Core group");
    let api = page.find("/internal/checks/api-https").unwrap();
    let elsewhere = page.find(">Not on the status page</h2>").unwrap();
    let disk = page.find("/internal/checks/backup-disk").unwrap();
    let dump = page.find("/internal/checks/nightly-dump").unwrap();
    assert!(core < api && api < elsewhere && elsewhere < disk && disk < dump);
    assert!(page.contains("1 down"), "{page}");
    for shown in [
        "Backup disk",
        "TCP check, every 1m",
        "on no public component",
        "backup.internal.example.com:22",
        "Down for 30m",
        "connection refused",
        "Nightly dump",
        "dumped 42 tables",
        "Its assertion passed.",
        "Latency over the last 24 hours",
    ] {
        assert!(page.contains(shown), "{shown} missing from the board");
    }

    let (status, status_page) = board.anonymous("/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(status_page.contains("API"));
    for hidden in ["Backup disk", "backup-disk", "Nightly dump", "/internal"] {
        assert!(!status_page.contains(hidden), "{hidden} on /");
    }
}

#[tokio::test]
async fn links_from_config_show_only_on_the_board() {
    let board = header_board();
    let page = board.page("/internal/").await;
    let detail = board.page("/internal/checks/nightly-dump").await;
    for (name, url) in [
        (
            "Backup runbook",
            "https://wiki.example.com/runbooks/backup-disk",
        ),
        ("Dump logs", "https://logs.example.com/nightly-dump"),
    ] {
        assert!(page.contains(&format!("<a href=\"{url}\">{name}</a>")));
    }
    assert!(detail.contains("Dump logs") && !detail.contains("Backup runbook"));
}

#[tokio::test]
async fn a_heartbeat_shows_its_last_ping_deadline_and_message() {
    let board = header_board();
    board.furnish().await;
    let page = board.page("/internal/checks/nightly-dump").await;
    for shown in [
        "<dt>Last ping</dt><dd><time",
        // From the stored ping, as the engine resumes it, though the
        // snapshot has seen no result since the engine started.
        "<dt>Next deadline</dt><dd><time",
        "<dt>Last message</dt><dd>dumped 42 tables</dd>",
    ] {
        assert!(page.contains(shown), "{shown}");
    }
    // A heartbeat has no latency to draw.
    assert!(!page.contains("svg class=\"spark\""));
}

#[tokio::test]
async fn a_check_page_lists_its_results_and_assertions() {
    let board = header_board();
    board.furnish().await;
    let disk = board.page("/internal/checks/backup-disk").await;
    assert!(disk.contains("<h1>Backup disk</h1>"));
    assert_eq!(disk.matches("on node-a").count(), 4, "{disk}");
    assert!(disk.contains("connection refused"));
    assert!(disk.contains("The newest result records no assertions."));

    let api = board.page("/internal/checks/api-https").await;
    assert!(api.contains("<code>status_code</code>"));
    assert!(api.contains("expected <code>200</code>"));
    assert!(api.contains("Passed"));
}

#[tokio::test]
async fn the_board_reads_without_javascript_and_updates_over_the_event_stream() {
    let board = header_board();
    board.furnish().await;
    let page = board.page("/internal/").await;
    // Everything is in the HTML as served; the scripts only refresh it.
    assert!(page.contains("Backup disk") && page.contains("connection refused"));
    assert!(page.contains("sse-connect=\"/internal/events\""));
    assert!(page.contains("hx-get=\"/internal/fragments/board\""));
    assert!(page.contains("sse:check_changed"));
    let detail = board.page("/internal/checks/backup-disk").await;
    assert!(detail.contains("hx-get=\"/internal/checks/backup-disk/live\""));
}

// --- What it stays out of ------------------------------------------------

#[tokio::test]
async fn the_board_stays_out_of_public_surfaces() {
    let board = header_board();
    board.furnish().await;
    for uri in [
        "/",
        "/llms.txt",
        OPENAPI_PATH,
        INCIDENTS,
        RELEASES,
        "/api/v1/checks",
    ] {
        let (status, body) = board.anonymous(uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        for hidden in [
            "/internal",
            "backup.internal.example.com",
            "https://wiki.example.com/runbooks/backup-disk",
            "https://logs.example.com/nightly-dump",
        ] {
            assert!(!body.contains(hidden), "{hidden} in {uri}");
        }
    }
    // There is no sitemap to leak into.
    assert_eq!(
        board.anonymous("/sitemap.xml").await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn the_board_and_a_check_page_pass_the_structural_audit() {
    let board = header_board();
    board.furnish().await;
    for uri in [
        "/internal/",
        "/internal/checks/backup-disk",
        "/internal/checks/nightly-dump",
        "/internal/checks/api-https",
    ] {
        let page = board.page(uri).await;
        assert_eq!(audit(&page), Vec::<String>::new(), "{uri}");
        // The page is private: nothing in it for search engines.
        assert!(!page.contains("application/ld+json"), "{uri}");
    }
}
