// SPDX-License-Identifier: AGPL-3.0-only

//! The read API in process: the status document joined with a live
//! snapshot, what a `status:read` token adds, history validation and
//! paging, unknown ids, and `/llms.txt`.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::{Value, json};
use tocsin_core::Health::{Degraded, Down, Up};
use tocsin_core::config::{Environment, Source};
use tocsin_core::incident::Timestamp;
use tocsin_core::state::{ComponentStatus, Confirmed};
use tocsin_engine::{CheckSnapshot, ComponentSnapshot, LastResult, Snapshot};
use tocsin_store::{NewCheckResult, NewOutage, Store};
use tower::ServiceExt;

use super::events::EventBus;
use super::timeline::DAY_MS;
use super::{AppState, now, router};

const CONFIG: &str = r#"
[server]
public_url = "https://status.example.com/"

[page]
title = "Example status"

[[group]]
id = "core"
name = "Core"

[[component]]
id = "api"
name = "API"
description = "The public API"
group = "core"

[[component]]
id = "jobs"
name = "Jobs"

[[check]]
id = "api-https"
name = "API over HTTPS"
component = "api"
type = "http"
target = "https://api.internal.example.com/health"
interval = "30s"

[[check]]
id = "db"
component = "api"
type = "tcp"
target = "db.internal.example.com:5432"
interval = "1h30m"

[[heartbeat]]
id = "backup"
name = "Nightly backup"
component = "jobs"
interval = "1d"

[[project]]
id = "secret"
name = "Secret"
public = false
components = ["api"]

[[token]]
id = "oncall"
secret = "${ENV:ONCALL_TOKEN}"
scopes = ["incident"]

[[token]]
id = "reader"
secret = "${ENV:READER_TOKEN}"
scopes = ["status:read"]
"#;

const ONCALL: &str = "oncall-secret-0123456789";
const READER: &str = "reader-secret-0123456789";

/// When the snapshot says `api-https` went down, and its last result.
const DOWN_SINCE: i64 = 1_760_000_000_000;
const LAST_RESULT: i64 = 1_760_000_060_000;

/// The engine's view: the API down on its HTTPS check, `db` not yet
/// confirmed, no state for `jobs` or the heartbeat.
fn snapshot() -> Snapshot {
    Snapshot {
        checks: vec![
            CheckSnapshot {
                id: "api-https".into(),
                component_id: Some("api".into()),
                state: Some(Confirmed {
                    health: Down,
                    since_ms: DOWN_SINCE,
                }),
                last: Some(LastResult {
                    health: Degraded,
                    started_at_ms: LAST_RESULT,
                }),
            },
            CheckSnapshot {
                id: "db".into(),
                component_id: Some("api".into()),
                state: None,
                last: None,
            },
        ],
        components: vec![ComponentSnapshot {
            id: "api".into(),
            health: Some(Down),
            status: ComponentStatus::Down,
        }],
    }
}

struct Api {
    _dir: tempfile::TempDir,
    state: AppState,
    router: Router,
}

