// SPDX-License-Identifier: AGPL-3.0-only

//! Sign-up, confirmation, fan-out and unsubscribe, end to end through the
//! router and the postman, with a capture in place of SMTP.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use serde_json::{Value, json};
use tocsin_core::config::{Config, Environment, Source};
use tocsin_store::{NewSignup, Preferences, Store};
use tower::ServiceExt;

use super::testing::{Capture, Sent};
use super::worker::Postman;
use super::{Subscriptions, token};
use crate::http::events::EventBus;
use crate::http::{self, AppState};
use crate::web::identity::AdminAuth;

const BASE: &str = "https://status.example.com";
const ONCALL: &str = "oncall-secret-0123456789";
const DEPLOYS: &str = "deploy-secret-0123456789";
const OPERATOR: &str = "operator-secret-0123456789";
const READER: &str = "reader-secret-0123456789";
/// Where Turnstile is asked when it is not on: nothing listens.
const NO_TURNSTILE: &str = "http://127.0.0.1:9/";

/// `[subscribers]` with small limits, so the tests can reach them.
const ON: &str = "[subscribers]\nsignups_per_ip = 5\nsignups_per_email = 3\n";

fn config(subscribers: &str) -> Config {
    let admin_token = if subscribers.is_empty() {
        ""
    } else {
        "[[token]]\nid = \"operator\"\nsecret = \"${ENV:OPERATOR_TOKEN}\"\nscopes = [\"subscribers:admin\"]\n\n"
    };
    let text = format!(
        "[server]\npublic_url = \"{BASE}/\"\n\n\
         [server.auth]\nmode = \"trusted_header\"\ntrusted_proxies = [\"10.0.0.0/8\"]\n\n\
         [server.email]\nfrom = \"status@example.com\"\nreply_to = \"support@example.com\"\n\
         smtp_url = \"${{ENV:SMTP_URL}}\"\n\n\
         {subscribers}\n\
         [[component]]\nid = \"api\"\nname = \"API\"\n\n\
         [[component]]\nid = \"web\"\nname = \"Web\"\n\n\
         [[project]]\nid = \"api\"\nname = \"API server\"\ncomponents = [\"api\"]\n\n\
         [[project]]\nid = \"internal\"\nname = \"Internal tool\"\npublic = false\n\n\
         [[token]]\nid = \"oncall\"\nsecret = \"${{ENV:ONCALL_TOKEN}}\"\nscopes = [\"incident\"]\n\n\
         [[token]]\nid = \"deploys\"\nsecret = \"${{ENV:DEPLOY_TOKEN}}\"\nscopes = [\"release\"]\n\n\
         [[token]]\nid = \"reader\"\nsecret = \"${{ENV:READER_TOKEN}}\"\nscopes = [\"status:read\"]\n\n\
         {admin_token}"
    );
    let variable = |name: &str| match name {
        "SMTP_URL" => Some("smtp://127.0.0.1:25".to_owned()),
        "ONCALL_TOKEN" => Some(ONCALL.to_owned()),
        "DEPLOY_TOKEN" => Some(DEPLOYS.to_owned()),
        "OPERATOR_TOKEN" => Some(OPERATOR.to_owned()),
        "READER_TOKEN" => Some(READER.to_owned()),
        "TURNSTILE_SECRET" => Some("turnstile-secret".to_owned()),
        _ => None,
    };
    tocsin_core::config::load(
        &[Source {
            name: "tocsin.toml".into(),
            text: Ok(text),
        }],
        &Environment {
            hostname: "test".into(),
            variable: &variable,
        },
    )
    .unwrap()
}

/// A store, the state around it, the router, and every message sent.
struct Site {
    _dir: tempfile::TempDir,
    state: AppState,
    router: Router,
    sent: Arc<Capture>,
    peers: AtomicU8,
}

fn site() -> Site {
    site_with(ON, NO_TURNSTILE)
}

fn site_with(subscribers: &str, turnstile_url: &str) -> Site {
    let config = config(subscribers);
    let sent = Capture::new();
    let subscriptions = config.subscribers.is_some().then(|| {
        Arc::new(
            Subscriptions::with_outbox(&config, Arc::clone(&sent) as _, turnstile_url).unwrap(),
        )
    });
    let admin = AdminAuth::from_config(&config).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join(tocsin_store::DB_FILE)).unwrap();
    let state = AppState {
        store: Arc::new(store),
        config: Arc::new(config),
        events: EventBus::new(),
        admin: admin.map(Arc::new),
        live: Arc::new(tocsin_engine::Snapshot::default()),
        heartbeats: tocsin_engine::HeartbeatHandle::default(),
        config_status: crate::reload::steady_status(),
        metrics: Arc::new(crate::metrics::Metrics::new()),
        subscribers: subscriptions,
    };
    Site {
        _dir: dir,
        router: http::router(state.clone()),
        state,
        sent,
        peers: AtomicU8::new(10),
    }
}

