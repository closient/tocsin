// SPDX-License-Identifier: AGPL-3.0-only

//! Slack channels connected by signing in, end to end: the admin page and
//! "Add to Slack" through the router, a fake Slack standing in for the
//! consent screen, `oauth.v2.access` and the incoming webhooks, and the
//! notifier and postman posting to what was connected.
//!
//! The fake hands out webhook URLs holding [`MARKER`], so a leak anywhere
//! is a plain substring search.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use serde_json::{Value, json};
use tocsin_alert::{Alert, AlertEvent, SendError, SlackHooks};
use tocsin_core::Health;
use tocsin_core::config::Config;
use tocsin_store::{SlackPurpose, SubjectKind};

use super::{BASE, NO_TURNSTILE, ON, OPERATOR, READER, Site, config, form, site_from};
use crate::slack::{COOKIE, SlackApp};
use crate::testing::Captured;

pub(super) const CLIENT_ID: &str = "1111.2222";
pub(super) const CLIENT_SECRET: &str = "acme-client-secret-0123456789";
/// In every webhook URL and token the fake Slack hands out, and so in
/// nothing tocsin shows.
const MARKER: &str = "LEAKMARKER";
const SLACK: &str = "[slack]\nclient_id = \"${ENV:SLACK_CLIENT_ID}\"\n\
                     client_secret = \"${ENV:SLACK_CLIENT_SECRET}\"\n\n";
/// A notifier posting to the connection `ops-alerts`.
const ALERTS: &str = "[[notifier]]\nid = \"ops-slack\"\ntype = \"slack\"\n\n\
                      [notifier.slack]\nconnection = \"ops-alerts\"\n\n";
/// A notifier naming a connection nobody connects.
const NEVER: &str = "[[notifier]]\nid = \"lost-slack\"\ntype = \"slack\"\n\n\
                     [notifier.slack]\nconnection = \"never-connected\"\n\n";

/// What the fake Slack has seen and handed out.
#[derive(Default)]
struct Seen {
    /// Codes issued and not yet exchanged, with the redirect URI each was
    /// issued for. Slack's codes work once, and so do these.
    codes: HashMap<String, String>,
    /// Webhooks and codes issued so far.
    issued: usize,
    /// The channel the next person picks on the consent screen.
    channel: String,
    /// Calls to `oauth.v2.access`, accepted or not.
    exchanges: usize,
    /// Each post: the webhook's path under `/hooks/`, and the message.
    posts: Vec<(String, Value)>,
    /// Webhooks Slack has revoked, which answer `404 channel_not_found`.
    revoked: HashSet<String>,
}

/// A stand-in for Slack, checking what tocsin sends it against the app
/// manifest `tocsin slack setup` prints.
struct Fake {
    url: String,
    seen: Arc<Mutex<Seen>>,
}

/// What the fake's handlers share: the app as the manifest describes it,
/// and what they have seen.
struct Slack {
    url: String,
    redirect_urls: Vec<String>,
    scopes: Vec<String>,
    seen: Arc<Mutex<Seen>>,
}

/// The consent screen: Slack checks the app and its redirect URL, the
/// person approves, and Slack sends them back with a code.
async fn authorize(
    State(slack): State<Arc<Slack>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let field = |name: &str| query.get(name).cloned().unwrap_or_default();
    if field("client_id") != CLIENT_ID {
        return (StatusCode::BAD_REQUEST, "unknown client_id").into_response();
    }
    if field("scope")
        .split(',')
        .any(|scope| !slack.scopes.contains(&scope.to_owned()))
    {
        return (StatusCode::BAD_REQUEST, "scope not in the manifest").into_response();
    }
    let redirect_uri = field("redirect_uri");
    if !slack.redirect_urls.contains(&redirect_uri) {
        return (StatusCode::BAD_REQUEST, "redirect_uri not in the manifest").into_response();
    }
    let mut seen = slack.seen.lock().unwrap();
    seen.issued += 1;
    let code = format!("code-{}", seen.issued);
    seen.codes.insert(code.clone(), redirect_uri.clone());
    let back = url::Url::parse_with_params(
        &redirect_uri,
        [("code", code.as_str()), ("state", &field("state"))],
    )
    .unwrap();
    Redirect::to(back.as_str()).into_response()
}

/// `oauth.v2.access`: a code, once, for a webhook.
async fn access(State(slack): State<Arc<Slack>>, headers: HeaderMap, body: String) -> Json<Value> {
    let mut seen = slack.seen.lock().unwrap();
    seen.exchanges += 1;
    let basic =
        base64::engine::general_purpose::STANDARD.encode(format!("{CLIENT_ID}:{CLIENT_SECRET}"));
    if headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        != Some(&format!("Basic {basic}"))
    {
        return Json(json!({"ok": false, "error": "invalid_client"}));
    }
    let fields: HashMap<String, String> = url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect();
    let code = fields.get("code").cloned().unwrap_or_default();
    match seen.codes.remove(&code) {
        Some(redirect_uri) if fields.get("redirect_uri") == Some(&redirect_uri) => {}
        Some(_) => return Json(json!({"ok": false, "error": "bad_redirect_uri"})),
        None => return Json(json!({"ok": false, "error": "invalid_code"})),
    }
    let n = seen.issued;
    Json(json!({
        "ok": true,
        "app_id": "A0ACME",
        "access_token": format!("xoxb-{MARKER}-token-{n}"),
        "scope": "incoming-webhook",
        "team": {"id": "T0ACME", "name": "Acme"},
        "incoming_webhook": {
            "channel": seen.channel,
            "channel_id": format!("C0{n}"),
            "configuration_url": format!("https://acme.example/services/B0{n}"),
            "url": format!("{}/hooks/{MARKER}/{n}", slack.url),
        },
    }))
}

/// An incoming webhook: takes the post, unless the webhook is revoked.
async fn hook(
    State(slack): State<Arc<Slack>>,
    Path(hook): Path<String>,
    Json(message): Json<Value>,
) -> (StatusCode, &'static str) {
    let mut seen = slack.seen.lock().unwrap();
    seen.posts.push((hook.clone(), message));
    if seen.revoked.contains(&hook) {
        (StatusCode::NOT_FOUND, "channel_not_found")
    } else {
        (StatusCode::OK, "ok")
    }
}