fn api() -> Api {
    let variable = |name: &str| match name {
        "ONCALL_TOKEN" => Some(ONCALL.to_owned()),
        "READER_TOKEN" => Some(READER.to_owned()),
        _ => None,
    };
    let config = tocsin_core::config::load(
        &[Source {
            name: "tocsin.toml".into(),
            text: Ok(CONFIG.into()),
        }],
        &Environment {
            hostname: "test".into(),
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
    };
    Api {
        _dir: dir,
        router: router(state.clone()),
        state,
    }
}

impl Api {
    async fn raw(
        &self,
        method: Method,
        uri: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, String, Option<String>) {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let request = match body {
            Some(body) => request
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string())),
            None => request.body(Body::empty()),
        }
        .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .map(|value| value.to_str().unwrap().to_owned());
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            String::from_utf8(bytes.to_vec()).unwrap(),
            content_type,
        )
    }

    async fn get(&self, uri: &str, token: Option<&str>) -> (StatusCode, Value) {
        let (status, body, _) = self.raw(Method::GET, uri, token, None).await;
        (status, serde_json::from_str(&body).unwrap())
    }

    async fn post(&self, uri: &str, body: Value) -> Value {
        let (status, body, _) = self.raw(Method::POST, uri, Some(ONCALL), Some(body)).await;
        assert!(status.is_success(), "{status}: {body}");
        serde_json::from_str(&body).unwrap()
    }

    /// Appends a result for `check` `minutes` ago, returning its id.
    async fn result(&self, check: &str, minutes: i64, health: tocsin_core::Health) -> i64 {
        self.state
            .store
            .append(NewCheckResult {
                node_id: "node-a".into(),
                check_id: check.into(),
                started_at_ms: now().as_millis() - minutes * 60_000,
                duration_ms: 40,
                health,
                detail: None,
                error: (health != Up).then(|| "timed out".to_owned()),
                status_code: None,
            })
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn the_status_document_joins_the_configuration_with_the_live_state() {
    let api = api();
    let (status, document) = api.get("/api/v1/status.json", None).await;
    assert_eq!(status, StatusCode::OK, "{document}");
    assert_eq!(document["title"], "Example status");
    assert_eq!(document["status"], "down", "the worst component wins");
    assert_eq!(
        document["components"],
        json!([
            {
                "id": "api", "name": "API", "description": "The public API",
                "group": "core", "status": "down", "health": "down",
            },
            {
                "id": "jobs", "name": "Jobs", "description": null,
                "group": null, "status": "unknown", "health": null,
            },
        ])
    );
    assert_eq!(document["incidents"], json!([]));
    let generated = document["generated_at"].as_str().unwrap();
    let generated: Timestamp = serde_json::from_value(json!(generated)).unwrap();
    assert!((now().as_millis() - generated.as_millis()).abs() < 60_000);
}

#[tokio::test]
async fn open_incidents_show_until_resolved_and_maintenance_until_completed() {
    let api = api();
    let incident = api
        .post(
            "/api/v1/incidents",
            json!({"title": "API errors", "impact": "major", "components": ["api"], "message": "Looking."}),
        )
        .await;
    let start = Timestamp::from_millis(now().as_millis() + DAY_MS);
    let end = Timestamp::from_millis(now().as_millis() + DAY_MS + 3_600_000);
    let maintenance = api
        .post(
            "/api/v1/incidents",
            json!({
                "kind": "maintenance", "title": "Upgrade", "impact": "minor",
                "components": ["jobs"], "message": "Planned.",
                "scheduled_start": start, "scheduled_end": end,
            }),
        )
        .await;
    let titles = |document: &Value| -> Vec<String> {
        document["incidents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|incident| incident["title"].as_str().unwrap().to_owned())
            .collect()
    };
    let (_, document) = api.get("/api/v1/status.json", None).await;
    assert_eq!(titles(&document), ["Upgrade", "API errors"], "newest first");
    let (_, detail) = api.get("/api/v1/components/api", None).await;
    assert_eq!(titles(&detail), ["API errors"], "only those naming it");

    api.post(
        &format!("/api/v1/incidents/{}/updates", incident["id"]),
        json!({"status": "resolved", "message": "Fixed."}),
    )
    .await;
    api.post(
        &format!("/api/v1/incidents/{}/updates", maintenance["id"]),
        json!({"status": "completed", "message": "Done."}),
    )
    .await;
    let (_, document) = api.get("/api/v1/status.json", None).await;
    assert_eq!(titles(&document), Vec::<String>::new());
}

#[tokio::test]
async fn a_component_shows_its_checks_and_an_unknown_one_is_404() {
    let api = api();
    let (status, detail) = api.get("/api/v1/components/api", None).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["component"]["status"], "down");
    let ids: Vec<&str> = detail["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|check| check["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["api-https", "db"]);
    let (_, jobs) = api.get("/api/v1/components/jobs", None).await;
    assert_eq!(jobs["checks"][0]["id"], "backup");
    assert_eq!(jobs["checks"][0]["kind"], "heartbeat");

    let (status, error) = api.get("/api/v1/components/nope", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{error}");
    assert_eq!(error["error"], "not_found");
    assert_eq!(error["message"], "no component nope");
}

#[tokio::test]
async fn checks_carry_their_live_state_and_only_a_reader_sees_targets() {
    let api = api();
    let (status, public) = api.get("/api/v1/checks", None).await;
    assert_eq!(status, StatusCode::OK, "{public}");
    assert_eq!(
        public["checks"],
        json!([
            {
                "id": "api-https", "kind": "check", "name": "API over HTTPS",
                "component": "api", "type": "http", "interval": "30s",
                "health": "down",
                "since": Timestamp::from_millis(DOWN_SINCE),
                "last_health": "degraded",
                "last_result_at": Timestamp::from_millis(LAST_RESULT),
            },
            {
                "id": "db", "kind": "check", "name": "db", "component": "api",
                "type": "tcp", "interval": "1h30m", "health": null, "since": null,
                "last_health": null, "last_result_at": null,
            },
            {
                "id": "backup", "kind": "heartbeat", "name": "Nightly backup",
                "component": "jobs", "type": null, "interval": "1d", "health": null,
                "since": null, "last_health": null, "last_result_at": null,
            },
        ])
    );
    let text = public.to_string();
    assert!(!text.contains("internal"), "a target leaked: {text}");

    let (_, trusted) = api.get("/api/v1/checks", Some(READER)).await;
    assert_eq!(
        trusted["checks"][0]["target"],
        "https://api.internal.example.com/health"
    );
    assert_eq!(
        trusted["checks"][1]["target"],
        "db.internal.example.com:5432"
    );
    assert!(
        trusted["checks"][2].get("target").is_none(),
        "a heartbeat has none"
    );
    let (_, detail) = api.get("/api/v1/components/api", Some(READER)).await;
    assert_eq!(
        detail["checks"][1]["target"],
        "db.internal.example.com:5432"
    );
    let (_, detail) = api.get("/api/v1/components/api", None).await;
    assert!(!detail.to_string().contains("internal"), "{detail}");
}

#[tokio::test]
async fn a_token_on_a_public_read_must_be_a_reader() {
    let api = api();
    for uri in [
        "/api/v1/checks",
        "/api/v1/components/api",
        "/api/v1/checks/api-https/results",
    ] {
        let (status, error) = api.get(uri, Some("not-a-token-0123456789")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}: {error}");
        let (status, error) = api.get(uri, Some(ONCALL)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {error}");
        assert!(
            error["message"].as_str().unwrap().contains("status:read"),
            "{uri}: {error}"
        );
    }
}

#[tokio::test]
async fn raw_results_need_a_reader_token() {
    let api = api();
    api.result("api-https", 5, Down).await;
    let (status, error) = api.get("/api/v1/checks/api-https/results", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{error}");
    assert_eq!(error["error"], "unauthorized");
    let (status, page) = api
        .get("/api/v1/checks/api-https/results", Some(READER))
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["check_id"], "api-https");
    assert_eq!(page["results"][0]["health"], "down");
    assert_eq!(page["results"][0]["error"], "timed out");
}

#[tokio::test]
async fn results_page_newest_first_within_since() {
    let api = api();
    let old = api.result("api-https", 3 * 24 * 60, Up).await;
    let a = api.result("api-https", 30, Up).await;
    let b = api.result("api-https", 20, Degraded).await;
    let c = api.result("api-https", 10, Down).await;
    api.result("db", 15, Down).await;

    let ids = |page: &Value| -> Vec<i64> {
        page["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|result| result["id"].as_i64().unwrap())
            .collect()
    };
    let (_, page) = api
        .get("/api/v1/checks/api-https/results?limit=2", Some(READER))
        .await;
    assert_eq!(ids(&page), [c, b], "the default window is one day");
    assert_eq!(page["next_before"], b);
    let since = page["since"].as_str().unwrap().to_owned();
    let (_, next) = api
        .get(
            &format!("/api/v1/checks/api-https/results?limit=2&before={b}&since={since}"),
            Some(READER),
        )
        .await;
    assert_eq!(ids(&next), [a]);
    assert_eq!(
        next["next_before"],
        Value::Null,
        "the last page has no cursor"
    );

    let (_, exact) = api
        .get("/api/v1/checks/api-https/results?limit=3", Some(READER))
        .await;
    assert_eq!(ids(&exact), [c, b, a]);
    assert_eq!(
        exact["next_before"],
        Value::Null,
        "an exactly full last page"
    );

    let since = Timestamp::from_millis(now().as_millis() - 4 * DAY_MS);
    let (_, wide) = api
        .get(
            &format!("/api/v1/checks/api-https/results?since={since}"),
            Some(READER),
        )
        .await;
    assert_eq!(ids(&wide), [c, b, a, old]);
}

#[tokio::test]
async fn history_validates_since_and_limit() {
    let api = api();
    let too_old = Timestamp::from_millis(now().as_millis() - 367 * DAY_MS);
    for base in ["/api/v1/checks/api-https/results?", "/api/v1/outages?"] {
        for (query, path) in [
            ("limit=0", "limit"),
            ("limit=501", "limit"),
            (&*format!("since={too_old}"), "since"),
            ("since=yesterday", ""),
            ("limit=x", ""),
            ("colour=red", ""),
        ] {
            let uri = format!("{base}{query}");
            let (status, error) = api.get(&uri, Some(READER)).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{uri}: {error}");
            assert_eq!(error["fields"][0]["path"], path, "{uri}: {error}");
        }
        let uri = format!("{base}limit=500");
        let (status, error) = api.get(&uri, Some(READER)).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {error}");
    }
}

#[tokio::test]
async fn results_for_an_unknown_check_are_404() {
    let api = api();
    let (status, error) = api.get("/api/v1/checks/nope/results", Some(READER)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{error}");
    assert_eq!(error["message"], "no check nope");
    let (status, page) = api.get("/api/v1/checks/backup/results", Some(READER)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a heartbeat has results too: {page}"
    );
}

#[tokio::test]
async fn outages_are_public_and_overlap_the_window() {
    let api = api();
    let at = |minutes: i64| now().as_millis() - minutes * 60_000;
    let store = &api.state.store;
    let outage = |check: &str, minutes: i64| NewOutage {
        node_id: "node-a".into(),
        check_id: check.into(),
        component_id: Some("api".into()),
        health: Down,
        started_at_ms: at(minutes),
        alerted: true,
    };
    let ancient = store.open_outage(outage("db", 40 * 24 * 60)).await.unwrap();
    store.end_outage(ancient, at(39 * 24 * 60)).await.unwrap();
    let ended = store.open_outage(outage("db", 60)).await.unwrap();
    store.end_outage(ended, at(50)).await.unwrap();
    let open = store.open_outage(outage("api-https", 10)).await.unwrap();

    let (status, list) = api.get("/api/v1/outages", None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let ids: Vec<i64> = list["outages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|outage| outage["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, [open, ended], "30 days by default, newest first");
    assert_eq!(list["outages"][0]["ended_at"], Value::Null);
    assert_eq!(list["outages"][0]["check_id"], "api-https");
    assert_eq!(list["next_before"], Value::Null);

    let (_, page) = api.get("/api/v1/outages?limit=1", None).await;
    assert_eq!(page["outages"][0]["id"], open);
    assert_eq!(page["next_before"], open);
}

#[tokio::test]
async fn llms_txt_is_the_status_in_plain_text_with_where_to_go_next() {
    let api = api();
    api.post(
        "/api/v1/incidents",
        json!({"title": "API errors", "impact": "major", "components": ["api"], "message": "Looking."}),
    )
    .await;
    let (status, text, content_type) = api.raw(Method::GET, "/llms.txt", None, None).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(content_type.as_deref(), Some("text/plain; charset=utf-8"));
    assert!(text.starts_with("# Example status\n\n> "), "{text}");
    for line in [
        "Overall status: down as of ",
        "- [API](https://status.example.com/api/v1/components/api): down",
        "- [Jobs](https://status.example.com/api/v1/components/jobs): unknown",
        "- [API errors](https://status.example.com/api/v1/incidents/1): incident investigating, impact major, affecting api",
        "(https://status.example.com/api/v1/openapi.json)",
        "(https://status.example.com/api/v1/status.json)",
        "(https://github.com/closient/tocsin/blob/main/docs/api.md)",
        "(https://status.example.com/api/v1/outages?since=",
        "/api/v1/checks/ID/results?since=",
        "`status:read`",
    ] {
        assert!(text.contains(line), "missing {line:?} in:\n{text}");
    }
    assert!(!text.contains("internal"), "a target leaked:\n{text}");
    assert!(
        !text.contains("Secret"),
        "a private project leaked:\n{text}"
    );
}