/// A request from client `198.51.100.<peer>`.
fn request(method: Method, uri: &str, peer: u8) -> axum::http::request::Builder {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "status.example.com");
    request
        .extensions_mut()
        .unwrap()
        .insert(ConnectInfo(SocketAddr::from((
            [198, 51, 100, peer],
            40_000,
        ))));
    request
}

fn form(fields: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields)
        .finish()
}

/// The path of the link in `body` that starts with `prefix`.
fn link(body: &str, prefix: &str) -> String {
    let start = body
        .find(&format!("{BASE}{prefix}"))
        .unwrap_or_else(|| panic!("no {prefix} link in {body}"))
        + BASE.len();
    body[start..].split_whitespace().next().unwrap().to_owned()
}

fn subject(sent: &Sent) -> &str {
    sent.header("Subject").unwrap()
}

impl Site {
    fn subscriptions(&self) -> Arc<Subscriptions> {
        Arc::clone(self.state.subscribers.as_ref().unwrap())
    }

    /// A client no earlier request came from.
    fn fresh_peer(&self) -> u8 {
        self.peers.fetch_add(1, Ordering::Relaxed)
    }

    async fn call(&self, request: Request<Body>) -> (StatusCode, HeaderMap, String) {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
    }

    async fn get(&self, uri: &str) -> (StatusCode, HeaderMap, String) {
        self.call(request(Method::GET, uri, 1).body(Body::empty()).unwrap())
            .await
    }

    /// A form post, from `origin` if given.
    async fn post(
        &self,
        uri: &str,
        origin: Option<&str>,
        body: String,
        peer: u8,
    ) -> (StatusCode, HeaderMap, String) {
        let mut builder = request(Method::POST, uri, peer)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(origin) = origin {
            builder = builder.header(header::ORIGIN, origin);
        }
        self.call(builder.body(Body::from(body)).unwrap()).await
    }