impl Fake {
    async fn start(manifest: &Value) -> Fake {
        assert!(
            manifest["settings"]["incoming_webhooks"]["incoming_webhooks_enabled"]
                .as_bool()
                .unwrap()
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Seen {
            channel: "#ops-alerts".to_owned(),
            ..Seen::default()
        }));
        let slack = Arc::new(Slack {
            url: url.clone(),
            redirect_urls: serde_json::from_value(
                manifest["oauth_config"]["redirect_urls"].clone(),
            )
            .unwrap(),
            scopes: serde_json::from_value(manifest["oauth_config"]["scopes"]["bot"].clone())
                .unwrap(),
            seen: Arc::clone(&seen),
        });
        let app = axum::Router::new()
            .route("/oauth/v2/authorize", get(authorize))
            .route("/api/oauth.v2.access", post(access))
            .route("/hooks/{*hook}", post(hook))
            .with_state(slack);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Fake { url, seen }
    }

    /// The app that sends people to this fake and exchanges codes with it.
    fn app(&self, config: &Config) -> Arc<SlackApp> {
        Arc::new(
            SlackApp::with_urls(
                config,
                &format!("{}/oauth/v2/authorize", self.url),
                &format!("{}/api/oauth.v2.access", self.url),
            )
            .unwrap(),
        )
    }

    fn pick(&self, channel: &str) {
        channel.clone_into(&mut self.seen.lock().unwrap().channel);
    }

    fn exchanges(&self) -> usize {
        self.seen.lock().unwrap().exchanges
    }

    /// Every message posted to the `n`th webhook handed out.
    fn posts_to(&self, n: usize) -> Vec<Value> {
        let hook = format!("{MARKER}/{n}");
        self.seen
            .lock()
            .unwrap()
            .posts
            .iter()
            .filter(|(to, _)| *to == hook)
            .map(|(_, message)| message.clone())
            .collect()
    }

    /// Waits until the `n`th webhook has had `count` messages.
    async fn wait_for(&self, n: usize, count: usize) -> Vec<Value> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let posts = self.posts_to(n);
            if posts.len() >= count {
                return posts;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "webhook {n} has {} of {count} messages",
                posts.len()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Revokes the `n`th webhook, as archiving its channel does.
    fn revoke(&self, n: usize) {
        self.seen
            .lock()
            .unwrap()
            .revoked
            .insert(format!("{MARKER}/{n}"));
    }
}

/// A site with `[subscribers]`, `[slack]` and `extra`, and the fake Slack
/// its app talks to.
async fn slack_site(extra: &str) -> (Site, Fake) {
    let config = config(&format!("{ON}{SLACK}{extra}"));
    let fake = Fake::start(&crate::slack::manifest(&config).unwrap()).await;
    let app = fake.app(&config);
    (site_from(config, NO_TURNSTILE, Some(app)), fake)
}

/// A request from an admin, signed in through the trusted proxy.
fn admin(method: Method, uri: &str) -> axum::http::request::Builder {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "status.example.com")
        .header("x-forwarded-email", "alice@acme.example");
    builder
        .extensions_mut()
        .unwrap()
        .insert(ConnectInfo(std::net::SocketAddr::from((
            [10, 1, 2, 3],
            41_000,
        ))));
    builder
}

/// The form's CSRF token on `page`.
fn csrf(page: &str) -> String {
    let start = page.find("name=\"csrf\" value=\"").unwrap() + "name=\"csrf\" value=\"".len();
    page[start..][..page[start..].find('"').unwrap()].to_owned()
}

/// A sign-in on its way back from Slack: the callback's path and query,
/// and the cookie the browser holds.
struct Back {
    path: String,
    cookie: String,
}

impl Site {
    async fn admin_get(&self, uri: &str) -> (StatusCode, HeaderMap, String) {
        self.call(admin(Method::GET, uri).body(Body::empty()).unwrap())
            .await
    }

