// SPDX-License-Identifier: AGPL-3.0-only

//! The release API, the timeline and the public release pages in process:
//! idempotent writes, deploy stamps, limits, visibility and escaping.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::{Value, json};
use tocsin_core::Health;
use tocsin_core::config::{Environment, Source};
use tocsin_store::{NewOutage, Store};
use tower::ServiceExt;

use super::events::{Event, EventBus};
use super::{AppState, now, router};

const CONFIG: &str = r#"
[server]
public_url = "https://status.example.com/"

[[component]]
id = "api"
name = "API"

[[component]]
id = "web"
name = "Web"

[[check]]
id = "api-port"
component = "api"
type = "tcp"
target = "api.example.com:443"

[[project]]
id = "api"
name = "API server"
components = ["api"]

[[project]]
id = "internal"
name = "Internal tool"
public = false

[[token]]
id = "deploys"
secret = "${ENV:DEPLOY_TOKEN}"
scopes = ["release"]

[[token]]
id = "reader"
secret = "${ENV:READER_TOKEN}"
scopes = ["status:read"]

[[token]]
id = "oncall"
secret = "${ENV:ONCALL_TOKEN}"
scopes = ["incident"]
"#;

const DEPLOYS: &str = "deploy-secret-0123456789";
const READER: &str = "reader-secret-0123456789";
const ONCALL: &str = "oncall-secret-0123456789";

struct Api {
    _dir: tempfile::TempDir,
    state: AppState,
    router: Router,
}

fn api() -> Api {
    let variable = |name: &str| match name {
        "DEPLOY_TOKEN" => Some(DEPLOYS.to_owned()),
        "READER_TOKEN" => Some(READER.to_owned()),
        "ONCALL_TOKEN" => Some(ONCALL.to_owned()),
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
    async fn raw(&self, request: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
    }

    async fn send(
        &self,
        method: Method,
        uri: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
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
        let (status, _, text) = self.raw(request).await;
        let value = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap()
        };
        (status, value)
    }

    async fn publish(&self, body: Value) -> (StatusCode, Value) {
        self.send(Method::POST, "/api/v1/releases", Some(DEPLOYS), Some(body))
            .await
    }

    async fn deployed(
        &self,
        project: &str,
        version: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        self.send(
            Method::POST,
            &format!("/api/v1/releases/{project}/{version}/deployed"),
            Some(DEPLOYS),
            body,
        )
        .await
    }

    async fn page(&self, uri: &str) -> (StatusCode, String) {
        let (status, _, text) = self
            .raw(Request::get(uri).body(Body::empty()).unwrap())
            .await;
        (status, text)
    }
}

fn release(project: &str, version: &str) -> Value {
    json!({
        "project": project,
        "version": version,
        "title": format!("{project} {version}"),
        "notes": "**Faster** search.",
        "url": "https://github.com/closient/tocsin/releases/tag/v1",
    })
}

#[tokio::test]
async fn publishing_twice_updates_the_same_release() {
    let api = api();
    let (status, first) = api.publish(release("api", "v1.0.0")).await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    assert_eq!(first["environment"], "production");
    assert_eq!(
        first["created_by"],
        json!({"kind": "token", "id": "deploys"})
    );

    let mut again = release("api", "v1.0.0");
    again["notes"] = json!("Faster search, and a fix.");
    let (status, second) = api.publish(again).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(second["id"], first["id"]);
    assert_eq!(second["notes"], "Faster search, and a fix.");
    assert_eq!(second["released_at"], first["released_at"]);

    let mut staging = release("api", "v1.0.0");
    staging["environment"] = json!("staging");
    let (status, other) = api.publish(staging).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_ne!(other["id"], first["id"]);

    let (_, list) = api.send(Method::GET, "/api/v1/releases", None, None).await;
    assert_eq!(list["releases"].as_array().unwrap().len(), 2, "{list}");
}

