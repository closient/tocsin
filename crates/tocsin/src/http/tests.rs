// SPDX-License-Identifier: AGPL-3.0-only

//! The API in process: auth, validation, events and the OpenAPI document.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::{Value, json};
use tocsin_core::config::{Environment, Source};
use tocsin_store::Store;
use tower::ServiceExt;

use super::events::{Event, EventBus};
use super::{AppState, router};

const CONFIG: &str = r#"
[server]

[[component]]
id = "api"
name = "API"

[[component]]
id = "web"
name = "Web"

[[token]]
id = "oncall"
secret = "${ENV:ONCALL_TOKEN}"
scopes = ["incident"]

[[token]]
id = "deploys"
secret = "${ENV:DEPLOY_TOKEN}"
scopes = ["release", "heartbeat"]
"#;

const ONCALL: &str = "oncall-secret-0123456789";
const DEPLOYS: &str = "deploy-secret-0123456789";

struct Api {
    _dir: tempfile::TempDir,
    state: AppState,
    router: Router,
}

fn api() -> Api {
    let variable = |name: &str| match name {
        "ONCALL_TOKEN" => Some(ONCALL.to_owned()),
        "DEPLOY_TOKEN" => Some(DEPLOYS.to_owned()),
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
    ) -> (StatusCode, Value, axum::http::HeaderMap) {
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
        (status, value, headers)
    }

    async fn create(&self, token: Option<&str>, body: Value) -> (StatusCode, Value) {
        let (status, body, _) = self
            .send(Method::POST, "/api/v1/incidents", token, Some(body))
            .await;
        (status, body)
    }
}

fn outage() -> Value {
    json!({
        "title": "API errors",
        "impact": "major",
        "components": ["api"],
        "message": "Looking into it."
    })
}

/// Every write route, with a body that would be valid.
fn writes() -> [(Method, &'static str, Value); 3] {
    [
        (Method::POST, "/api/v1/incidents", outage()),
        (
            Method::POST,
            "/api/v1/incidents/1/updates",
            json!({"status": "resolved", "message": "Fixed."}),
        ),
        (
            Method::PATCH,
            "/api/v1/incidents/1",
            json!({"title": "Renamed"}),
        ),
    ]
}

#[tokio::test]
async fn writes_without_a_known_token_are_401() {
    let api = api();
    api.create(Some(ONCALL), outage()).await;
    for (method, uri, body) in writes() {
        for token in [None, Some("wrong"), Some(&ONCALL[..ONCALL.len() - 1])] {
            let (status, error, headers) = api
                .send(method.clone(), uri, token, Some(body.clone()))
                .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri} {token:?}");
            assert_eq!(error["error"], "unauthorized");
            assert_eq!(headers[header::WWW_AUTHENTICATE], "Bearer");
            assert!(!error.to_string().contains(ONCALL));
        }
    }
}

#[tokio::test]
async fn a_token_without_the_incident_scope_is_403() {
    let api = api();
    api.create(Some(ONCALL), outage()).await;
    for (method, uri, body) in writes() {
        let (status, error, _) = api
            .send(method.clone(), uri, Some(DEPLOYS), Some(body))
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
        assert_eq!(error["error"], "forbidden");
        assert!(error["message"].as_str().unwrap().contains("deploys"));
        assert!(
            !error.to_string().contains(DEPLOYS),
            "the secret is never echoed"
        );
    }
}