    async fn admin_post(
        &self,
        uri: &str,
        fields: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap, String) {
        let (_, _, page) = self.admin_get("/admin/slack").await;
        let token = csrf(&page);
        let mut fields = fields.to_vec();
        fields.push(("csrf", &token));
        self.call(
            admin(Method::POST, uri)
                .header(header::ORIGIN, BASE)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(form(&fields)))
                .unwrap(),
        )
        .await
    }

    /// `GET /metrics`, as Prometheus scrapes it.
    async fn metrics(&self) -> String {
        let (status, _, text) = self
            .call(
                super::request(Method::GET, "/metrics", 1)
                    .header(header::AUTHORIZATION, format!("Bearer {READER}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        text
    }

    /// The callback, from a browser holding `cookie`.
    async fn callback(&self, path: &str, cookie: Option<&str>) -> (StatusCode, HeaderMap, String) {
        let mut builder = super::request(Method::GET, path, 1);
        if let Some(cookie) = cookie {
            builder = builder.header(header::COOKIE, cookie);
        }
        self.call(builder.body(Body::empty()).unwrap()).await
    }

    /// An admin starts connecting `name`: the redirect to Slack.
    async fn start_operator(&self, name: &str) -> HeaderMap {
        let (status, headers, page) = self
            .admin_post("/admin/slack/connect", &[("name", name)])
            .await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
        headers
    }

    /// A visitor presses "Add to Slack" with `fields` chosen: the redirect
    /// to Slack.
    async fn start_subscriber(&self, fields: &[(&str, &str)]) -> HeaderMap {
        let (status, headers, page) = self
            .post(
                "/subscribe/slack",
                Some(BASE),
                form(fields),
                self.fresh_peer(),
            )
            .await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
        headers
    }

    /// Connects `name` for alerts, start to finish.
    async fn connect_operator(&self, fake: &Fake, name: &str) {
        let back = approve(fake, &self.start_operator(name).await).await;
        let (status, headers, page) = self.callback(&back.path, Some(&back.cookie)).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
        assert_eq!(headers[header::LOCATION], "/admin/slack?done=connected");
    }

    /// Adds a subscriber channel choosing `fields`, start to finish; its
    /// manage token.
    async fn connect_subscriber(&self, fake: &Fake, fields: &[(&str, &str)]) -> String {
        let back = approve(fake, &self.start_subscriber(fields).await).await;
        let (status, headers, page) = self.callback(&back.path, Some(&back.cookie)).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
        let location = headers[header::LOCATION].to_str().unwrap();
        location
            .strip_prefix("/subscribe/slack/manage/")
            .and_then(|rest| rest.strip_suffix("?done=connected"))
            .unwrap_or_else(|| panic!("connected to {location}"))
            .to_owned()
    }
}

/// Follows the redirect in `started` as the browser would: to the fake
/// Slack's consent screen, which approves at once, and back towards the
/// callback.
async fn approve(fake: &Fake, started: &HeaderMap) -> Back {
    let location = started[header::LOCATION].to_str().unwrap();
    assert!(location.starts_with(&fake.url), "{location}");
    let cookie = started
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|value| value.to_str().unwrap())
        .find(|value| value.starts_with(&format!("{COOKIE}=")))
        .expect("the sign-in sets its cookie");
    for attribute in ["Secure", "HttpOnly", "SameSite=Lax", "Path=/"] {
        assert!(cookie.contains(attribute), "{cookie}");
    }
    let cookie = cookie.split(';').next().unwrap().to_owned();
    let browser = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let consent = browser.get(location).send().await.unwrap();
    let status = consent.status();
    let back = consent
        .headers()
        .get(header::LOCATION)
        .map(|value| value.to_str().unwrap().to_owned());
    let Some(back) = back else {
        panic!("Slack refused: {status} {}", consent.text().await.unwrap());
    };
    let path = back
        .strip_prefix(BASE)
        .unwrap_or_else(|| panic!("Slack sent the browser to {back}"))
        .to_owned();
    assert!(path.starts_with("/slack/callback?"), "{path}");
    Back { path, cookie }
}

/// The `state` in a callback path.
fn state_of(path: &str) -> String {
    url::Url::parse(&format!("{BASE}{path}"))
        .unwrap()
        .query_pairs()
        .find(|(key, _)| key == "state")
        .unwrap()
        .1
        .into_owned()
}

/// An alert about check `api-health`, firing or resolving.
fn alert(event: AlertEvent) -> Alert {
    let now = tocsin_store::now_ms();
    Alert {
        event,
        node_id: "node-1".to_owned(),
        subject_kind: SubjectKind::Check,
        subject_id: "api-health".to_owned(),
        subject_name: "API health".to_owned(),
        labels: BTreeMap::new(),
        annotations: BTreeMap::new(),
        component_id: Some("api".to_owned()),
        component_name: Some("API".to_owned()),
        health: Health::Down,
        started_at_ms: now - 60_000,
        ended_at_ms: (event == AlertEvent::Resolve).then_some(now),
        ended_by: (event == AlertEvent::Resolve).then_some(tocsin_store::EndedBy::Recovery),
        sent_at_ms: now,
        delivery_id: None,
        late: None,
        test: false,
    }
}

/// The notifier `id`, built as `tocsin serve` builds it, with the store
/// holding the connections.
fn notifier_for(site: &Site, id: &str) -> Arc<dyn tocsin_alert::Notifier> {
    let config = &site.state.config;
    let notifier = config
        .notifiers
        .iter()
        .find(|notifier| notifier.id.as_str() == id)
        .unwrap();
    let hooks: Arc<dyn SlackHooks> = Arc::clone(&site.state.store) as _;
    tocsin_alert::build(
        notifier,
        config.server.email.as_ref(),
        &crate::net::http_client(Duration::from_secs(5))
            .build()
            .unwrap(),
        Some(&hooks),
    )
    .unwrap()
}

/// The text of a Slack message: its fallback and every block's.
fn text_of(message: &Value) -> String {
    message.to_string()
}

fn cleared(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::SET_COOKIE)
        .iter()
        .any(|value| value.to_str().unwrap() == crate::slack::clear_cookie())
}

#[tokio::test]
async fn an_admin_connects_a_channel_and_its_notifier_posts_fire_and_resolve() {
    let (site, fake) = slack_site(ALERTS).await;
    let up = "tocsin_slack_connection_up{notifier=\"ops-slack\",connection=\"ops-alerts\"}";

    let (status, _, page) = site.admin_get("/admin/slack").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(
        page.contains("Named by a notifier but not connected"),
        "{page}"
    );
    assert!(
        page.contains("<code>ops-alerts</code>, named by <code>ops-slack</code>"),
        "{page}"
    );
    assert!(site.metrics().await.contains(&format!("{up} 0")));
    let (_, _, index) = site.admin_get("/admin/").await;
    assert!(index.contains("href=\"/admin/slack\""), "{index}");

    // The redirect asks for exactly what the manifest grants.
    let started = site.start_operator("ops-alerts").await;
    let location = url::Url::parse(started[header::LOCATION].to_str().unwrap()).unwrap();
    let query: HashMap<_, _> = location.query_pairs().into_owned().collect();
    assert_eq!(query["client_id"], CLIENT_ID);
    assert_eq!(query["scope"], "incoming-webhook");
    assert_eq!(query["redirect_uri"], format!("{BASE}/slack/callback"));
    let back = approve(&fake, &started).await;
    let (status, headers, page) = site.callback(&back.path, Some(&back.cookie)).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
    assert_eq!(headers[header::LOCATION], "/admin/slack?done=connected");
    assert!(cleared(&headers));

    let (_, _, page) = site.admin_get("/admin/slack?done=connected").await;
    assert!(page.contains("Connected."), "{page}");
    assert!(
        !page.contains("Named by a notifier but not connected"),
        "{page}"
    );
    assert!(page.contains("ops-alerts"), "{page}");
    assert!(page.contains("badge resolved\">live"), "{page}");
    assert!(page.contains("#ops-alerts in Acme"), "{page}");
    assert!(page.contains("used by ops-slack"), "{page}");
    assert!(page.contains("by alice@acme.example"), "{page}");
    let metrics = site.metrics().await;
    assert!(metrics.contains(&format!("{up} 1")), "{metrics}");
    assert!(
        metrics.contains("tocsin_slack_connections{purpose=\"operator\",state=\"live\"} 1"),
        "{metrics}"
    );

    let notifier = notifier_for(&site, "ops-slack");
    notifier.send(&alert(AlertEvent::Fire)).await.unwrap();
    notifier.send(&alert(AlertEvent::Resolve)).await.unwrap();
    let posts = fake.posts_to(1);
    assert_eq!(posts.len(), 2, "{posts:#?}");
    assert!(
        posts[0]["text"]
            .as_str()
            .unwrap()
            .starts_with(":rotating_light:"),
        "{posts:#?}"
    );
    assert!(text_of(&posts[0]).contains("API health"), "{posts:#?}");
    assert!(
        posts[1]["text"]
            .as_str()
            .unwrap()
            .starts_with(":white_check_mark:"),
        "{posts:#?}"
    );
    assert!(text_of(&posts[1]).contains("API health"), "{posts:#?}");

    // Connecting the name again replaces its channel; the notifier follows.
    fake.pick("#ops-oncall");
    site.connect_operator(&fake, "ops-alerts").await;
    notifier.send(&alert(AlertEvent::Fire)).await.unwrap();
    assert_eq!(fake.posts_to(1).len(), 2);
    assert_eq!(fake.posts_to(2).len(), 1);
    let (_, _, page) = site.admin_get("/admin/slack").await;
    assert!(page.contains("#ops-oncall in Acme"), "{page}");
    assert!(!page.contains("#ops-alerts in Acme"), "{page}");

    // Removed, the notifier fails loudly rather than skipping.
    let id = site
        .state
        .store
        .slack_connections(SlackPurpose::Operator)
        .await
        .unwrap()[0]
        .id;
    let (status, headers, _) = site
        .admin_post(&format!("/admin/slack/{id}/remove"), &[])
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "/admin/slack?done=removed");
    let error = notifier.send(&alert(AlertEvent::Fire)).await.unwrap_err();
    assert!(error.is_permanent(), "{error:?}");
    assert!(
        error
            .to_string()
            .contains("no Slack connection `ops-alerts`"),
        "{error}"
    );
    assert!(site.metrics().await.contains(&format!("{up} 0")));
}