#[tokio::test]
async fn a_deploy_before_the_publish_is_a_stub_the_publish_fills() {
    let api = api();
    // No body at all: production, now.
    let (status, stub) = api.deployed("api", "v2.0.0", None).await;
    assert_eq!(status, StatusCode::CREATED, "{stub}");
    assert_eq!(stub["title"], "");
    assert_eq!(stub["released_at"], Value::Null);
    assert!(stub["deployed_at"].is_string());
    let (_, list) = api.send(Method::GET, "/api/v1/releases", None, None).await;
    assert_eq!(list["releases"], json!([]), "a stub is not in the log");
    let (_, timeline) = api.send(Method::GET, "/api/v1/timeline", None, None).await;
    assert_eq!(timeline["events"][0]["kind"], "deploy", "{timeline}");

    let (status, published) = api.publish(release("api", "v2.0.0")).await;
    assert_eq!(status, StatusCode::OK, "{published}");
    assert_eq!(published["id"], stub["id"]);
    assert_eq!(published["deployed_at"], stub["deployed_at"]);
    assert_eq!(published["title"], "api v2.0.0");

    // Stamped after the publish, with a body naming the environment.
    let mut staging = release("api", "v2.0.0");
    staging["environment"] = json!("staging");
    api.publish(staging).await;
    let (status, stamped) = api
        .deployed(
            "api",
            "v2.0.0",
            Some(json!({"environment": "staging", "deployed_at": "2026-10-23T12:00:00Z"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{stamped}");
    assert_eq!(stamped["environment"], "staging");
    assert_eq!(stamped["deployed_at"], "2026-10-23T12:00:00.000Z");
    assert_eq!(stamped["title"], "api v2.0.0");
}

#[tokio::test]
async fn an_unknown_project_is_422_naming_it() {
    let api = api();
    let (status, error) = api.publish(release("nope", "v1")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    assert_eq!(error["fields"][0]["path"], "project");
    assert!(
        error["message"].as_str().unwrap().contains("nope"),
        "{error}"
    );
    let (status, error) = api.deployed("nope", "v1", None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    assert!(
        error["message"].as_str().unwrap().contains("nope"),
        "{error}"
    );
}

#[tokio::test]
async fn writes_need_the_release_scope() {
    let api = api();
    for (token, expected) in [
        (None, StatusCode::UNAUTHORIZED),
        (Some("wrong"), StatusCode::UNAUTHORIZED),
        (Some(ONCALL), StatusCode::FORBIDDEN),
        (Some(READER), StatusCode::FORBIDDEN),
    ] {
        let (status, _) = api
            .send(
                Method::POST,
                "/api/v1/releases",
                token,
                Some(release("api", "v1")),
            )
            .await;
        assert_eq!(status, expected, "{token:?}");
        let (status, _) = api
            .send(
                Method::POST,
                "/api/v1/releases/api/v1/deployed",
                token,
                None,
            )
            .await;
        assert_eq!(status, expected, "{token:?}");
    }
}

#[tokio::test]
async fn notes_over_64_kib_are_413_and_titles_over_200_are_422() {
    let api = api();
    let mut body = release("api", "v1");
    body["notes"] = json!("x".repeat(64 * 1024 + 1));
    let (status, error) = api.publish(body.clone()).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{error}");
    assert_eq!(error["error"], "too_large");
    body["notes"] = json!("x".repeat(64 * 1024));
    assert_eq!(api.publish(body).await.0, StatusCode::CREATED);

    let mut body = release("api", "v2");
    body["title"] = json!("x".repeat(201));
    let (status, error) = api.publish(body.clone()).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    assert_eq!(error["fields"][0]["path"], "title");
    body["title"] = json!("x".repeat(200));
    assert_eq!(api.publish(body).await.0, StatusCode::CREATED);
}

#[tokio::test]
async fn versions_are_url_safe_in_the_body_and_the_path() {
    let api = api();
    for version in ["a/b", "..", "../etc", "v\u{e9}", "", "-rc", &"1".repeat(65)] {
        let (status, error) = api.publish(release("api", version)).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{version:?}: {error}"
        );
        assert_eq!(error["fields"][0]["path"], "version", "{version:?}");
    }
    for path in ["..", "a%2Fb", "v%C3%A9", "%2E%2E"] {
        let (status, error) = api.deployed("api", path, None).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{path}: {error}");
        assert_eq!(error["fields"][0]["path"], "version", "{path}");
    }
    let (status, _) = api.publish(release("api", "1.2.3-rc.1+build.7")).await;
    assert_eq!(status, StatusCode::CREATED);
}

/// The ticket's acceptance test: a non-public project's release is absent
/// everywhere an anonymous reader looks.
#[tokio::test]
async fn a_private_release_is_absent_without_status_read() {
    let api = api();
    api.publish(release("api", "v1")).await;
    let mut private = release("internal", "v9");
    private["title"] = json!("Secret internal title");
    let (status, _) = api.publish(private).await;
    assert_eq!(status, StatusCode::CREATED);
    api.deployed("internal", "v9", None).await;

    let (_, list) = api.send(Method::GET, "/api/v1/releases", None, None).await;
    assert!(!list.to_string().contains("internal"), "{list}");
    assert_eq!(list["releases"].as_array().unwrap().len(), 1);
    let (_, asked) = api
        .send(Method::GET, "/api/v1/releases?project=internal", None, None)
        .await;
    assert_eq!(asked["releases"], json!([]));
    let (_, timeline) = api.send(Method::GET, "/api/v1/timeline", None, None).await;
    assert!(!timeline.to_string().contains("internal"), "{timeline}");
    assert_eq!(timeline["events"].as_array().unwrap().len(), 1);

    for page in ["/releases", "/releases.atom", "/releases?project=api"] {
        let (status, text) = api.page(page).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert!(
            !text.contains("Secret internal title") && !text.contains("v9"),
            "{page}"
        );
    }
    for page in [
        "/releases/internal/v9",
        "/releases?project=internal",
        "/releases.atom?project=internal",
    ] {
        let (status, text) = api.page(page).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{page}");
        assert!(!text.contains("Secret"), "{page}");
    }

    // A status:read token sees it.
    let (_, list) = api
        .send(Method::GET, "/api/v1/releases", Some(READER), None)
        .await;
    assert_eq!(list["releases"].as_array().unwrap().len(), 2, "{list}");
    let (_, timeline) = api
        .send(Method::GET, "/api/v1/timeline", Some(READER), None)
        .await;
    assert_eq!(
        timeline["events"].as_array().unwrap().len(),
        2,
        "{timeline}"
    );

    // A token that is not status:read is refused, not quietly narrowed.
    for (token, expected) in [
        (DEPLOYS, StatusCode::FORBIDDEN),
        ("wrong", StatusCode::UNAUTHORIZED),
    ] {
        for uri in ["/api/v1/releases", "/api/v1/timeline"] {
            let (status, _) = api.send(Method::GET, uri, Some(token), None).await;
            assert_eq!(status, expected, "{uri} {token}");
        }
    }
}

#[tokio::test]
async fn the_timeline_validates_its_window_and_filters_by_component() {
    let api = api();
    for query in [
        "since=2020-01-01T00:00:00Z",
        "limit=0",
        "limit=501",
        "component=nope",
        "colour=red",
        "since=yesterday",
    ] {
        let (status, error) = api
            .send(
                Method::GET,
                &format!("/api/v1/timeline?{query}"),
                None,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{query}: {error}");
    }
    let (_, error) = api
        .send(Method::GET, "/api/v1/timeline?component=nope", None, None)
        .await;
    assert!(error["message"].as_str().unwrap().contains("nope"));

    let start = now().as_millis();
    api.state
        .store
        .open_outage(NewOutage {
            node_id: "node-a".into(),
            check_id: "api-port".into(),
            component_id: None,
            health: Health::Down,
            started_at_ms: start - 1_000,
            alerted: true,
        })
        .await
        .unwrap();
    api.deployed("api", "v1", None).await;
    let (_, timeline) = api
        .send(Method::GET, "/api/v1/timeline?component=web", None, None)
        .await;
    assert_eq!(timeline["events"], json!([]), "{timeline}");
    let (status, timeline) = api
        .send(
            Method::GET,
            "/api/v1/timeline?component=api&limit=500",
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let kinds: Vec<_> = timeline["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["outage", "deploy"], "{timeline}");
}

#[tokio::test]
async fn release_writes_emit_events_that_say_whether_they_are_public() {
    let api = api();
    let mut events = api.state.events.subscribe();
    api.publish(release("api", "v1")).await;
    let Event::ReleasePublished {
        release: published,
        public,
    } = events.try_recv().unwrap()
    else {
        panic!("expected ReleasePublished");
    };
    assert_eq!((published.version.as_str(), public), ("v1", true));
    api.deployed("internal", "v2", None).await;
    let Event::ReleaseDeployed {
        release: deployed,
        public,
    } = events.try_recv().unwrap()
    else {
        panic!("expected ReleaseDeployed");
    };
    assert_eq!((deployed.project.as_str(), public), ("internal", false));
    // Refused writes emit nothing.
    api.publish(release("nope", "v1")).await;
    assert!(events.try_recv().is_err());
}

/// Script, event handlers, `javascript:` links and raw HTML never survive
/// into a page or the feed, from the notes or from the title.
#[tokio::test]
async fn notes_and_titles_are_sanitized_everywhere_they_are_shown() {
    let api = api();
    let hostile = [
        "<script>alert(1)</script>",
        "<img src=x onerror=alert(1)>",
        "[click](javascript:alert(1))",
        "<a href=\"javascript:alert(2)\">x</a>",
        "<iframe src=\"https://evil.example\"></iframe>",
    ];
    for (index, text) in hostile.iter().enumerate() {
        let mut body = release("api", &format!("v{index}"));
        body["title"] = json!(text);
        body["notes"] = json!(format!("Notes {text} end"));
        let (status, _) = api.publish(body).await;
        assert_eq!(status, StatusCode::CREATED, "{text}");
    }
    let mut pages = vec!["/releases".to_owned(), "/releases.atom".to_owned()];
    pages.extend((0..hostile.len()).map(|index| format!("/releases/api/v{index}")));
    for page in &pages {
        let (status, text) = api.page(page).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        for tag in live_tags(text.as_str()) {
            let lower = tag.to_ascii_lowercase();
            for bad in ["<script", "<img", "<iframe", "javascript:", "onerror"] {
                assert!(!lower.contains(bad), "{page} has a live {bad}: {tag}");
            }
        }
    }
    let (_, detail) = api.page("/releases/api/v0").await;
    assert!(
        detail.contains("&lt;script&gt;") || detail.contains("&#60;script&#62;"),
        "{detail}"
    );
}

/// Every markup tag in `text`: `<` then a letter, up to the next `>`.
/// Escaped text (`&lt;img onerror=…&gt;`) is not a tag, so it is not here.
fn live_tags(text: &str) -> Vec<&str> {
    let mut tags = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        rest = &rest[start..];
        let end = rest.find('>').map_or(rest.len(), |end| end + 1);
        if rest[1..].starts_with(|c: char| c.is_ascii_alphabetic()) {
            tags.push(&rest[..end]);
        }
        rest = &rest[end.max(1)..];
    }
    tags
}

#[tokio::test]
async fn the_pages_render_without_script_and_the_feed_is_atom() {
    let api = api();
    api.publish(release("api", "v1.0.0")).await;
    let (status, headers, list) = api
        .raw(Request::get("/releases").body(Body::empty()).unwrap())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("script-src 'self'")
    );
    assert!(!list.contains("<script"), "no script on the page");
    assert!(list.contains("name=\"viewport\""));
    assert!(list.contains("href=\"/releases/api/v1.0.0\""), "{list}");

    let (_, detail) = api.page("/releases/api/v1.0.0").await;
    assert!(
        detail.contains("<strong>Faster</strong> search."),
        "{detail}"
    );

    let (status, headers, feed) = api
        .raw(
            Request::get("/releases.atom?project=api")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers[header::CONTENT_TYPE],
        "application/atom+xml; charset=utf-8"
    );
    assert!(feed.starts_with("<?xml"), "{feed}");
    assert!(
        feed.contains("href=\"https://status.example.com/releases/api/v1.0.0\""),
        "{feed}"
    );
    // The notes' HTML, escaped once more as the feed's text content.
    assert!(
        feed.contains("&#60;strong&#62;Faster&#60;/strong&#62;"),
        "{feed}"
    );
}

#[tokio::test]
async fn the_openapi_document_lists_the_release_routes() {
    let api = api();
    let (_, spec) = api.send(Method::GET, super::OPENAPI_PATH, None, None).await;
    for (path, method) in [
        ("/api/v1/releases", "get"),
        ("/api/v1/releases", "post"),
        ("/api/v1/releases/{project}/{version}/deployed", "post"),
        ("/api/v1/timeline", "get"),
    ] {
        assert!(spec["paths"][path][method].is_object(), "{method} {path}");
    }
    for schema in [
        "Release",
        "NewRelease",
        "DeployStamp",
        "ReleaseList",
        "Timeline",
        "TimelineEvent",
    ] {
        assert!(
            spec["components"]["schemas"][schema].is_object(),
            "{schema}"
        );
    }
}

#[tokio::test]
async fn a_time_far_past_the_server_clock_is_422_naming_its_field() {
    let api = api();
    // 3026 for 2026.
    let typo = "3026-10-23T12:00:00Z";
    for field in ["released_at", "deployed_at"] {
        let mut body = release("api", "v1");
        body[field] = json!(typo);
        let (status, error) = api.publish(body).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
        assert_eq!(error["fields"][0]["path"], field, "{error}");
        assert!(
            error["fields"][0]["message"]
                .as_str()
                .unwrap()
                .contains("3026")
        );
    }
    let (status, error) = api
        .deployed("api", "v1", Some(json!({"deployed_at": typo})))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    assert_eq!(error["fields"][0]["path"], "deployed_at", "{error}");
    let (_, list) = api.send(Method::GET, "/api/v1/releases", None, None).await;
    assert_eq!(list["releases"], json!([]), "{list}");
}

#[tokio::test]
async fn the_release_list_pages_down_to_the_oldest_with_before() {
    let api = api();
    let mut published = Vec::new();
    for index in 0..205 {
        let version = format!("r{index}");
        let (status, _) = api.publish(release("api", &version)).await;
        assert_eq!(status, StatusCode::CREATED);
        published.push(version);
    }
    published.reverse();

    let mut seen = Vec::new();
    let mut uri = "/api/v1/releases?limit=200".to_owned();
    loop {
        let (status, page) = api.send(Method::GET, &uri, None, None).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        seen.extend(
            page["releases"]
                .as_array()
                .unwrap()
                .iter()
                .map(|release| release["version"].as_str().unwrap().to_owned()),
        );
        if page["truncated"] == json!(false) {
            assert_eq!(page["next_before"], Value::Null);
            break;
        }
        let before = page["next_before"].as_str().unwrap();
        uri = format!("/api/v1/releases?limit=200&before={before}");
    }
    assert_eq!(seen, published);

    let (status, error) = api
        .send(Method::GET, "/api/v1/releases?before=yesterday", None, None)
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    assert_eq!(error["fields"][0]["path"], "before", "{error}");
}

#[tokio::test]
async fn the_timeline_api_reads_oldest_first_and_says_where_to_go_on() {
    let api = api();
    for index in 0..510 {
        api.deployed("api", &format!("r{index}"), None).await;
    }
    let versions = |timeline: &Value| -> Vec<String> {
        timeline["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["release"]["version"].as_str().unwrap().to_owned())
            .collect()
    };
    let (_, first) = api
        .send(Method::GET, "/api/v1/timeline?limit=500", None, None)
        .await;
    assert_eq!(first["truncated"], true, "{first}");
    let first_versions = versions(&first);
    assert_eq!(first_versions.len(), 500);
    assert_eq!(first_versions[0], "r0");
    let after = first["next_after"].as_str().unwrap();
    let (_, rest) = api
        .send(
            Method::GET,
            &format!("/api/v1/timeline?limit=500&after={after}"),
            None,
            None,
        )
        .await;
    assert_eq!(rest["truncated"], false, "{rest}");
    assert_eq!(rest["next_after"], Value::Null);
    let all: Vec<_> = first_versions.into_iter().chain(versions(&rest)).collect();
    let expected: Vec<_> = (0..510).map(|index| format!("r{index}")).collect();
    assert_eq!(all, expected);

    let (status, error) = api
        .send(Method::GET, "/api/v1/timeline?after=9_9", None, None)
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    assert_eq!(error["fields"][0]["path"], "after", "{error}");
}

#[tokio::test]
async fn a_version_lists_once_and_its_page_shows_every_environment() {
    let api = api();
    let mut staging = release("api", "v2.0.0");
    staging["environment"] = json!("staging");
    staging["title"] = json!("Staging title");
    api.publish(staging).await;
    let mut production = release("api", "v2.0.0");
    production["title"] = json!("Production title");
    api.publish(production).await;
    // Only stamped: no publish for canary.
    api.deployed("api", "v2.0.0", Some(json!({"environment": "canary"})))
        .await;

    let (_, list) = api.page("/releases").await;
    assert_eq!(
        list.matches("href=\"/releases/api/v2.0.0\"").count(),
        1,
        "{list}"
    );
    assert!(list.contains("Production title") && !list.contains("Staging title"));
    let (_, feed) = api.page("/releases.atom").await;
    assert_eq!(feed.matches("<entry>").count(), 1, "{feed}");
    assert!(
        feed.contains("<id>urn:tocsin:release:api:v2.0.0</id>"),
        "{feed}"
    );

    let (status, detail) = api.page("/releases/api/v2.0.0").await;
    assert_eq!(status, StatusCode::OK);
    for environment in ["production", "staging", "canary"] {
        assert!(
            detail.contains(&format!("<span class=\"badge\">{environment}</span>")),
            "{environment}: {detail}"
        );
    }
    // A version that was only ever stamped has no page yet.
    api.deployed("api", "v3.0.0", None).await;
    assert_eq!(
        api.page("/releases/api/v3.0.0").await.0,
        StatusCode::NOT_FOUND
    );
}