    async fn api(
        &self,
        method: Method,
        uri: &str,
        token: Option<&str>,
        body: Option<Value>,
        peer: u8,
    ) -> (StatusCode, HeaderMap, Value) {
        let mut builder = request(method, uri, peer);
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let request = match body {
            Some(body) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string())),
            None => builder.body(Body::empty()),
        }
        .unwrap();
        let (status, headers, text) = self.call(request).await;
        let value = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::String(text))
        };
        (status, headers, value)
    }

    async fn sign_up(&self, body: Value, peer: u8) -> (StatusCode, HeaderMap, Value) {
        self.api(Method::POST, "/api/v1/subscribers", None, Some(body), peer)
            .await
    }

    /// The newest confirmation link sent to `email`, after waiting for the
    /// `nth` (1-based) message to it.
    async fn confirmation(&self, email: &str, nth: usize) -> String {
        let sent = self
            .sent
            .wait_until(&format!("message {nth} to {email}"), |sent| {
                sent.iter().filter(|message| message.to == email).count() >= nth
            })
            .await;
        let last = sent.iter().rfind(|message| message.to == email).unwrap();
        assert!(subject(last).starts_with("Confirm"), "{}", subject(last));
        link(&last.body(), "/subscribe/confirm/")
    }

    /// Signs `email` up for `body`'s choices and confirms it as its owner
    /// would; its manage token.
    async fn subscribe_with(&self, email: &str, choices: Value) -> String {
        let mut body = choices;
        body["email"] = json!(email);
        let before = self.sent.to(email).len();
        let (status, _, answer) = self.sign_up(body, self.fresh_peer()).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{answer}");
        let confirm = self.confirmation(email, before + 1).await;
        let (status, headers, page) = self
            .post(&confirm, Some(BASE), String::new(), self.fresh_peer())
            .await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
        let location = headers[header::LOCATION].to_str().unwrap();
        location
            .strip_prefix("/subscribe/manage/")
            .and_then(|rest| rest.strip_suffix("?done=confirmed"))
            .unwrap_or_else(|| panic!("confirmed to {location}"))
            .to_owned()
    }

    /// [`Site::subscribe_with`] every component and project.
    async fn subscribe(&self, email: &str) -> String {
        self.subscribe_with(email, json!({"components": ["*"], "projects": ["*"]}))
            .await
    }

    fn postman(&self) -> Postman {
        Postman::start(Arc::clone(&self.state.store), self.subscriptions())
    }

    async fn open_incident(&self, title: &str, components: &[&str]) -> i64 {
        let (status, _, incident) = self
            .api(
                Method::POST,
                "/api/v1/incidents",
                Some(ONCALL),
                Some(json!({
                    "title": title,
                    "impact": "major",
                    "components": components,
                    "message": "Looking into it.",
                })),
                1,
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{incident}");
        incident["id"].as_i64().unwrap()
    }

    async fn update_incident(&self, id: i64, message: &str, notify: bool) {
        let (status, _, incident) = self
            .api(
                Method::POST,
                &format!("/api/v1/incidents/{id}/updates"),
                Some(ONCALL),
                Some(json!({"status": "identified", "message": message, "notify": notify})),
                1,
            )
            .await;
        assert!(status.is_success(), "{status} {incident}");
    }

    async fn publish(&self, project: &str, version: &str) {
        let (status, _, release) = self
            .api(
                Method::POST,
                "/api/v1/releases",
                Some(DEPLOYS),
                Some(json!({"project": project, "version": version, "title": "Faster"})),
                1,
            )
            .await;
        assert!(status.is_success(), "{status} {release}");
    }

    /// Waits until every announcement is expanded and every delivery
    /// finished.
    async fn settled(&self) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let busy: i64 = self
                .state
                .store
                .read(|conn| {
                    Ok(conn.query_row(
                        "SELECT (SELECT count(*) FROM subscriber_event WHERE expanded_at IS NULL)
                              + (SELECT count(*) FROM subscriber_delivery
                                 WHERE status IN ('pending', 'sending'))",
                        [],
                        |row| row.get(0),
                    )?)
                })
                .await
                .unwrap();
            if busy == 0 {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{busy} announcements or deliveries still open"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Each delivery's recipient and status, in id order.
    async fn deliveries(&self) -> Vec<(String, String)> {
        self.state
            .store
            .read(|conn| {
                let mut statement = conn.prepare(
                    "SELECT s.email, d.status FROM subscriber_delivery d
                     JOIN subscriber s ON s.id = d.subscriber_id ORDER BY d.id",
                )?;
                let rows = statement
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await
            .unwrap()
    }

    fn subscriber_id(manage: &str) -> i64 {
        token::parse_manage(manage).unwrap().0
    }
}

#[tokio::test]
async fn unconfirmed_addresses_receive_only_the_confirmation() {
    let site = site();
    let postman = site.postman();
    let (status, _, _) = site
        .sign_up(
            json!({"email": "waiting@example.com", "projects": ["*"]}),
            site.fresh_peer(),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    // A mail scanner that opens the link confirms nothing.
    let confirm = site.confirmation("waiting@example.com", 1).await;
    let (status, _, _) = site.get(&confirm).await;
    assert_eq!(status, StatusCode::OK);
    site.subscribe("reader@example.com").await;

    let id = site.open_incident("API errors", &["api"]).await;
    site.update_incident(id, "Quiet typo fix.", false).await;
    site.update_incident(id, "Found the cause.", true).await;
    site.publish("api", "v1.0.0").await;
    site.settled().await;
    postman.stop().await;

    let waiting = site.sent.to("waiting@example.com");
    assert_eq!(waiting.len(), 1, "{waiting:#?}");
    assert!(subject(&waiting[0]).starts_with("Confirm your subscription"));
    assert!(
        site.deliveries()
            .await
            .iter()
            .all(|(email, _)| email != "waiting@example.com"),
        "a delivery was queued for an unconfirmed address"
    );

    let reader = site.sent.to("reader@example.com");
    let subjects: Vec<&str> = reader.iter().map(subject).collect();
    assert_eq!(subjects.len(), 4, "{subjects:#?}");
    assert!(
        subjects[1].contains("Incident: API errors"),
        "{subjects:#?}"
    );
    assert!(subjects[2].contains("API errors"), "{subjects:#?}");
    assert!(
        subjects[3].contains("API server v1.0.0 released"),
        "{subjects:#?}"
    );
    assert!(
        reader
            .iter()
            .all(|sent| !sent.body().contains("Quiet typo fix.")),
        "a notify=false update was sent"
    );
    assert!(reader[2].body().contains("Found the cause."));

    // Every fan-out message can be answered and left in one click.
    for sent in &reader[1..] {
        assert_eq!(sent.header("Reply-To"), Some("support@example.com"));
        assert!(sent.header("From").unwrap().contains("status@example.com"));
        let unsubscribe = sent.header("List-Unsubscribe").unwrap();
        assert!(
            unsubscribe.starts_with(&format!("<{BASE}/subscribe/unsubscribe/")),
            "{unsubscribe}"
        );
        assert_eq!(
            sent.header("List-Unsubscribe-Post"),
            Some("List-Unsubscribe=One-Click")
        );
        assert!(sent.body().contains("/subscribe/manage/"));
    }
}

#[tokio::test]
async fn restarting_mid_fan_out_sends_no_duplicates() {
    let site = site();
    for email in ["a@example.com", "b@example.com", "c@example.com"] {
        site.subscribe(email).await;
    }
    site.open_incident("API errors", &["api"]).await;

    // The first postman's second send never answers, as an SMTP server
    // that took the message and then died; the process stops under it.
    let first = Capture::hanging_on(2);
    let postman = Postman::start(
        Arc::clone(&site.state.store),
        Arc::new(
            Subscriptions::with_outbox(&site.state.config, Arc::clone(&first) as _, NO_TURNSTILE)
                .unwrap(),
        ),
    );
    first.wait_for(2).await;
    postman.stop_within(Duration::from_millis(50)).await;

    let second = Capture::new();
    let postman = Postman::start(
        Arc::clone(&site.state.store),
        Arc::new(
            Subscriptions::with_outbox(&site.state.config, Arc::clone(&second) as _, NO_TURNSTILE)
                .unwrap(),
        ),
    );
    site.settled().await;
    postman.stop().await;

    let recipients = |capture: &Capture| -> Vec<String> {
        capture.sent().into_iter().map(|sent| sent.to).collect()
    };
    assert_eq!(recipients(&first), ["a@example.com", "b@example.com"]);
    assert_eq!(recipients(&second), ["c@example.com"]);
    assert_eq!(
        site.deliveries().await,
        [
            ("a@example.com".to_owned(), "sent".to_owned()),
            ("b@example.com".to_owned(), "failed".to_owned()),
            ("c@example.com".to_owned(), "sent".to_owned()),
        ]
    );
}

#[tokio::test]
async fn one_click_unsubscribe_needs_no_login_and_takes_effect_at_once() {
    let site = site();
    let gone = site.subscribe("gone@example.com").await;
    site.subscribe("stays@example.com").await;
    let postman = site.postman();
    site.open_incident("First", &["api"]).await;
    site.settled().await;

    let mail = site
        .sent
        .to("gone@example.com")
        .into_iter()
        .find(|sent| subject(sent).contains("First"))
        .unwrap();
    let header = mail.header("List-Unsubscribe").unwrap();
    let path = header
        .strip_prefix(&format!("<{BASE}"))
        .and_then(|rest| rest.strip_suffix('>'))
        .unwrap()
        .to_owned();
    assert_eq!(path, format!("/subscribe/unsubscribe/{gone}"));

    // Opening the link, as a scanner or prefetcher does, changes nothing.
    let (status, headers, page) = site.get(&path).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains(&format!("action=\"{path}\"")), "{page}");
    assert_eq!(headers["x-robots-tag"], "noindex");
    let id = Site::subscriber_id(&gone);
    assert!(site.state.store.subscriber(id).await.unwrap().is_some());

    // RFC 8058: the mail client posts with no origin, cookie or login.
    let (status, _, page) = site
        .post(&path, None, "List-Unsubscribe=One-Click".to_owned(), 99)
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains("You are unsubscribed"), "{page}");
    assert!(site.state.store.subscriber(id).await.unwrap().is_none());

    site.open_incident("Second", &["api"]).await;
    site.settled().await;
    postman.stop().await;
    assert!(
        site.sent
            .to("stays@example.com")
            .iter()
            .any(|sent| subject(sent).contains("Second"))
    );
    assert!(
        site.sent
            .to("gone@example.com")
            .iter()
            .all(|sent| !subject(sent).contains("Second"))
    );

    // The link works once; after that it is a 404, not an error.
    let (status, _, page) = site
        .post(&path, None, "List-Unsubscribe=One-Click".to_owned(), 99)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(page.contains("No subscription here"), "{page}");
}

#[tokio::test]
async fn unsubscribing_drops_mail_already_queued() {
    let site = site();
    let gone = site.subscribe("gone@example.com").await;
    site.subscribe("stays@example.com").await;
    site.open_incident("Queued", &["api"]).await;
    let queued = site
        .state
        .store
        .expand_announcements(tocsin_store::now_ms())
        .await
        .unwrap();
    assert_eq!(queued, 2);

    let (status, _, page) = site
        .post(
            &format!("/subscribe/unsubscribe/{gone}"),
            None,
            String::new(),
            99,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");

    let postman = site.postman();
    site.settled().await;
    postman.stop().await;
    assert!(
        site.sent
            .to("gone@example.com")
            .iter()
            .all(|sent| !subject(sent).contains("Queued"))
    );
    assert!(
        site.sent
            .to("stays@example.com")
            .iter()
            .any(|sent| subject(sent).contains("Queued"))
    );
}

#[tokio::test]
async fn sign_up_answers_the_same_whatever_the_address_state() {
    let site = site();
    let body = json!({"email": "someone@example.com"});
    let (new_status, _, new) = site.sign_up(body.clone(), site.fresh_peer()).await;
    site.confirmation("someone@example.com", 1).await;
    let (waiting_status, _, waiting) = site.sign_up(body.clone(), site.fresh_peer()).await;
    let confirm = site.confirmation("someone@example.com", 2).await;
    site.post(&confirm, Some(BASE), String::new(), site.fresh_peer())
        .await;
    let (confirmed_status, _, confirmed) = site.sign_up(body, site.fresh_peer()).await;
    assert_eq!(new_status, StatusCode::ACCEPTED);
    assert_eq!(waiting_status, new_status);
    assert_eq!(confirmed_status, new_status);
    assert_eq!(waiting, new);
    assert_eq!(confirmed, new);

    // Only the owner learns the difference: a confirmed address gets its
    // manage link, never a fresh confirmation.
    let sent = site
        .sent
        .wait_until("the third message", |sent| sent.len() >= 3)
        .await;
    assert!(subject(&sent[2]).starts_with("You are already subscribed"));
    assert!(!sent[2].body().contains("/subscribe/confirm/"));
    assert!(sent[2].body().contains("/subscribe/manage/"));

    // The form says the same for a new and a confirmed address.
    site.subscribe("member@example.com").await;
    let (new_form_status, _, new_page) = site
        .post(
            "/subscribe",
            Some(BASE),
            form(&[("email", "fresh@example.com"), ("components", "*")]),
            site.fresh_peer(),
        )
        .await;
    let (old_form_status, _, old_page) = site
        .post(
            "/subscribe",
            Some(BASE),
            form(&[("email", "member@example.com"), ("components", "*")]),
            site.fresh_peer(),
        )
        .await;
    assert_eq!(new_form_status, StatusCode::OK);
    assert_eq!(old_form_status, StatusCode::OK);
    assert_eq!(new_page, old_page);
    assert!(new_page.contains("Check your inbox"));
}

#[tokio::test]
async fn sign_ups_past_a_limit_are_429_with_retry_after() {
    let site = site();
    let peer = site.fresh_peer();
    for n in 0..5 {
        let (status, _, body) = site
            .sign_up(json!({"email": format!("person{n}@example.com")}), peer)
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    }
    let (status, headers, body) = site
        .sign_up(json!({"email": "person5@example.com"}), peer)
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    let wait: u64 = headers[header::RETRY_AFTER]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=3600).contains(&wait), "{wait}");
    assert!(
        body["message"].as_str().unwrap().contains("your address"),
        "{body}"
    );

    // The same address, from anywhere.
    for _ in 0..3 {
        let (status, _, body) = site
            .sign_up(json!({"email": "popular@example.com"}), site.fresh_peer())
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    }
    let (status, headers, body) = site
        .sign_up(json!({"email": "popular@example.com"}), site.fresh_peer())
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert!(headers.contains_key(header::RETRY_AFTER));
    assert!(
        body["message"].as_str().unwrap().contains("this address"),
        "{body}"
    );

    // The form refuses the same way, as a page.
    let (status, headers, page) = site
        .post(
            "/subscribe",
            Some(BASE),
            form(&[("email", "popular@example.com"), ("components", "*")]),
            site.fresh_peer(),
        )
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(headers.contains_key(header::RETRY_AFTER));
    assert!(page.contains("<html"), "{page}");
}

#[tokio::test]
async fn sign_up_refuses_bad_input_by_field() {
    let site = site();
    let (status, _, body) = site
        .sign_up(
            json!({"email": "not an address", "components": ["nope"], "projects": ["internal"]}),
            site.fresh_peer(),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let paths: Vec<&str> = body["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|field| field["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["email", "components[0]", "projects[0]"], "{body}");

    let (status, _, page) = site
        .post(
            "/subscribe",
            Some(BASE),
            form(&[("email", "someone@example.com")]),
            site.fresh_peer(),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        page.contains("Incidents: choose at least one component or project."),
        "{page}"
    );
    assert!(page.contains("value=\"someone@example.com\""), "{page}");
    assert_eq!(site.sent.sent().len(), 0);
}

#[tokio::test]
async fn subscribers_are_off_without_the_table() {
    let site = site_with("", NO_TURNSTILE);
    let (status, _, _) = site
        .sign_up(json!({"email": "someone@example.com"}), 1)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = site
        .api(Method::GET, "/api/v1/subscribers", Some(READER), None, 1)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    for uri in [
        "/subscribe",
        "/subscribe/manage/1.abc",
        "/admin/subscribers",
    ] {
        let (status, _, _) = site.get(uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
    let (status, _, page) = site.get("/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!page.contains("id=\"subscribe\""), "{page}");
    assert!(!page.contains("action=\"/subscribe\""), "{page}");
}

#[tokio::test]
async fn the_page_offers_a_plain_form_and_the_feeds() {
    let site = site();
    let (status, headers, page) = site.get("/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("<section id=\"subscribe\""), "{page}");
    assert!(
        page.contains("method=\"post\" action=\"/subscribe\""),
        "{page}"
    );
    assert!(page.contains("href=\"/feeds/incidents.atom\""), "{page}");
    assert!(page.contains("href=\"/feeds/releases.atom\""), "{page}");
    assert!(!page.contains("style=\""), "inline style");
    assert!(!page.contains("<script>"), "inline script");
    assert_eq!(
        headers[header::CONTENT_SECURITY_POLICY],
        crate::web::page::CONTENT_SECURITY_POLICY
    );

    let (status, headers, page) = site.get("/subscribe").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers[header::CONTENT_SECURITY_POLICY],
        crate::web::page::CONTENT_SECURITY_POLICY
    );
    for choice in ["value=\"api\"", "value=\"web\"", "API server"] {
        assert!(page.contains(choice), "{choice}: {page}");
    }
    assert!(
        !page.contains("Internal tool"),
        "a private project is offered"
    );
}

#[tokio::test]
async fn turnstile_guards_sign_up_when_on() {
    let (verify_url, verifier) = turnstile_server().await;
    let site = site_with(
        &format!(
            "{ON}[subscribers.turnstile]\nsite_key = \"0x4AAAAAAA\"\n\
             secret_key = \"${{ENV:TURNSTILE_SECRET}}\"\n"
        ),
        &verify_url,
    );

    let (_, _, page) = site.get("/").await;
    assert!(
        page.contains("class=\"button\" href=\"/subscribe\""),
        "{page}"
    );
    assert!(!page.contains("challenges.cloudflare.com"), "{page}");

    let (status, headers, page) = site.get("/subscribe").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("data-sitekey=\"0x4AAAAAAA\""), "{page}");
    assert!(page.contains(super::turnstile::SCRIPT_URL), "{page}");
    let policy = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
    assert!(
        policy.contains("script-src 'self' https://challenges.cloudflare.com;"),
        "{policy}"
    );
    assert!(
        policy.contains("frame-src https://challenges.cloudflare.com;"),
        "{policy}"
    );

    let (status, _, body) = site
        .sign_up(json!({"email": "someone@example.com"}), site.fresh_peer())
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, _, body) = site
        .sign_up(
            json!({"email": "someone@example.com", "turnstile": "wrong"}),
            site.fresh_peer(),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, _, body) = site
        .sign_up(
            json!({"email": "someone@example.com", "turnstile": "right"}),
            site.fresh_peer(),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(verifier.lock().unwrap().as_slice(), ["wrong", "right"]);
    assert!(site.sent.to("someone@example.com").len() <= 1);
}