#[tokio::test]
async fn a_connection_name_must_be_an_id() {
    let (site, fake) = slack_site("").await;
    let (status, _, page) = site
        .admin_post("/admin/slack/connect", &[("name", "Ops Alerts!")])
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{page}");
    assert!(page.contains("Not connected:"), "{page}");
    assert!(page.contains("value=\"Ops Alerts!\""), "{page}");
    // A post without the page's CSRF token starts nothing.
    let (status, _, _) = site
        .call(
            admin(Method::POST, "/admin/slack/connect")
                .header(header::ORIGIN, BASE)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(form(&[("name", "ops-alerts")])))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(fake.exchanges(), 0);
}

#[tokio::test]
async fn a_visitor_adds_slack_and_hears_incidents_and_releases() {
    let (site, fake) = slack_site("").await;
    let (_, _, page) = site.get("/subscribe").await;
    assert!(
        page.contains("formaction=\"/subscribe/slack\" formnovalidate>Add to Slack</button>"),
        "{page}"
    );

    fake.pick("#acme-status");
    let manage = site
        .connect_subscriber(&fake, &[("components", "api"), ("projects", "api")])
        .await;
    // No confirmation step: approving on Slack's screen is the opt-in.
    let welcome = fake.wait_for(1, 1).await;
    let welcome = text_of(&welcome[0]);
    assert!(welcome.contains("This channel now hears from"), "{welcome}");
    assert!(
        welcome.contains(&format!(
            "<{BASE}/subscribe/slack/unsubscribe/{manage}|Remove this channel>"
        )),
        "{welcome}"
    );
    assert!(
        welcome.contains(&format!(
            "<{BASE}/subscribe/slack/manage/{manage}|Choose what this channel hears>"
        )),
        "{welcome}"
    );
    assert!(
        welcome.contains("<mailto:support@example.com|support@example.com>"),
        "{welcome}"
    );

    let (status, _, page) = site
        .get(&format!("/subscribe/slack/manage/{manage}?done=connected"))
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains("#acme-status"), "{page}");
    assert!(page.contains("Acme"), "{page}");

    let postman = site.postman();
    let id = site.open_incident("API errors", &["api"]).await;
    site.update_incident(id, "Found the cause.", true).await;
    site.open_incident("Web slow", &["web"]).await;
    site.publish("api", "v1.0.0").await;
    site.publish("internal", "v9.0.0").await;
    site.settled().await;

    let posts = fake.posts_to(1);
    let texts: Vec<String> = posts.iter().map(text_of).collect();
    assert_eq!(texts.len(), 4, "{texts:#?}");
    assert!(texts[1].contains("API errors"), "{texts:#?}");
    assert!(
        texts[1].contains(&format!("{BASE}/incidents/{id}")),
        "{texts:#?}"
    );
    assert!(texts[2].contains("Found the cause."), "{texts:#?}");
    assert!(
        texts[2].contains(&format!("{BASE}/incidents/{id}")),
        "{texts:#?}"
    );
    assert!(texts[3].contains("v1.0.0"), "{texts:#?}");
    assert!(
        texts[3].contains(&format!("{BASE}/releases/api/v1.0.0")),
        "{texts:#?}"
    );
    for text in &texts {
        assert!(
            text.contains(&format!("{BASE}/subscribe/slack/unsubscribe/{manage}")),
            "{text}"
        );
        assert!(
            !text.contains("Web slow") && !text.contains("v9.0.0"),
            "{text}"
        );
    }
    for post in &posts {
        assert_eq!(post["unfurl_links"], false);
    }

    // Removing it from the link in any message ends it at once.
    let (status, _, page) = site
        .get(&format!("/subscribe/slack/unsubscribe/{manage}"))
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let (status, _, page) = site
        .post(
            &format!("/subscribe/slack/unsubscribe/{manage}"),
            Some(BASE),
            String::new(),
            9,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    site.publish("api", "v1.0.1").await;
    site.settled().await;
    postman.stop().await;
    assert_eq!(fake.posts_to(1).len(), 4);
    let left = site
        .state
        .store
        .slack_connections(SlackPurpose::Subscriber)
        .await
        .unwrap();
    assert_eq!(left.len(), 0, "{left:?}");
}