#[tokio::test]
async fn the_scheme_is_case_insensitive_and_the_token_is_the_author() {
    let api = api();
    let request = Request::post("/api/v1/incidents")
        .header(header::AUTHORIZATION, format!("bearer {ONCALL}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(outage().to_string()))
        .unwrap();
    let response = api.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.headers()[header::LOCATION], "/api/v1/incidents/1");
    let (_, incident, _) = api
        .send(Method::GET, "/api/v1/incidents/1", None, None)
        .await;
    assert_eq!(
        incident["created_by"],
        json!({"kind": "token", "id": "oncall"})
    );
    assert_eq!(incident["updates"][0]["created_by"]["id"], "oncall");
}

#[tokio::test]
async fn auth_is_checked_before_the_body() {
    let api = api();
    let (status, _) = api.create(None, json!({"nonsense": true})).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn unknown_components_are_rejected_by_path() {
    let api = api();
    let mut body = outage();
    body["components"] = json!(["api", "nope"]);
    let (status, error) = api.create(Some(ONCALL), body).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error["error"], "invalid");
    assert_eq!(error["fields"][0]["path"], "components[1]");
    assert!(
        error["fields"][0]["message"]
            .as_str()
            .unwrap()
            .contains("nope")
    );

    api.create(Some(ONCALL), outage()).await;
    let (status, error, _) = api
        .send(
            Method::PATCH,
            "/api/v1/incidents/1",
            Some(ONCALL),
            Some(json!({"components": ["web", "gone"]})),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error["fields"][0]["path"], "components[1]");
}

#[tokio::test]
async fn bad_bodies_are_422_with_paths_or_400() {
    let api = api();
    let cases = [
        (
            json!({"title": "x", "message": "y", "impact": "awful"}),
            "impact",
        ),
        (json!({"title": "x"}), "message"),
        (json!({"title": "", "message": "y"}), "title"),
        (
            json!({"title": "x", "message": "y", "colour": "red"}),
            "colour",
        ),
        (
            json!({"kind": "maintenance", "title": "x", "message": "y"}),
            "scheduled_start",
        ),
        (
            json!({"title": "x", "message": "y", "scheduled_start": "yesterday"}),
            "scheduled_start",
        ),
    ];
    for (body, path) in cases {
        let (status, error) = api.create(Some(ONCALL), body.clone()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}: {error}");
        assert_eq!(error["fields"][0]["path"], path, "{body}: {error}");
    }

    let request = Request::post("/api/v1/incidents")
        .header(header::AUTHORIZATION, format!("Bearer {ONCALL}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{"))
        .unwrap();
    let response = api.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// A body over the limit is 413, not a 400 that blames its syntax.
#[tokio::test]
async fn an_oversized_body_is_413() {
    let api = api();
    let title = "x".repeat(super::BODY_LIMIT);
    let (status, error) = api
        .create(Some(ONCALL), json!({"title": title, "message": "y"}))
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{error}");
    assert_eq!(error["error"], "too_large");
}

/// A title is one line, whether set on create or by PATCH.
#[tokio::test]
async fn a_title_with_a_line_break_is_422_on_create_and_edit() {
    let api = api();
    let (status, error) = api
        .create(
            Some(ONCALL),
            json!({"title": "API\nerrors", "message": "y"}),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    assert_eq!(error["fields"][0]["path"], "title", "{error}");

    api.create(Some(ONCALL), outage()).await;
    let (status, error, _) = api
        .send(
            Method::PATCH,
            "/api/v1/incidents/1",
            Some(ONCALL),
            Some(json!({"title": "API errors\r"})),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    assert_eq!(error["fields"][0]["path"], "title", "{error}");
}

/// A title is at most 200 characters over the API, as the schema says.
#[tokio::test]
async fn a_title_over_200_characters_is_422_on_create_and_edit() {
    let api = api();
    let long = "x".repeat(201);
    let (status, error) = api
        .create(Some(ONCALL), json!({"title": long, "message": "y"}))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    assert_eq!(error["fields"][0]["path"], "title", "{error}");

    let (status, _) = api
        .create(
            Some(ONCALL),
            json!({"title": "x".repeat(200), "message": "y"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, error, _) = api
        .send(
            Method::PATCH,
            "/api/v1/incidents/1",
            Some(ONCALL),
            Some(json!({"title": long})),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    assert_eq!(error["fields"][0]["path"], "title", "{error}");
}

/// Scheduling an existing incident cannot be fixed by changing its kind,
/// so the error does not suggest it.
#[tokio::test]
async fn a_schedule_on_an_incident_edit_says_kind_is_fixed() {
    let api = api();
    api.create(Some(ONCALL), outage()).await;
    let (status, error, _) = api
        .send(
            Method::PATCH,
            "/api/v1/incidents/1",
            Some(ONCALL),
            Some(json!({"scheduled_end": "2026-10-23T12:00:00Z"})),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    assert_eq!(error["fields"][0]["path"], "scheduled_end");
    let message = error["fields"][0]["message"].as_str().unwrap();
    assert!(
        message.contains("fixed at creation") && !message.contains("set kind"),
        "{message}"
    );
}

#[tokio::test]
async fn updates_and_edits_follow_the_kind_and_404_unknown_ids() {
    let api = api();
    api.create(Some(ONCALL), outage()).await;
    let (status, error, _) = api
        .send(
            Method::POST,
            "/api/v1/incidents/1/updates",
            Some(ONCALL),
            Some(json!({"status": "completed", "message": "x"})),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    assert_eq!(error["fields"][0]["path"], "status");

    for uri in ["/api/v1/incidents/9/updates", "/api/v1/incidents/x/updates"] {
        let (status, _, _) = api
            .send(
                Method::POST,
                uri,
                Some(ONCALL),
                Some(json!({"status": "resolved", "message": "x"})),
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
    let (status, error, _) = api
        .send(Method::GET, "/api/v1/incidents/9", None, None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(error["error"], "not_found");
}

#[tokio::test]
async fn the_list_pages_and_validates_its_query() {
    let api = api();
    for _ in 0..3 {
        api.create(Some(ONCALL), outage()).await;
    }
    let (status, page, _) = api
        .send(Method::GET, "/api/v1/incidents?limit=2", None, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["incidents"].as_array().unwrap().len(), 2);
    assert_eq!(page["next_before"], 2);
    let (_, page, _) = api
        .send(
            Method::GET,
            "/api/v1/incidents?limit=2&before=2",
            None,
            None,
        )
        .await;
    assert_eq!(page["incidents"][0]["id"], 1);
    assert_eq!(page["next_before"], Value::Null);
    // A last page that is exactly full has no cursor either; one more
    // incident beyond it does.
    let (_, page, _) = api
        .send(Method::GET, "/api/v1/incidents?limit=3", None, None)
        .await;
    assert_eq!(page["incidents"].as_array().unwrap().len(), 3);
    assert_eq!(page["next_before"], Value::Null, "{page}");
    let (_, page, _) = api
        .send(
            Method::GET,
            "/api/v1/incidents?limit=2&before=3",
            None,
            None,
        )
        .await;
    assert_eq!(page["incidents"].as_array().unwrap().len(), 2);
    assert_eq!(page["next_before"], Value::Null, "{page}");
    api.create(Some(ONCALL), outage()).await;
    let (_, page, _) = api
        .send(Method::GET, "/api/v1/incidents?limit=3", None, None)
        .await;
    assert_eq!(page["incidents"].as_array().unwrap().len(), 3);
    assert_eq!(page["next_before"], 2, "{page}");
    for query in ["limit=0", "limit=101", "limit=x", "colour=red"] {
        let (status, _, _) = api
            .send(
                Method::GET,
                &format!("/api/v1/incidents?{query}"),
                None,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{query}");
    }
}

#[tokio::test]
async fn every_write_emits_an_event_after_it_commits() {
    let api = api();
    let mut events = api.state.events.subscribe();
    let (_, created) = api.create(Some(ONCALL), outage()).await;
    let Event::IncidentCreated { incident } = events.try_recv().unwrap() else {
        panic!("expected IncidentCreated");
    };
    assert_eq!(serde_json::to_value(&incident).unwrap(), created);

    api.send(
        Method::POST,
        "/api/v1/incidents/1/updates",
        Some(ONCALL),
        Some(json!({"status": "resolved", "message": "Fixed."})),
    )
    .await;
    let Event::IncidentUpdated { incident } = events.try_recv().unwrap() else {
        panic!("expected IncidentUpdated");
    };
    assert_eq!(incident.updates.len(), 2);

    api.send(
        Method::PATCH,
        "/api/v1/incidents/1",
        Some(ONCALL),
        Some(json!({"title": "Renamed"})),
    )
    .await;
    let Event::IncidentEdited { incident } = events.try_recv().unwrap() else {
        panic!("expected IncidentEdited");
    };
    assert_eq!(incident.title, "Renamed");

    // Refused writes emit nothing.
    api.create(Some(DEPLOYS), outage()).await;
    api.create(Some(ONCALL), json!({"title": ""})).await;
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn errors_are_json_everywhere() {
    let api = api();
    let (status, error, _) = api.send(Method::GET, "/nowhere", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(error["error"], "not_found");
    let (status, error, _) = api
        .send(Method::DELETE, "/api/v1/incidents", None, None)
        .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(error["error"], "method_not_allowed");
}

#[tokio::test]
async fn the_openapi_document_is_served_and_lists_every_incident_route() {
    let api = api();
    let (status, spec, _) = api.send(Method::GET, super::OPENAPI_PATH, None, None).await;
    assert_eq!(status, StatusCode::OK);
    let routes = [
        ("/api/v1/incidents", "get", false),
        ("/api/v1/incidents", "post", true),
        ("/api/v1/incidents/{id}", "get", false),
        ("/api/v1/incidents/{id}", "patch", true),
        ("/api/v1/incidents/{id}/updates", "post", true),
    ];
    for (path, method, secured) in routes {
        let operation = &spec["paths"][path][method];
        assert!(
            operation.is_object(),
            "{method} {path} missing from the spec"
        );
        assert_eq!(
            operation["security"] == json!([{"bearer": []}]),
            secured,
            "{method} {path} security"
        );
    }
    assert_eq!(
        spec["components"]["securitySchemes"]["bearer"]["scheme"],
        "bearer"
    );
    for schema in [
        "Incident",
        "NewIncident",
        "NewUpdate",
        "IncidentEdit",
        "ErrorBody",
    ] {
        assert!(
            spec["components"]["schemas"][schema].is_object(),
            "schema {schema} missing"
        );
    }
}
