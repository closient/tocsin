// SPDX-License-Identifier: AGPL-3.0-only

//! The silence API in process: the scope it needs, the expiry it demands
//! and caps, the origin it records from the token, episodes, and ending a
//! silence early.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::{Value, json};
use tocsin_core::Health;
use tocsin_core::config::{Config, Environment, Id, Source};
use tocsin_core::incident::Timestamp;
use tocsin_engine::HeartbeatHandle;
use tocsin_store::{NewOutage, Store, SubjectKind};
use tower::ServiceExt;

use super::events::EventBus;
use super::{AppState, now, router};

const CONFIG: &str = r#"
[server]
public_url = "https://status.example.com/"
max_silence = "2h"

[[component]]
id = "api"
name = "API"

[[check]]
id = "api-port"
component = "api"
type = "tcp"
target = "api.example.com:443"

[[heartbeat]]
id = "backup"
component = "api"
interval = "1h"

[[token]]
id = "pager"
secret = "${ENV:PAGER_TOKEN}"
scopes = ["silence"]

[[token]]
id = "reader"
secret = "${ENV:READER_TOKEN}"
scopes = ["status:read"]
"#;

const PAGER: &str = "pager-secret-0123456789";
const READER: &str = "reader-secret-0123456789";

const MINUTE_MS: i64 = 60_000;

struct Api {
    _dir: tempfile::TempDir,
    state: AppState,
    router: Router,
}

fn api() -> Api {
    api_with(|_| ())
}

/// [`api`], its configuration changed by `change` first.
fn api_with(change: impl FnOnce(&mut Config)) -> Api {
    let variable = |name: &str| match name {
        "PAGER_TOKEN" => Some(PAGER.to_owned()),
        "READER_TOKEN" => Some(READER.to_owned()),
        _ => None,
    };
    let mut config = tocsin_core::config::load(
        &[Source {
            name: "tocsin.toml".into(),
            text: Ok(CONFIG.into()),
        }],
        &Environment {
            hostname: "node-a".into(),
            node_id: None,
            variable: &variable,
        },
    )
    .unwrap();
    change(&mut config);
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join(tocsin_store::DB_FILE)).unwrap();
    let state = AppState {
        store: Arc::new(store),
        config: Arc::new(config),
        events: EventBus::new(),
        admin: None,
        live: Arc::new(tocsin_engine::Snapshot::default()),
        heartbeats: HeartbeatHandle::default(),
        liveness: crate::liveness::Pulse::default(),
        alert_wake: Arc::default(),
        config_status: crate::reload::steady_status(),
        metrics: Arc::new(crate::metrics::Metrics::new()),
        subscribers: None,
        slack: None,
    };
    Api {
        _dir: dir,
        router: router(state.clone()),
        state,
    }
}

impl Api {
    async fn send(
        &self,
        method: Method,
        uri: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, axum::http::HeaderMap, Value) {
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
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, headers, value)
    }

    async fn create(&self, body: Value) -> (StatusCode, Value) {
        let (status, _, value) = self
            .send(Method::POST, "/api/v1/silences", Some(PAGER), Some(body))
            .await;
        (status, value)
    }

    async fn open_outage(&self) -> i64 {
        self.state
            .store
            .open_outage(NewOutage {
                node_id: "node-a".into(),
                subject_kind: SubjectKind::Check,
                check_id: "api-port".into(),
                component_id: Some("api".into()),
                health: Health::Down,
                started_at_ms: now().as_millis() - 1_000,
                alerted: true,
            })
            .await
            .unwrap()
    }
}

/// An instant `minutes` from now, as the API takes it.
fn from_now(minutes: i64) -> String {
    Timestamp::from_millis(now().as_millis() + minutes * MINUTE_MS).to_string()
}