#[tokio::test]
async fn forged_replayed_and_misdirected_callbacks_are_refused() {
    let (site, fake) = slack_site("").await;

    // A state this server never issued.
    let (status, headers, page) = site
        .callback(
            "/slack/callback?code=code-1&state=made-up",
            Some(&format!("{COOKIE}=made-up")),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{page}");
    assert!(cleared(&headers));

    // A real state, carried to a browser without the cookie, or with
    // another one: refused, and used up, so the right browser can't
    // finish it after.
    let back = approve(&fake, &site.start_operator("ops-alerts").await).await;
    let (status, headers, page) = site.callback(&back.path, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{page}");
    assert!(page.contains("started in another browser"), "{page}");
    assert!(cleared(&headers));
    let (status, _, _) = site.callback(&back.path, Some(&back.cookie)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let back = approve(&fake, &site.start_operator("ops-alerts").await).await;
    let (status, _, _) = site
        .callback(&back.path, Some(&format!("{COOKIE}=someone-else")))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // The same state with a forged code: still one use.
    let back = approve(&fake, &site.start_operator("ops-alerts").await).await;
    let forged = format!("/slack/callback?code=forged&state={}", state_of(&back.path));
    let (status, _, page) = site.callback(&forged, Some(&back.cookie)).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{page}");
    assert!(
        page.contains("Slack refused the sign-in: invalid_code"),
        "{page}"
    );
    let (status, _, _) = site.callback(&back.path, Some(&back.cookie)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(fake.exchanges(), 1);

    // Cancelled on Slack's screen: nothing added, and the state is spent.
    let back = approve(&fake, &site.start_operator("ops-alerts").await).await;
    let state = state_of(&back.path);
    let (status, _, page) = site
        .callback(
            &format!("/slack/callback?error=access_denied&state={state}"),
            Some(&back.cookie),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.contains("You cancelled on Slack"), "{page}");
    let (status, _, _) = site.callback(&back.path, Some(&back.cookie)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(fake.exchanges(), 1);

    // A real sign-in works once; its replay is refused.
    let back = approve(&fake, &site.start_operator("ops-alerts").await).await;
    let (status, _, page) = site.callback(&back.path, Some(&back.cookie)).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{page}");
    let (status, _, page) = site.callback(&back.path, Some(&back.cookie)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{page}");
    assert!(page.contains("expired or was already used"), "{page}");
    assert_eq!(fake.exchanges(), 2);
    let connections = site
        .state
        .store
        .slack_connections(SlackPurpose::Operator)
        .await
        .unwrap();
    assert_eq!(connections.len(), 1);
}

#[tokio::test]
async fn a_revoked_channel_is_marked_dead_in_metrics_and_on_the_admin_page() {
    let log = Captured::default();
    let _log = log.capture();
    let (site, fake) = slack_site(&format!("{ALERTS}{NEVER}")).await;
    site.connect_operator(&fake, "ops-alerts").await;
    let manage = site.connect_subscriber(&fake, &[("projects", "api")]).await;
    fake.wait_for(2, 1).await;
    fake.revoke(1);
    fake.revoke(2);

    let notifier = notifier_for(&site, "ops-slack");
    let error = notifier.send(&alert(AlertEvent::Fire)).await.unwrap_err();
    assert!(error.is_permanent(), "{error:?}");
    assert!(error.to_string().contains("channel_not_found"), "{error}");
    // Dead: nothing is posted to it again, and every alert fails loudly.
    let error = notifier
        .send(&alert(AlertEvent::Resolve))
        .await
        .unwrap_err();
    assert!(error.is_permanent(), "{error:?}");
    assert!(error.to_string().contains("is dead"), "{error}");
    assert_eq!(fake.posts_to(1).len(), 1);

    // A name never connected fails the same way.
    let error = notifier_for(&site, "lost-slack")
        .send(&alert(AlertEvent::Fire))
        .await
        .unwrap_err();
    assert!(error.is_permanent(), "{error:?}");

    let postman = site.postman();
    site.publish("api", "v2.0.0").await;
    site.settled().await;
    site.publish("api", "v2.0.1").await;
    site.settled().await;
    postman.stop().await;
    assert_eq!(fake.posts_to(2).len(), 2, "one welcome, one refused post");
    let deliveries = column(&site, "SELECT status FROM slack_delivery ORDER BY id").await;
    assert_eq!(deliveries, ["failed"]);

    let metrics = site.metrics().await;
    for line in [
        "tocsin_slack_connections{purpose=\"operator\",state=\"dead\"} 1",
        "tocsin_slack_connections{purpose=\"operator\",state=\"live\"} 0",
        "tocsin_slack_connections{purpose=\"subscriber\",state=\"dead\"} 1",
        "tocsin_slack_connection_up{notifier=\"ops-slack\",connection=\"ops-alerts\"} 0",
        "tocsin_slack_connection_up{notifier=\"lost-slack\",connection=\"never-connected\"} 0",
    ] {
        assert!(metrics.contains(line), "{line} not in {metrics}");
    }

    let (_, _, page) = site.admin_get("/admin/slack").await;
    assert!(page.contains("badge down\">dead"), "{page}");
    assert!(!page.contains("badge resolved\">live"), "{page}");
    assert!(
        page.contains("Slack stopped accepting posts: channel_not_found"),
        "{page}"
    );
    assert!(
        page.contains("<code>never-connected</code>, named by <code>lost-slack</code>"),
        "{page}"
    );
    let (_, _, page) = site.get(&format!("/subscribe/slack/manage/{manage}")).await;
    assert!(page.contains("channel_not_found"), "{page}");

    let log = log.text();
    assert!(
        log.contains("Slack revoked the connection; marking it dead"),
        "{log}"
    );
    assert!(
        log.contains("Slack connection is dead; reconnect it on the admin page"),
        "{log}"
    );
    assert!(log.contains("no Slack connection by this name"), "{log}");
    assert!(
        log.contains("Slack revoked a subscriber's channel; marking it dead"),
        "{log}"
    );
}

/// The one column `sql` selects, every row.
async fn column(site: &Site, sql: &'static str) -> Vec<String> {
    site.state
        .store
        .read(move |conn| {
            let mut statement = conn.prepare(sql)?;
            let rows = statement
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
        .unwrap()
}

/// Every page and API answer that could show a connection, by where.
async fn served(site: &Site, id: i64, manage: &str) -> Vec<(String, String)> {
    let mut shown: Vec<(String, String)> = Vec::new();
    for uri in [
        "/".to_owned(),
        "/subscribe".to_owned(),
        format!("/incidents/{id}"),
        format!("/subscribe/slack/manage/{manage}"),
        format!("/subscribe/slack/unsubscribe/{manage}"),
    ] {
        let (_, _, page) = site.get(&uri).await;
        shown.push((uri, page));
    }
    for uri in ["/admin/", "/admin/slack", "/admin/subscribers"] {
        let (_, _, page) = site.admin_get(uri).await;
        shown.push((uri.to_owned(), page));
    }
    for (uri, token) in [
        ("/api/v1/subscribers", OPERATOR),
        ("/api/v1/config/status", READER),
        ("/api/v1/checks", READER),
        ("/api/v1/outages", READER),
        ("/api/v1/releases", READER),
        ("/api/v1/incidents", READER),
        ("/api/v1/timeline", READER),
    ] {
        let (status, _, body) = site.api(Method::GET, uri, Some(token), None, 1).await;
        assert!(status.is_success(), "{uri}: {status} {body}");
        shown.push((uri.to_owned(), body.to_string()));
    }
    shown
}

#[tokio::test]
async fn webhook_urls_never_leave_the_store() {
    let log = Captured::default();
    let _log = log.capture();
    let (site, fake) = slack_site(ALERTS).await;
    site.connect_operator(&fake, "ops-alerts").await;
    let manage = site
        .connect_subscriber(&fake, &[("components", "*"), ("projects", "*")])
        .await;
    fake.wait_for(2, 1).await;
    let postman = site.postman();
    let id = site.open_incident("API errors", &["api"]).await;
    site.publish("api", "v1.0.0").await;
    site.settled().await;
    fake.revoke(2);
    site.publish("api", "v1.0.1").await;
    site.settled().await;
    postman.stop().await;
    let notifier = notifier_for(&site, "ops-slack");
    notifier.send(&alert(AlertEvent::Fire)).await.unwrap();
    fake.revoke(1);
    let failed = notifier
        .send(&alert(AlertEvent::Resolve))
        .await
        .unwrap_err();

    // The secret is there to leak: the store holds it.
    let stored = column(&site, "SELECT webhook_url FROM slack_connection").await;
    assert_eq!(stored.len(), 2);
    assert!(stored.iter().all(|url| url.contains(MARKER)), "{stored:?}");

    let mut shown = served(&site, id, &manage).await;
    shown.push(("/metrics".to_owned(), site.metrics().await));
    let incident = site.state.store.incident(id).await.unwrap().unwrap();
    shown.push(("export".to_owned(), crate::export::markdown(&incident)));
    shown.push(("Debug".to_owned(), format!("{:?}", site.state.slack)));
    for purpose in [SlackPurpose::Operator, SlackPurpose::Subscriber] {
        let connections = site.state.store.slack_connections(purpose).await.unwrap();
        shown.push(("connections".to_owned(), format!("{connections:?}")));
    }
    shown.push(("error".to_owned(), format!("{failed} {failed:?}")));
    let recorded = column(
        &site,
        "SELECT coalesce(last_error, '') FROM slack_delivery
         UNION ALL SELECT coalesce(dead_reason, '') FROM slack_connection",
    )
    .await;
    shown.push(("recorded errors".to_owned(), recorded.join("\n")));
    let log = log.text();
    assert!(log.contains("Slack channel connected for alerts"), "{log}");
    assert!(log.contains("Slack revoked the connection"), "{log}");
    shown.push(("log".to_owned(), log));

    for (place, text) in shown {
        assert!(
            !text.contains(MARKER),
            "{place} shows a webhook URL or token: {text}"
        );
        assert!(
            !text.contains(&fake.url),
            "{place} shows a webhook URL: {text}"
        );
        assert!(
            !text.contains(CLIENT_SECRET),
            "{place} shows the client secret: {text}"
        );
    }
}

#[tokio::test]
async fn slack_widens_form_action_only_when_on() {
    let (site, _fake) = slack_site("").await;
    let (on, off) = (
        site.get("/subscribe").await.1,
        super::site().get("/subscribe").await.1,
    );
    let policy = |headers: &HeaderMap| {
        headers[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .to_owned()
    };
    assert!(
        policy(&on).contains("form-action 'self' https://slack.com;"),
        "{}",
        policy(&on)
    );
    assert!(
        policy(&off).contains("form-action 'self';"),
        "{}",
        policy(&off)
    );
    assert!(!policy(&off).contains("slack.com"), "{}", policy(&off));
    let (_, headers, _) = site.admin_get("/admin/slack").await;
    assert!(policy(&headers).contains("form-action 'self' https://slack.com;"));

    // With Turnstile's own policy too.
    let config = config(&format!(
        "{ON}[subscribers.turnstile]\nsite_key = \"0x4AAAAAAA\"\n\
         secret_key = \"${{ENV:TURNSTILE_SECRET}}\"\n\n{SLACK}"
    ));
    let fake = Fake::start(&crate::slack::manifest(&config).unwrap()).await;
    let app = fake.app(&config);
    let site = site_from(config, NO_TURNSTILE, Some(app));
    let (_, headers, _) = site.get("/subscribe").await;
    let policy = policy(&headers);
    assert!(
        policy.contains("form-action 'self' https://slack.com;"),
        "{policy}"
    );
    assert!(
        policy.contains("frame-src https://challenges.cloudflare.com;"),
        "{policy}"
    );
}

#[tokio::test]
async fn slack_pages_pass_the_structural_audit() {
    let (site, fake) = slack_site("").await;
    let manage = site.connect_subscriber(&fake, &[("projects", "api")]).await;
    fake.wait_for(1, 1).await;

    let mut pages = Vec::new();
    for uri in [
        "/subscribe".to_owned(),
        format!("/subscribe/slack/manage/{manage}?done=connected"),
        format!("/subscribe/slack/unsubscribe/{manage}"),
    ] {
        pages.push((uri.clone(), site.get(&uri).await.2));
    }
    let back = approve(&fake, &site.start_subscriber(&[("projects", "api")]).await).await;
    let cancelled = format!(
        "/slack/callback?error=access_denied&state={}",
        state_of(&back.path)
    );
    pages.push((
        "cancelled".to_owned(),
        site.callback(&cancelled, Some(&back.cookie)).await.2,
    ));
    let (_, _, left) = site
        .post(
            &format!("/subscribe/slack/unsubscribe/{manage}"),
            Some(BASE),
            String::new(),
            9,
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

/// `SendError` keeps whether a failure is for good.
trait Permanence {
    fn is_permanent(&self) -> bool;
}

impl Permanence for SendError {
    fn is_permanent(&self) -> bool {
        self.permanent
    }
}

impl Site {
    /// Opens an incident from the admin create form, as a browser would.
    async fn admin_create(&self, title: &str, extra: &[(&str, &str)]) {
        let (_, _, page) = self.admin_get("/admin/").await;
        let token = csrf(&page);
        let mut fields = vec![
            ("title", title),
            ("impact", "minor"),
            ("status", "investigating"),
            ("components", "api"),
            ("message", "Looking into it."),
            ("csrf", token.as_str()),
        ];
        fields.extend_from_slice(extra);
        let (status, _, page) = self
            .call(
                admin(Method::POST, "/admin/incidents")
                    .header(header::ORIGIN, BASE)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(form(&fields)))
                    .unwrap(),
            )
            .await;
        assert!(status.is_redirection(), "{status} {page}");
    }

    /// Posts an update to incident `id` from the admin page, as a browser
    /// would: the CSRF token comes from the incident's own page.
    async fn admin_update(&self, id: i64, fields: &[(&str, &str)]) -> StatusCode {
        let path = format!("/admin/incidents/{id}");
        let (status, _, page) = self.admin_get(&path).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let token = csrf(&page);
        let mut fields = fields.to_vec();
        fields.push(("csrf", &token));
        let (status, _, page) = self
            .call(
                admin(Method::POST, &format!("{path}/updates"))
                    .header(header::ORIGIN, BASE)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(form(&fields)))
                    .unwrap(),
            )
            .await;
        assert!(status.is_redirection(), "{status} {page}");
        status
    }

    /// Calls the MCP tool `name` over `/mcp` as the on-call token.
    async fn mcp_tool(&self, name: &str, arguments: Value) {
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        });
        let (status, _, text) = self
            .call(
                super::request(Method::POST, "/mcp", 1)
                    .header(header::AUTHORIZATION, format!("Bearer {}", super::ONCALL))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ACCEPT, "application/json, text/event-stream")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert!(!text.contains("\"isError\":true"), "{text}");
    }
}

/// Every email and Slack post that mentions `needle`, as (email, slack).
fn heard(site: &Site, fake: &Fake, needle: &str) -> (usize, usize) {
    let email = site
        .sent
        .to("reader@example.com")
        .iter()
        .filter(|sent| sent.body().contains(needle))
        .count();
    let slack = fake
        .posts_to(1)
        .iter()
        .filter(|post| text_of(post).contains(needle))
        .count();
    (email, slack)
}

#[tokio::test]
async fn the_admin_update_form_can_post_quietly_and_defaults_to_telling_subscribers() {
    let (site, fake) = slack_site("").await;
    fake.pick("#acme-status");
    site.connect_subscriber(&fake, &[("components", "api"), ("projects", "api")])
        .await;
    site.subscribe("reader@example.com").await;
    let postman = site.postman();
    let id = site.open_incident("API errors", &["api"]).await;

    let (_, _, page) = site.admin_get(&format!("/admin/incidents/{id}")).await;
    assert!(page.contains("name=\"notify\""), "{page}");
    assert!(page.contains("name=\"notify_shown\""), "{page}");
    assert!(page.contains(" checked>"), "the switch is on by default");

    // Unchecked: the browser sends `notify_shown` and no `notify`.
    site.admin_update(
        id,
        &[
            ("status", "identified"),
            ("message", "Quiet typo fix."),
            ("notify_shown", "1"),
        ],
    )
    .await;
    // Checked.
    site.admin_update(
        id,
        &[
            ("status", "identified"),
            ("message", "Found the cause."),
            ("notify_shown", "1"),
            ("notify", "1"),
        ],
    )
    .await;
    // No field at all: the server's default is to tell.
    site.admin_update(id, &[("status", "monitoring"), ("message", "Watching it.")])
        .await;
    site.settled().await;
    postman.stop().await;

    assert_eq!(heard(&site, &fake, "Quiet typo fix."), (0, 0));
    assert_eq!(heard(&site, &fake, "Found the cause."), (1, 1));
    assert_eq!(heard(&site, &fake, "Watching it."), (1, 1));
}

#[tokio::test]
async fn the_mcp_update_tool_can_post_quietly_and_defaults_to_telling_subscribers() {
    let (site, fake) = slack_site("").await;
    fake.pick("#acme-status");
    site.connect_subscriber(&fake, &[("components", "api"), ("projects", "api")])
        .await;
    site.subscribe("reader@example.com").await;
    let postman = site.postman();
    let id = site.open_incident("API errors", &["api"]).await;

    let update =
        |status: &str, message: &str| json!({"id": id, "status": status, "message": message});
    let mut quiet = update("identified", "Quiet typo fix.");
    quiet["notify"] = json!(false);
    site.mcp_tool("update_incident", quiet).await;
    let mut loud = update("identified", "Found the cause.");
    loud["notify"] = json!(true);
    site.mcp_tool("update_incident", loud).await;
    site.mcp_tool("update_incident", update("monitoring", "Watching it."))
        .await;
    site.settled().await;
    postman.stop().await;

    assert_eq!(heard(&site, &fake, "Quiet typo fix."), (0, 0));
    assert_eq!(heard(&site, &fake, "Found the cause."), (1, 1));
    assert_eq!(heard(&site, &fake, "Watching it."), (1, 1));
}

#[tokio::test]
async fn opening_an_incident_can_be_quiet_over_the_api_the_admin_form_and_mcp() {
    let (site, fake) = slack_site("").await;
    fake.pick("#acme-status");
    site.connect_subscriber(&fake, &[("components", "api"), ("projects", "api")])
        .await;
    site.subscribe("reader@example.com").await;
    let postman = site.postman();

    let (_, _, page) = site.admin_get("/admin/").await;
    assert!(page.contains("name=\"notify_shown\""), "{page}");
    assert!(page.contains(" checked> Notify subscribers"), "{page}");

    let api = |title: &str, extra: Value| {
        let mut body =
            json!({"title": title, "message": "Looking into it.", "components": ["api"]});
        if let Value::Object(extra) = extra {
            body.as_object_mut().unwrap().extend(extra);
        }
        body
    };
    let post = |body: Value| async {
        let (status, _, answer) = site
            .api(
                Method::POST,
                "/api/v1/incidents",
                Some(super::ONCALL),
                Some(body),
                1,
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{answer}");
    };
    post(api("Quiet via API", json!({"notify": false}))).await;
    post(api("Loud via API", json!({"notify": true}))).await;
    post(api("Default via API", json!({}))).await;

    // The admin form: unchecked, checked, and no field at all.
    site.admin_create("Quiet via form", &[("notify_shown", "1")])
        .await;
    site.admin_create("Loud via form", &[("notify_shown", "1"), ("notify", "1")])
        .await;
    site.admin_create("Default via form", &[]).await;

    // MCP, which drafts unless told not to: a draft tells nobody.
    site.mcp_tool(
        "create_incident",
        api("Quiet via MCP", json!({"notify": false, "draft": false})),
    )
    .await;
    site.mcp_tool(
        "create_incident",
        api("Loud via MCP", json!({"notify": true, "draft": false})),
    )
    .await;
    site.mcp_tool(
        "create_incident",
        api("Default via MCP", json!({"draft": false})),
    )
    .await;
    site.mcp_tool(
        "create_incident",
        api("Drafted via MCP", json!({"notify": true})),
    )
    .await;
    site.settled().await;
    postman.stop().await;

    let titles = |site: &Site, fake: &Fake, title: &str| {
        let email = site
            .sent
            .to("reader@example.com")
            .iter()
            .filter(|sent| super::subject(sent).contains(title))
            .count();
        let slack = fake
            .posts_to(1)
            .iter()
            .filter(|post| text_of(post).contains(title))
            .count();
        (email, slack)
    };
    for path in ["API", "form", "MCP"] {
        assert_eq!(
            titles(&site, &fake, &format!("Quiet via {path}")),
            (0, 0),
            "{path}"
        );
        assert_eq!(
            titles(&site, &fake, &format!("Loud via {path}")),
            (1, 1),
            "{path}"
        );
        assert_eq!(
            titles(&site, &fake, &format!("Default via {path}")),
            (1, 1),
            "{path}"
        );
    }
    assert_eq!(titles(&site, &fake, "Drafted via MCP"), (0, 0));
}

/// A draft tells nobody however it is written to, and publishing it tells
/// everyone exactly once, over the API, the admin and the internal board.
#[tokio::test]
async fn a_draft_tells_nobody_until_it_is_published_once() {
    let (site, fake) = slack_site("").await;
    fake.pick("#acme-status");
    site.connect_subscriber(&fake, &[("components", "api"), ("projects", "api")])
        .await;
    site.subscribe("reader@example.com").await;
    let postman = site.postman();

    // Over the API: a draft, then a loud update to it.
    let (status, _, draft) = site
        .api(
            Method::POST,
            "/api/v1/incidents",
            Some(super::ONCALL),
            Some(json!({
                "title": "Drafted via API",
                "components": ["api"],
                "message": "First look.",
                "draft": true,
            })),
            1,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{draft}");
    assert_eq!(draft["published_at"], Value::Null, "{draft}");
    let id = draft["id"].as_i64().unwrap();
    site.update_incident(id, "Found it.", true).await;
    site.settled().await;
    assert_eq!(heard(&site, &fake, "Drafted via API"), (0, 0));

    let site = &site;
    let publish = |id: i64| async move {
        site.api(
            Method::POST,
            &format!("/api/v1/incidents/{id}/publish"),
            Some(super::ONCALL),
            None,
            1,
        )
        .await
    };
    let (status, _, published) = publish(id).await;
    assert_eq!(status, StatusCode::OK, "{published}");
    assert_ne!(published["published_at"], Value::Null, "{published}");
    assert_eq!(published["published_by"]["id"], "oncall", "{published}");
    site.settled().await;
    assert_eq!(heard(site, &fake, "Drafted via API"), (1, 1));
    // The announcement carries where the draft got to, not its first word.
    assert_eq!(heard(site, &fake, "Found it."), (1, 1));
    let (status, _, again) = publish(id).await;
    assert_eq!(status, StatusCode::CONFLICT, "{again}");
    site.settled().await;
    assert_eq!(heard(site, &fake, "Drafted via API"), (1, 1));

    // The admin form's draft box, then Publish on the internal board.
    site.admin_create(
        "Drafted via form",
        &[("draft", "1"), ("notify_shown", "1"), ("notify", "1")],
    )
    .await;
    site.settled().await;
    assert_eq!(heard(site, &fake, "Drafted via form"), (0, 0));
    let drafts = site.state.store.draft_incidents().await.unwrap();
    assert_eq!(drafts.len(), 1, "{drafts:?}");
    let id = drafts[0].id;
    let (status, _, board) = site.admin_get("/internal/").await;
    assert_eq!(status, StatusCode::OK, "{board}");
    let section = board.find("Draft incidents").expect("the drafts section");
    assert!(
        section < board.find("id=\"board\"").unwrap(),
        "drafts come before the checks: {board}"
    );
    assert!(board.contains("Drafted via form"), "{board}");
    assert!(
        board.contains(&format!("action=\"/admin/incidents/{id}/publish\"")),
        "{board}"
    );
    assert!(board.contains(">Publish</button>"), "{board}");
    let (status, headers, page) = site
        .admin_post(
            &format!("/admin/incidents/{id}/publish"),
            &[("back", "internal")],
        )
        .await;
    assert!(status.is_redirection(), "{status} {page}");
    assert_eq!(headers[header::LOCATION], "/internal/");
    site.settled().await;
    assert_eq!(heard(site, &fake, "Drafted via form"), (1, 1));
    let (_, _, board) = site.admin_get("/internal/").await;
    assert!(!board.contains("Draft incidents"), "{board}");

    // A discarded draft is gone and told nobody.
    site.admin_create("Discarded via form", &[("draft", "1")])
        .await;
    let id = site.state.store.draft_incidents().await.unwrap()[0].id;
    let (status, headers, page) = site
        .admin_post(&format!("/admin/incidents/{id}/discard"), &[])
        .await;
    assert!(status.is_redirection(), "{status} {page}");
    assert_eq!(headers[header::LOCATION], "/admin/");
    assert!(site.state.store.incident(id).await.unwrap().is_none());
    site.settled().await;
    postman.stop().await;
    assert_eq!(heard(site, &fake, "Discarded via form"), (0, 0));
}