/// A stand-in for Cloudflare's siteverify: `right` passes. Its URL and the
/// answers it was asked about.
async fn turnstile_server() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    use axum::routing::post;
    let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = Arc::clone(&asked);
    let app = Router::new().route(
        "/siteverify",
        post(move |body: String| {
            let seen = Arc::clone(&seen);
            async move {
                let fields: Vec<(String, String)> = url::form_urlencoded::parse(body.as_bytes())
                    .into_owned()
                    .collect();
                let field = |name: &str| {
                    fields
                        .iter()
                        .find(|(key, _)| key == name)
                        .map(|(_, value)| value.clone())
                        .unwrap_or_default()
                };
                assert_eq!(field("secret"), "turnstile-secret");
                let answer = field("response");
                let success = answer == "right";
                seen.lock().unwrap().push(answer);
                axum::Json(json!({"success": success}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}/siteverify"), asked)
}

#[tokio::test]
async fn links_are_refused_once_expired_or_guessed() {
    let site = site();
    let confirm = "expired-token-0123456789";
    site.state
        .store
        .sign_up(NewSignup {
            email: "late@example.com".into(),
            confirm_hash: token::digest(confirm),
            confirm_expires_at_ms: tocsin_store::now_ms() - 1,
            manage_secret: token::random().unwrap(),
            preferences: Preferences {
                components: vec!["*".into()],
                projects: Vec::new(),
            },
            now_ms: tocsin_store::now_ms() - 10_000,
        })
        .await
        .unwrap();
    let path = format!("/subscribe/confirm/{confirm}");
    let (status, _, page) = site.get(&path).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(page.contains("This link has expired"), "{page}");
    let (status, _, _) = site.post(&path, Some(BASE), String::new(), 1).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let counts = site.state.store.subscriber_counts().await.unwrap();
    assert_eq!((counts.confirmed, counts.unconfirmed), (0, 1));

    // A manage link with the right id and a wrong secret finds nothing.
    let manage = site.subscribe("owner@example.com").await;
    let (id, _) = token::parse_manage(&manage).unwrap();
    let guessed = token::manage(id, &token::random().unwrap());
    for path in [
        format!("/subscribe/manage/{guessed}"),
        format!("/subscribe/unsubscribe/{guessed}"),
    ] {
        let (status, _, page) = site.get(&path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert!(!page.contains("owner@example.com"), "{page}");
    }
    let (status, _, _) = site
        .post(
            &format!("/subscribe/unsubscribe/{guessed}"),
            None,
            String::new(),
            1,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(site.state.store.subscriber(id).await.unwrap().is_some());
}

#[tokio::test]
async fn forms_from_another_site_are_refused() {
    let site = site();
    let (status, _, _) = site
        .sign_up(json!({"email": "someone@example.com"}), site.fresh_peer())
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let confirm = site.confirmation("someone@example.com", 1).await;
    for (path, body) in [
        (
            "/subscribe".to_owned(),
            form(&[("email", "x@example.com"), ("components", "*")]),
        ),
        (confirm.clone(), String::new()),
    ] {
        for origin in [Some("https://evil.example.net"), None] {
            let (status, _, _) = site.post(&path, origin, body.clone(), 1).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{path} from {origin:?}");
        }
    }
    let counts = site.state.store.subscriber_counts().await.unwrap();
    assert_eq!(counts.confirmed, 0);
    // The real form still works.
    let (status, _, _) = site.post(&confirm, Some(BASE), String::new(), 1).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn preferences_choose_which_components_and_projects_send() {
    let site = site();
    let manage = site.subscribe("picky@example.com").await;
    let path = format!("/subscribe/manage/{manage}");

    let (status, headers, page) = site.get(&format!("{path}?done=confirmed")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-robots-tag"], "noindex");
    assert!(page.contains("picky@example.com"), "{page}");
    assert!(page.contains("role=\"status\""), "{page}");

    let (status, _, _) = site
        .post(
            &path,
            Some("https://evil.example.net"),
            form(&[("components", "web")]),
            1,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _, page) = site.post(&path, Some(BASE), String::new(), 1).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(page.contains("choose at least one"), "{page}");

    let (status, headers, page) = site
        .post(&path, Some(BASE), form(&[("components", "api")]), 1)
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
    assert_eq!(headers[header::LOCATION], format!("{path}?done=saved"));
    let (_, _, page) = site.get(&path).await;
    assert!(page.contains("value=\"api\" checked"), "{page}");
    assert!(!page.contains("value=\"web\" checked"), "{page}");

    let postman = site.postman();
    site.open_incident("Web only", &["web"]).await;
    site.open_incident("API down", &["api"]).await;
    site.publish("api", "v2.0.0").await;
    site.settled().await;
    postman.stop().await;
    let subjects: Vec<String> = site
        .sent
        .to("picky@example.com")
        .iter()
        .skip(1)
        .map(|sent| subject(sent).to_owned())
        .collect();
    assert_eq!(subjects.len(), 1, "{subjects:#?}");
    assert!(subjects[0].contains("API down"), "{subjects:#?}");
}

#[tokio::test]
async fn operators_list_count_and_remove_subscribers() {
    let site = site();
    let manage = site.subscribe("kept@example.com").await;
    let (status, _, _) = site
        .sign_up(json!({"email": "pending@example.com"}), site.fresh_peer())
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let (status, _, _) = site
        .api(Method::GET, "/api/v1/subscribers", None, None, 1)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = site
        .api(Method::GET, "/api/v1/subscribers", Some(READER), None, 1)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, list) = site
        .api(
            Method::GET,
            "/api/v1/subscribers?limit=1",
            Some(OPERATOR),
            None,
            1,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["totals"], json!({"confirmed": 1, "unconfirmed": 1}));
    assert_eq!(list["subscribers"][0]["email"], "kept@example.com");
    assert_eq!(list["subscribers"][0]["components"], json!(["*"]));
    let after = list["next_after"].as_i64().unwrap();
    let (_, _, rest) = site
        .api(
            Method::GET,
            &format!("/api/v1/subscribers?after={after}"),
            Some(OPERATOR),
            None,
            1,
        )
        .await;
    assert_eq!(rest["subscribers"][0]["email"], "pending@example.com");
    assert!(
        rest["subscribers"][0].get("confirmed_at").is_none(),
        "{rest}"
    );
    assert!(rest.get("next_after").is_none(), "{rest}");

    let id = Site::subscriber_id(&manage);
    let uri = format!("/api/v1/subscribers/{id}");
    let (status, _, _) = site.api(Method::DELETE, &uri, Some(READER), None, 1).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = site
        .api(Method::DELETE, &uri, Some(OPERATOR), None, 1)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = site
        .api(Method::DELETE, &uri, Some(OPERATOR), None, 1)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = site.get(&format!("/subscribe/manage/{manage}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_admin_page_lists_and_removes_subscribers() {
    let site = site();
    let manage = site.subscribe("kept@example.com").await;
    let id = Site::subscriber_id(&manage);
    let admin = |method: Method, uri: &str| {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "status.example.com")
            .header("x-forwarded-email", "alice@example.com");
        builder
            .extensions_mut()
            .unwrap()
            .insert(ConnectInfo(SocketAddr::from(([10, 1, 2, 3], 41_000))));
        builder
    };
    let (status, _, page) = site
        .call(
            admin(Method::GET, "/admin/subscribers")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains("kept@example.com"), "{page}");
    let (_, _, index) = site
        .call(admin(Method::GET, "/admin/").body(Body::empty()).unwrap())
        .await;
    assert!(index.contains("href=\"/admin/subscribers\""), "{index}");

    let start = page.find("name=\"csrf\" value=\"").unwrap() + "name=\"csrf\" value=\"".len();
    let csrf = &page[start..][..page[start..].find('"').unwrap()];
    let (status, headers, _) = site
        .call(
            admin(Method::POST, &format!("/admin/subscribers/{id}/remove"))
                .header(header::ORIGIN, BASE)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(form(&[("csrf", csrf)])))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "/admin/subscribers");
    assert!(site.state.store.subscriber(id).await.unwrap().is_none());
}

#[tokio::test]
async fn every_subscription_page_passes_the_structural_audit() {
    let site = site();
    let manage = site.subscribe("audited@example.com").await;
    let (status, _, _) = site
        .sign_up(json!({"email": "waiting@example.com"}), site.fresh_peer())
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let confirm = site.confirmation("waiting@example.com", 1).await;
    let mut pages = Vec::new();
    for uri in [
        "/".to_owned(),
        "/subscribe".to_owned(),
        confirm,
        format!("/subscribe/manage/{manage}?done=confirmed"),
        format!("/subscribe/unsubscribe/{manage}"),
        "/subscribe/confirm/unknown".to_owned(),
    ] {
        let (_, _, page) = site.get(&uri).await;
        pages.push((uri, page));
    }
    let (_, _, invalid) = site
        .post("/subscribe", Some(BASE), form(&[("email", "nope")]), 77)
        .await;
    pages.push(("POST /subscribe, invalid".to_owned(), invalid));
    let (_, _, accepted) = site
        .post(
            "/subscribe",
            Some(BASE),
            form(&[("email", "new@example.com"), ("components", "*")]),
            78,
        )
        .await;
    pages.push(("POST /subscribe".to_owned(), accepted));
    let (_, _, left) = site
        .post(
            &format!("/subscribe/unsubscribe/{manage}"),
            None,
            String::new(),
            79,
        )
        .await;
    pages.push(("POST unsubscribe".to_owned(), left));
    for (uri, page) in pages {
        assert_eq!(
            crate::web::status_tests::audit(&page),
            Vec::<String>::new(),
            "{uri}"
        );
    }
}