/// The paths of a 422's field errors.
fn paths(body: &Value) -> Vec<&str> {
    body["fields"]
        .as_array()
        .unwrap_or_else(|| panic!("no fields: {body}"))
        .iter()
        .map(|field| field["path"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn every_silence_route_needs_the_silence_scope() {
    let api = api();
    for (method, uri) in [
        (Method::GET, "/api/v1/silences"),
        (Method::GET, "/api/v1/silences/1"),
        (Method::POST, "/api/v1/silences/1/expire"),
    ] {
        let (status, _, _) = api.send(method.clone(), uri, None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri}");
        let (status, _, body) = api.send(method.clone(), uri, Some(READER), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}: {body}");
    }
    let body = json!({
        "matchers": {"subject": "api-port"},
        "expires_at": from_now(30),
        "reason": "deploying",
    });
    let (status, _, _) = api
        .send(Method::POST, "/api/v1/silences", Some(READER), Some(body))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_silence_records_the_token_that_made_it() {
    let api = api();
    let expires_at = from_now(30);
    let (status, headers, silence) = api
        .send(
            Method::POST,
            "/api/v1/silences",
            Some(PAGER),
            Some(json!({
                "matchers": {"subject": "api-port"},
                "expires_at": expires_at,
                "reason": "deploying the api",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{silence}");
    let id = silence["id"].as_i64().unwrap();
    assert_eq!(headers[header::LOCATION], format!("/api/v1/silences/{id}"));
    assert_eq!(
        silence["created_by"],
        json!({"kind": "token", "id": "pager"})
    );
    assert_eq!(silence["state"], "active");
    assert_eq!(silence["expires_at"], expires_at);
    assert_eq!(
        silence["matchers"],
        json!({"subject": "api-port"}),
        "matchers are kept as given"
    );

    let (status, _, read) = api
        .send(
            Method::GET,
            &format!("/api/v1/silences/{id}"),
            Some(PAGER),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(read, silence);
}

/// A node taken down on purpose: its liveness heartbeat, which no file
/// names, can still be silenced, and one this node keeps no watch for
/// cannot.
#[tokio::test]
async fn a_silence_can_name_a_liveness_heartbeat() {
    let api = api_with(|config| {
        let mut watch = config.heartbeats[0].clone();
        watch.id = Id::synthetic("node", &Id::parse("spoke").unwrap());
        config.heartbeats.push(watch);
    });
    let silence = |subject: &str| {
        json!({
            "matchers": {"subject": subject},
            "expires_at": from_now(30),
            "reason": "moving the spoke",
        })
    };
    let (status, body) = api.create(silence("node:spoke")).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = api.create(silence("node:elsewhere")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

#[tokio::test]
async fn the_client_cannot_say_who_made_a_silence() {
    let api = api();
    let (status, body) = api
        .create(json!({
            "matchers": {"subject": "api-port"},
            "expires_at": from_now(30),
            "reason": "deploying",
            "created_by": {"kind": "person", "id": "someone@example.com"},
        }))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body.to_string().contains("created_by"), "{body}");
}

#[tokio::test]
async fn a_silence_without_an_expiry_is_refused() {
    let api = api();
    let (status, body) = api
        .create(json!({
            "matchers": {"subject": "api-port"},
            "reason": "until I remember",
        }))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body.to_string().contains("expires_at"), "{body}");
    let (status, body) = api
        .create(json!({
            "matchers": {"subject": "api-port"},
            "expires_at": null,
            "reason": "until I remember",
        }))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

#[tokio::test]
async fn a_duration_is_counted_from_the_server_s_clock_and_capped_the_same() {
    let api = api();
    let before = now().as_millis();
    let (status, body) = api
        .create(json!({
            "matchers": {"subject": "api-port"},
            "expires_in": "30m",
            "reason": "deploying",
        }))
        .await;
    let after = now().as_millis();
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let expires_at: Timestamp = serde_json::from_value(body["expires_at"].clone()).unwrap();
    assert!(
        (before + 30 * MINUTE_MS..=after + 30 * MINUTE_MS).contains(&expires_at.as_millis()),
        "{body}"
    );

    let (status, body) = api
        .create(json!({
            "matchers": {"subject": "api-port"},
            "expires_in": "121m",
            "reason": "a long one",
        }))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(paths(&body), ["expires_in"], "{body}");
    assert!(body.to_string().contains("max_silence"), "{body}");

    let (status, body) = api
        .create(json!({
            "matchers": {"subject": "api-port"},
            "expires_in": "30m",
            "expires_at": from_now(30),
            "reason": "which one",
        }))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(paths(&body), ["expires_in"], "{body}");
}

#[tokio::test]
async fn an_expiry_is_capped_by_max_silence_and_must_be_ahead() {
    let api = api();
    let (status, body) = api
        .create(json!({
            "matchers": {"subject": "api-port"},
            "expires_at": from_now(121),
            "reason": "a long one",
        }))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(paths(&body), ["expires_at"], "{body}");
    assert!(body.to_string().contains("max_silence"), "{body}");

    let (status, body) = api
        .create(json!({
            "matchers": {"subject": "api-port"},
            "expires_at": from_now(-1),
            "reason": "already over",
        }))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(paths(&body), ["expires_at"], "{body}");

    let (status, body) = api
        .create(json!({
            "matchers": {"subject": "api-port"},
            "expires_at": from_now(119),
            "reason": "just inside the cap",
        }))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[tokio::test]
async fn matchers_must_name_what_is_configured() {
    let api = api();
    for (matchers, path) in [
        (json!({}), "matchers"),
        (json!({"subject": "nope"}), "matchers.subject"),
        (json!({"component": "nope"}), "matchers.component"),
        (json!({"notifier": "nope"}), "matchers.notifier"),
        (
            json!({"subject": "backup", "subject_kind": "check"}),
            "matchers.subject_kind",
        ),
    ] {
        let (status, body) = api
            .create(json!({
                "matchers": matchers,
                "expires_at": from_now(30),
                "reason": "deploying",
            }))
            .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{matchers}: {body}"
        );
        assert_eq!(paths(&body), [path], "{matchers}: {body}");
    }
    let (status, body) = api
        .create(json!({
            "matchers": {"subject": "backup"},
            "expires_at": from_now(30),
            "reason": "   ",
        }))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(paths(&body), ["reason"], "{body}");
}

#[tokio::test]
async fn an_episode_silence_is_scoped_to_its_open_outage() {
    let api = api();
    let outage = api.open_outage().await;
    let (status, silence) = api
        .create(json!({
            "episode": outage,
            "expires_at": from_now(60),
            "reason": "acknowledged",
        }))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{silence}");
    assert_eq!(silence["episode"], outage);
    assert_eq!(
        silence["matchers"],
        json!({"node": "node-a", "subject_kind": "check", "subject": "api-port"})
    );

    api.state
        .store
        .end_outage(outage, now().as_millis())
        .await
        .unwrap();
    let id = silence["id"].as_i64().unwrap();
    let (_, _, read) = api
        .send(
            Method::GET,
            &format!("/api/v1/silences/{id}"),
            Some(PAGER),
            None,
        )
        .await;
    assert_eq!(read["state"], "episode_ended", "{read}");

    let (status, body) = api
        .create(json!({
            "episode": outage,
            "expires_at": from_now(60),
            "reason": "too late",
        }))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(paths(&body), ["episode"], "{body}");
    let (status, body) = api
        .create(json!({
            "episode": outage + 100,
            "expires_at": from_now(60),
            "reason": "no such outage",
        }))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(paths(&body), ["episode"], "{body}");
}

#[tokio::test]
async fn a_silence_ends_early_once_and_is_listed_newest_first() {
    let api = api();
    let mut ids = Vec::new();
    for _ in 0..3 {
        let (_, silence) = api
            .create(json!({
                "matchers": {"component": "api"},
                "expires_at": from_now(30),
                "reason": "deploying",
            }))
            .await;
        ids.push(silence["id"].as_i64().unwrap());
    }
    let woken = || {
        let wake = Arc::clone(&api.state.alert_wake);
        async move {
            tokio::time::timeout(std::time::Duration::from_millis(50), wake.notified())
                .await
                .is_ok()
        }
    };
    assert!(!woken().await, "making a silence woke the alert worker");

    let (status, _, ended) = api
        .send(
            Method::POST,
            &format!("/api/v1/silences/{}/expire", ids[0]),
            Some(PAGER),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{ended}");
    assert_eq!(ended["state"], "ended");
    assert_eq!(ended["ended_by"], json!({"kind": "token", "id": "pager"}));
    // So an alert it kept quiet fires now, not at the worker's next tick.
    assert!(
        woken().await,
        "ending a silence did not wake the alert worker"
    );
    let (status, _, again) = api
        .send(
            Method::POST,
            &format!("/api/v1/silences/{}/expire", ids[0]),
            Some(PAGER),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        again["ended_at"], ended["ended_at"],
        "ending twice moves nothing"
    );
    let (status, _, _) = api
        .send(
            Method::POST,
            "/api/v1/silences/999/expire",
            Some(PAGER),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _, page) = api
        .send(Method::GET, "/api/v1/silences?limit=2", Some(PAGER), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let listed: Vec<i64> = page["silences"]
        .as_array()
        .unwrap()
        .iter()
        .map(|silence| silence["id"].as_i64().unwrap())
        .collect();
    assert_eq!(listed, [ids[2], ids[1]]);
    assert_eq!(page["next_before"], ids[1]);
    let (_, _, last) = api
        .send(
            Method::GET,
            &format!("/api/v1/silences?limit=2&before={}", ids[1]),
            Some(PAGER),
            None,
        )
        .await;
    assert_eq!(last["silences"][0]["id"], ids[0]);
    assert_eq!(last["next_before"], Value::Null, "{last}");
    let (status, _, _) = api
        .send(Method::GET, "/api/v1/silences?limit=0", Some(PAGER), None)
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}
