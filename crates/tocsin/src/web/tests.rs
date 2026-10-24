// SPDX-License-Identifier: AGPL-3.0-only

//! The admin page: Cloudflare Access tokens against a local key server,
//! trusted headers by socket peer, CSRF, allow-lists, escaping, assets,
//! and create → update → resolve over a real socket.

use std::net::SocketAddr;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode, header};
use axum::response::Response;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair, RsaPublicKeyComponents};
use serde_json::{Value, json};
use tocsin_core::config::{Config, Environment, Source};
use tocsin_core::incident::{Actor, Impact, IncidentKind, NewIncident, Timestamp};
use tocsin_store::Store;
use tokio::sync::watch;
use tower::ServiceExt;

use super::access::{Rejection, TOKEN_HEADER, Verifier};
use super::identity::AdminAuth;
use crate::http::events::{Event, EventBus};
use crate::http::{self, AppState, incidents};

pub(super) const SIGNING_KEY: &[u8] = include_bytes!("testdata/signing.pk8");
pub(super) const OTHER_KEY: &[u8] = include_bytes!("testdata/other.pk8");
pub(super) const TEAM: &str = "acme";
pub(super) const AUDIENCE: &str =
    "4714c1358e65fe4b408ad6d432a5f878f08194bdb4752441fd56faefa9b2b6f2";
pub(super) const HOST: &str = "status.example.com";
/// `[server] public_url`'s origin: the site as people open it.
const ORIGIN: &str = "https://status.example.com";
pub(super) const ALICE: &str = "alice@example.com";

fn config(auth: &str) -> Config {
    config_with(auth, "")
}

/// [`config`] with `more` TOML after the components.
fn config_with(auth: &str, more: &str) -> Config {
    let text = format!(
        "[server]\npublic_url = \"{ORIGIN}/\"\n\n[server.auth]\n{auth}\n\n\
         [[component]]\nid = \"api\"\nname = \"API\"\n\n\
         [[component]]\nid = \"web\"\nname = \"Web\"\n\n{more}"
    );
    tocsin_core::config::load(
        &[Source {
            name: "tocsin.toml".into(),
            text: Ok(text),
        }],
        &Environment {
            hostname: "test".into(),
            variable: &|_| None,
        },
    )
    .unwrap()
}

/// A store, the state around it, and the router serving it.
struct Site {
    _dir: tempfile::TempDir,
    state: AppState,
    router: Router,
}

fn site(config: Config, admin: Option<AdminAuth>) -> Site {
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
        subscribers: None,
    };
    Site {
        _dir: dir,
        router: http::router(state.clone()),
        state,
    }
}

/// A site in `trusted_header` mode, trusting 10.0.0.0/8, with `extra`
/// settings.
fn header_site(extra: &str) -> Site {
    let config = config(&format!(
        "mode = \"trusted_header\"\ntrusted_proxies = [\"10.0.0.0/8\"]\n{extra}"
    ));
    let admin = AdminAuth::from_config(&config).unwrap();
    site(config, admin)
}

pub(super) const PROXY: &str = "10.1.2.3:41000";
pub(super) const OUTSIDER: &str = "203.0.113.9:41000";

/// A request arriving from socket `peer`.
pub(super) fn from(peer: &str, method: Method, uri: &str) -> axum::http::request::Builder {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, HOST);
    request
        .extensions_mut()
        .unwrap()
        .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
    request
}

pub(super) async fn send(router: &Router, request: Request<Body>) -> (StatusCode, Response) {
    let response = router.clone().oneshot(request).await.unwrap();
    (response.status(), response)
}

pub(super) async fn text(response: Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// `GET /admin/` as `email` through the trusted proxy; the page's CSRF
/// token.
async fn csrf_token(site: &Site, email: &str) -> String {
    let request = from(PROXY, Method::GET, "/admin/")
        .header("x-forwarded-email", email)
        .body(Body::empty())
        .unwrap();
    let (status, response) = send(&site.router, request).await;
    assert_eq!(status, StatusCode::OK);
    hidden_csrf(&text(response).await)
}

fn hidden_csrf(page: &str) -> String {
    let start = page.find("name=\"csrf\" value=\"").unwrap() + "name=\"csrf\" value=\"".len();
    page[start..][..page[start..].find('"').unwrap()].to_owned()
}

fn form(fields: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields)
        .finish()
}

/// `POST uri` as alice through the trusted proxy, from `origin`.
fn post_form(uri: &str, origin: &str, body: String) -> Request<Body> {
    from(PROXY, Method::POST, uri)
        .header("x-forwarded-email", ALICE)
        .header(header::ORIGIN, origin)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap()
}

async fn incident_count(state: &AppState) -> i64 {
    state
        .store
        .read(|conn| Ok(conn.query_row("SELECT count(*) FROM incident", [], |row| row.get(0))?))
        .await
        .unwrap()
}

// --- Cloudflare Access tokens ---------------------------------------------

fn pair(pkcs8: &[u8]) -> RsaKeyPair {
    RsaKeyPair::from_pkcs8(pkcs8).unwrap()
}

pub(super) fn jwk(kid: &str, pkcs8: &[u8]) -> Value {
    let public = RsaPublicKeyComponents::<Vec<u8>>::from(pair(pkcs8).public());
    json!({
        "kid": kid,
        "kty": "RSA",
        "alg": "RS256",
        "use": "sig",
        "n": URL_SAFE_NO_PAD.encode(&public.n),
        "e": URL_SAFE_NO_PAD.encode(&public.e),
    })
}

fn encode(value: &Value) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).unwrap())
}

/// A token with `header` and `claims`, RS256-signed by `pkcs8`.
pub(super) fn sign(pkcs8: &[u8], header: &Value, claims: &Value) -> String {
    let signed = format!("{}.{}", encode(header), encode(claims));
    let key = pair(pkcs8);
    let mut signature = vec![0; key.public().modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        signed.as_bytes(),
        &mut signature,
    )
    .unwrap();
    format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature))
}

pub(super) fn epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Claims Access would issue for `email`, valid for five minutes.
pub(super) fn claims(email: &str) -> Value {
    let now = epoch();
    json!({
        "iss": format!("https://{TEAM}.cloudflareaccess.com"),
        "aud": [AUDIENCE],
        "email": email,
        "iat": now,
        "nbf": now,
        "exp": now + 300,
        "sub": "7335d417-61da-459d-899c-0a01c76a2f94",
        "type": "app",
    })
}

pub(super) fn rs256(kid: &str) -> Value {
    json!({"alg": "RS256", "kid": kid, "typ": "JWT"})
}

/// A local stand-in for `https://<team>.cloudflareaccess.com/cdn-cgi/access/certs`.
pub(super) struct KeyServer {
    url: String,
    hits: Arc<AtomicUsize>,
}

impl KeyServer {
    pub(super) async fn start(keys: Vec<Value>) -> Self {
        let keys = Arc::new(Mutex::new(keys));
        let hits = Arc::new(AtomicUsize::new(0));
        let app = {
            let (keys, hits) = (Arc::clone(&keys), Arc::clone(&hits));
            Router::new().route(
                "/cdn-cgi/access/certs",
                axum::routing::get(move || {
                    hits.fetch_add(1, Ordering::SeqCst);
                    let body = json!({"keys": *keys.lock().unwrap()});
                    async move { axum::Json(body) }
                }),
            )
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/cdn-cgi/access/certs",
            listener.local_addr().unwrap()
        );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        KeyServer { url, hits }
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    pub(super) fn verifier(&self) -> Verifier {
        Verifier::new(TEAM, AUDIENCE)
            .unwrap()
            .with_certs_url(&self.url)
    }
}

/// A certs endpoint in memory, for tests that run on paused time: a real
/// socket would let the paused clock jump while it waits on I/O.
struct FakeCerts {
    keys: Mutex<Vec<Value>>,
    /// Fetches started.
    hits: watch::Sender<usize>,
    /// Fetches answered.
    answered: AtomicUsize,
    /// Answer an error instead of the keys.
    failing: AtomicBool,
    /// While true, fetches wait before answering.
    held: watch::Sender<bool>,
}

impl FakeCerts {
    fn new(keys: &[Value]) -> Arc<Self> {
        Arc::new(FakeCerts {
            keys: Mutex::new(keys.to_vec()),
            hits: watch::Sender::new(0),
            answered: AtomicUsize::new(0),
            failing: AtomicBool::new(false),
            held: watch::Sender::new(false),
        })
    }

    /// A verifier with the production timings that fetches from here.
    fn verifier(self: &Arc<Self>) -> Verifier {
        let certs = Arc::clone(self);
        Verifier::new(TEAM, AUDIENCE)
            .unwrap()
            .with_certs(Arc::new(move || {
                let certs = Arc::clone(&certs);
                Box::pin(async move {
                    certs.hits.send_modify(|hits| *hits += 1);
                    let _ = certs.held.subscribe().wait_for(|held| !held).await;
                    certs.answered.fetch_add(1, Ordering::SeqCst);
                    if certs.failing.load(Ordering::SeqCst) {
                        Err("HTTP 500".to_owned())
                    } else {
                        let keys = certs.keys.lock().unwrap();
                        Ok(serde_json::to_vec(&json!({ "keys": *keys })).unwrap())
                    }
                })
            }))
    }

    fn hits(&self) -> usize {
        *self.hits.borrow()
    }

    fn fail(&self, failing: bool) {
        self.failing.store(failing, Ordering::SeqCst);
    }
}

async fn verify(verifier: &Verifier, token: &str) -> Result<String, Rejection> {
    verifier.verify(token, SystemTime::now()).await
}

#[tokio::test]
async fn access_accepts_a_valid_token() {
    let server = KeyServer::start(vec![jwk("k1", SIGNING_KEY)]).await;
    let verifier = server.verifier();
    assert_eq!(
        verifier.issuer(),
        "https://acme.cloudflareaccess.com",
        "only the certs URL is overridden"
    );
    let token = sign(SIGNING_KEY, &rs256("k1"), &claims(ALICE));
    assert_eq!(verify(&verifier, &token).await.unwrap(), ALICE);
    // A single-string `aud` is accepted too.
    let mut single = claims(ALICE);
    single["aud"] = json!(AUDIENCE);
    let token = sign(SIGNING_KEY, &rs256("k1"), &single);
    assert_eq!(verify(&verifier, &token).await.unwrap(), ALICE);
    assert_eq!(server.hits(), 1, "keys are cached");
}

#[tokio::test]
async fn access_rejects_the_wrong_audience() {
    let server = KeyServer::start(vec![jwk("k1", SIGNING_KEY)]).await;
    let verifier = server.verifier();
    for aud in [
        json!("another-app"),
        json!(["another-app"]),
        json!([]),
        Value::Null,
    ] {
        let mut claims = claims(ALICE);
        claims["aud"] = aud.clone();
        let token = sign(SIGNING_KEY, &rs256("k1"), &claims);
        assert!(
            matches!(verify(&verifier, &token).await, Err(Rejection::Audience)),
            "aud {aud}"
        );
    }
}

#[tokio::test]
async fn access_rejects_the_wrong_issuer() {
    let server = KeyServer::start(vec![jwk("k1", SIGNING_KEY)]).await;
    let verifier = server.verifier();
    let mut claims = claims(ALICE);
    claims["iss"] = json!("https://evil.cloudflareaccess.com");
    let token = sign(SIGNING_KEY, &rs256("k1"), &claims);
    assert!(matches!(
        verify(&verifier, &token).await,
        Err(Rejection::Issuer)
    ));
}

#[tokio::test]
async fn access_rejects_an_expired_token_beyond_the_leeway() {
    let server = KeyServer::start(vec![jwk("k1", SIGNING_KEY)]).await;
    let verifier = server.verifier();
    let now = epoch();
    let mut expired = claims(ALICE);
    expired["exp"] = json!(now - 120);
    let token = sign(SIGNING_KEY, &rs256("k1"), &expired);
    assert!(matches!(
        verify(&verifier, &token).await,
        Err(Rejection::Expired)
    ));
    let mut missing = claims(ALICE);
    missing.as_object_mut().unwrap().remove("exp");
    let token = sign(SIGNING_KEY, &rs256("k1"), &missing);
    assert!(matches!(
        verify(&verifier, &token).await,
        Err(Rejection::Expired)
    ));
    // Within the 30 s leeway, a clock a little behind Access's still works.
    let mut just = claims(ALICE);
    just["exp"] = json!(now - 5);
    let token = sign(SIGNING_KEY, &rs256("k1"), &just);
    assert_eq!(verify(&verifier, &token).await.unwrap(), ALICE);
}

#[tokio::test]
async fn access_rejects_a_token_not_yet_valid() {
    let server = KeyServer::start(vec![jwk("k1", SIGNING_KEY)]).await;
    let verifier = server.verifier();
    let mut early = claims(ALICE);
    early["nbf"] = json!(epoch() + 600);
    let token = sign(SIGNING_KEY, &rs256("k1"), &early);
    assert!(matches!(
        verify(&verifier, &token).await,
        Err(Rejection::NotYetValid)
    ));
}

#[tokio::test]
async fn access_rejects_a_token_signed_by_another_key() {
    let server = KeyServer::start(vec![jwk("k1", SIGNING_KEY)]).await;
    let verifier = server.verifier();
    let token = sign(OTHER_KEY, &rs256("k1"), &claims(ALICE));
    assert!(matches!(
        verify(&verifier, &token).await,
        Err(Rejection::Signature)
    ));
    // A valid signature over different claims does not carry over.
    let good = sign(SIGNING_KEY, &rs256("k1"), &claims(ALICE));
    let forged = sign(SIGNING_KEY, &rs256("k1"), &claims("mallory@example.com"));
    let (good_head, good_rest) = good.split_once('.').unwrap();
    let good_signature = good_rest.rsplit_once('.').unwrap().1;
    let forged_payload = forged.split('.').nth(1).unwrap();
    let spliced = format!("{good_head}.{forged_payload}.{good_signature}");
    assert!(matches!(
        verify(&verifier, &spliced).await,
        Err(Rejection::Signature)
    ));
}

#[tokio::test]
async fn access_accepts_only_rs256() {
    let server = KeyServer::start(vec![jwk("k1", SIGNING_KEY)]).await;
    let verifier = server.verifier();
    let payload = encode(&claims(ALICE));
    // alg none: no signature at all.
    let none = format!(
        "{}.{payload}.",
        encode(&json!({"alg": "none", "kid": "k1"}))
    );
    // HS256 keyed with the public modulus, the classic confusion attack.
    let header = encode(&json!({"alg": "HS256", "kid": "k1", "typ": "JWT"}));
    let public = RsaPublicKeyComponents::<Vec<u8>>::from(pair(SIGNING_KEY).public());
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &public.n);
    let tag = ring::hmac::sign(&key, format!("{header}.{payload}").as_bytes());
    let hs256 = format!("{header}.{payload}.{}", URL_SAFE_NO_PAD.encode(tag));
    // RS512 is RSA too, but not what Access signs with.
    let rs512 = sign(
        SIGNING_KEY,
        &json!({"alg": "RS512", "kid": "k1"}),
        &claims(ALICE),
    );
    for (name, token) in [("none", none), ("HS256", hs256), ("RS512", rs512)] {
        assert!(
            matches!(verify(&verifier, &token).await, Err(Rejection::Algorithm(alg)) if alg == name),
            "{name}"
        );
    }
    assert_eq!(server.hits(), 0, "rejected before any key is fetched");
}

// The key-cache timings run on paused time with the production values:
// keys are fresh for an hour, trusted for an hour more while refetches
// fail, and refetched at most once a minute; before the first success,
// fetches back off from one second, doubling, up to that minute.

const HOUR: Duration = Duration::from_secs(3600);
const MINUTE: Duration = Duration::from_secs(60);
const TICK: Duration = Duration::from_millis(1);

#[tokio::test(start_paused = true)]
async fn access_refetches_keys_for_an_unknown_kid_at_most_once_per_cooldown() {
    let certs = FakeCerts::new(&[jwk("k1", SIGNING_KEY)]);
    let verifier = certs.verifier();
    let first = sign(SIGNING_KEY, &rs256("k1"), &claims(ALICE));
    assert_eq!(verify(&verifier, &first).await.unwrap(), ALICE);
    assert_eq!(certs.hits(), 1);

    // Access rotates in a new key: the unknown kid triggers a refetch once
    // the cooldown has passed, and the new key is picked up.
    certs.keys.lock().unwrap().push(jwk("k2", OTHER_KEY));
    let rotated = sign(OTHER_KEY, &rs256("k2"), &claims(ALICE));
    tokio::time::advance(MINUTE.saturating_sub(TICK)).await;
    assert!(matches!(
        verify(&verifier, &rotated).await,
        Err(Rejection::UnknownKey(_))
    ));
    assert_eq!(certs.hits(), 1, "no refetch within the cooldown");
    tokio::time::advance(TICK).await;
    assert_eq!(verify(&verifier, &rotated).await.unwrap(), ALICE);
    assert_eq!(certs.hits(), 2);

    // A kid Access never published: refused, and a burst of them within
    // the cooldown does not hammer the key server.
    let unknown = sign(SIGNING_KEY, &rs256("forged"), &claims(ALICE));
    for _ in 0..5 {
        assert!(matches!(
            verify(&verifier, &unknown).await,
            Err(Rejection::UnknownKey(kid)) if kid == "forged"
        ));
    }
    assert_eq!(certs.hits(), 2, "rate-limited by the cooldown");
    // After the cooldown it refetches once, and the kid is still unknown.
    tokio::time::advance(MINUTE).await;
    assert!(matches!(
        verify(&verifier, &unknown).await,
        Err(Rejection::UnknownKey(_))
    ));
    assert_eq!(certs.hits(), 3);
}

#[tokio::test]
async fn access_refuses_everything_while_the_keys_are_unreachable() {
    let verifier = Verifier::new(TEAM, AUDIENCE)
        .unwrap()
        .with_certs_url("http://127.0.0.1:9/cdn-cgi/access/certs");
    let token = sign(SIGNING_KEY, &rs256("k1"), &claims(ALICE));
    assert!(matches!(
        verify(&verifier, &token).await,
        Err(Rejection::KeysUnavailable)
    ));
}

#[tokio::test(start_paused = true)]
async fn access_backs_off_from_one_second_until_keys_are_first_fetched() {
    let certs = FakeCerts::new(&[jwk("k1", SIGNING_KEY)]);
    certs.fail(true);
    let verifier = certs.verifier();
    let token = sign(SIGNING_KEY, &rs256("k1"), &claims(ALICE));
    let unavailable = |result| matches!(result, Err(Rejection::KeysUnavailable));

    assert!(unavailable(verify(&verifier, &token).await));
    assert_eq!(certs.hits(), 1);
    let gaps = [1, 2, 4, 8, 16, 32, 60, 60].map(Duration::from_secs);
    for (fetched, gap) in (1..).zip(gaps) {
        tokio::time::advance(gap.saturating_sub(TICK)).await;
        assert!(unavailable(verify(&verifier, &token).await));
        assert_eq!(certs.hits(), fetched, "a fetch before {gap:?} had passed");
        tokio::time::advance(TICK).await;
        assert!(unavailable(verify(&verifier, &token).await));
        assert_eq!(
            certs.hits(),
            fetched + 1,
            "no fetch once {gap:?} had passed"
        );
    }

    // Once Access answers, the next fetch admits the token.
    certs.fail(false);
    tokio::time::advance(MINUTE).await;
    assert_eq!(verify(&verifier, &token).await.unwrap(), ALICE);
}

#[tokio::test(start_paused = true)]
async fn access_uses_stale_keys_without_waiting_while_one_task_refetches() {
    let certs = FakeCerts::new(&[jwk("k1", SIGNING_KEY)]);
    let verifier = certs.verifier();
    let token = sign(SIGNING_KEY, &rs256("k1"), &claims(ALICE));
    assert_eq!(verify(&verifier, &token).await.unwrap(), ALICE);
    assert_eq!(certs.hits(), 1);

    // The keys are past their age, and the refetch will not answer until
    // released. Every request completes on its first poll: none waits.
    tokio::time::advance(HOUR + TICK).await;
    certs.held.send_replace(true);
    for request in 0..10 {
        let verification = pin!(verify(&verifier, &token));
        match verification.poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(result) => assert_eq!(result.unwrap(), ALICE),
            Poll::Pending => panic!("request {request} waited on the refetch"),
        }
    }

    // One refetch started for all ten, and it has not answered yet.
    let mut hits = certs.hits.subscribe();
    let _ = hits.wait_for(|hits| *hits > 1).await;
    tokio::task::yield_now().await;
    assert_eq!(certs.hits(), 2, "one refetch for all ten requests");
    assert_eq!(certs.answered.load(Ordering::SeqCst), 1, "only the first");

    certs.held.send_replace(false);
    verifier.settle().await;
    assert_eq!(certs.answered.load(Ordering::SeqCst), 2);
    assert_eq!(verify(&verifier, &token).await.unwrap(), ALICE);
    assert_eq!(certs.hits(), 2, "fresh keys again: no fetch");
}

#[tokio::test(start_paused = true)]
async fn access_starts_no_second_fetch_while_one_is_still_open() {
    let certs = FakeCerts::new(&[jwk("k1", SIGNING_KEY)]);
    let verifier = certs.verifier();
    let token = sign(SIGNING_KEY, &rs256("k1"), &claims(ALICE));
    let mut context = Context::from_waker(Waker::noop());

    // The first fetch hangs. The startup backoff alone would allow another
    // after a second, well inside the fetch timeout.
    certs.held.send_replace(true);
    let mut first = pin!(verify(&verifier, &token));
    assert!(first.as_mut().poll(&mut context).is_pending());
    tokio::task::yield_now().await;
    assert_eq!(certs.hits(), 1);
    for _ in 0..2 {
        tokio::time::advance(Duration::from_secs(2)).await;
        let later = pin!(verify(&verifier, &token));
        assert!(
            later.poll(&mut context).is_pending(),
            "waits on the open fetch"
        );
        tokio::task::yield_now().await;
        assert_eq!(certs.hits(), 1, "a second fetch while the first is open");
    }

    // When it answers, the request waiting on it goes through.
    certs.held.send_replace(false);
    assert_eq!(first.await.unwrap(), ALICE);
    assert_eq!(certs.hits(), 1);
}

#[tokio::test(start_paused = true)]
async fn access_refuses_keys_past_their_age_and_grace_when_refetches_fail() {
    let certs = FakeCerts::new(&[jwk("k1", SIGNING_KEY)]);
    let verifier = certs.verifier();
    let token = sign(SIGNING_KEY, &rs256("k1"), &claims(ALICE));
    assert_eq!(verify(&verifier, &token).await.unwrap(), ALICE);

    // Access is unreachable. Within the grace the cached key still works,
    // and a refetch is attempted.
    certs.fail(true);
    tokio::time::advance(HOUR + TICK).await;
    assert_eq!(verify(&verifier, &token).await.unwrap(), ALICE);
    verifier.settle().await;
    assert_eq!(certs.hits(), 2);

    // Just inside the grace the key is still used; at its end, nothing is
    // trusted, however valid the token.
    tokio::time::advance(HOUR.saturating_sub(TICK * 2)).await;
    assert_eq!(verify(&verifier, &token).await.unwrap(), ALICE);
    tokio::time::advance(TICK).await;
    assert!(matches!(
        verify(&verifier, &token).await,
        Err(Rejection::KeysStale)
    ));

    // Once Access answers again, tokens are accepted again.
    certs.fail(false);
    tokio::time::advance(MINUTE).await;
    assert_eq!(verify(&verifier, &token).await.unwrap(), ALICE);
}

#[test]
fn a_mixed_case_team_builds_the_issuer_access_signs() {
    for team in ["MyTeam", "MyTeam.CloudflareAccess.com"] {
        let config = config(&format!(
            "mode = \"cloudflare_access\"\nteam_domain = \"{team}\"\naudience = \"{AUDIENCE}\""
        ));
        let admin = AdminAuth::from_config(&config).unwrap().unwrap();
        assert_eq!(
            admin.verifier().unwrap().issuer(),
            "https://myteam.cloudflareaccess.com",
            "{team}"
        );
    }
}

/// A site in `cloudflare_access` mode whose verifier reads `server`.
fn access_site(server: &KeyServer) -> Site {
    let config = config(&format!(
        "mode = \"cloudflare_access\"\nteam_domain = \"{TEAM}\"\naudience = \"{AUDIENCE}\""
    ));
    let admin = AdminAuth::with_verifier(server.verifier(), &config);
    site(config, Some(admin))
}

#[tokio::test]
async fn access_mode_admits_a_valid_token_and_nothing_else() {
    let server = KeyServer::start(vec![jwk("k1", SIGNING_KEY)]).await;
    let site = access_site(&server);
    let get = |headers: &[(&str, &str)]| {
        let mut request = Request::get("/admin/").header(header::HOST, HOST);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        request.body(Body::empty()).unwrap()
    };

    let token = sign(SIGNING_KEY, &rs256("k1"), &claims(ALICE));
    let (status, response) = send(&site.router, get(&[(TOKEN_HEADER, &token)])).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        text(response)
            .await
            .contains("Signed in as alice@example.com")
    );

    // No token at all.
    let (status, _) = send(&site.router, get(&[])).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // The email header Access adds, without the token: anything that
    // reaches the origin around Access can send it.
    let (status, response) = send(
        &site.router,
        get(&[("cf-access-authenticated-user-email", ALICE)]),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(!text(response).await.contains(ALICE));
    // Nor does it override the token's identity.
    let (status, response) = send(
        &site.router,
        get(&[
            (TOKEN_HEADER, &token),
            ("cf-access-authenticated-user-email", "mallory@example.com"),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!text(response).await.contains("mallory"));
    // The `CF_Authorization` cookie is not read.
    let (status, _) = send(
        &site.router,
        get(&[(
            header::COOKIE.as_str(),
            &format!("CF_Authorization={token}"),
        )]),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // Two token headers are ambiguous.
    let request = Request::get("/admin/")
        .header(TOKEN_HEADER, &token)
        .header(TOKEN_HEADER, &token)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        send(&site.router, request).await.0,
        StatusCode::UNAUTHORIZED
    );
}

// --- Trusted header ------------------------------------------------------

#[tokio::test]
async fn trusted_header_is_ignored_from_outside_trusted_proxies() {
    let site = header_site("");
    for forwarded_for in [None, Some("10.1.2.3"), Some("127.0.0.1, 10.1.2.3")] {
        let mut request = from(OUTSIDER, Method::GET, "/admin/").header("x-forwarded-email", ALICE);
        if let Some(value) = forwarded_for {
            request = request
                .header("x-forwarded-for", value)
                .header("x-real-ip", "10.1.2.3")
                .header("forwarded", "for=10.1.2.3");
        }
        let (status, response) = send(&site.router, request.body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{forwarded_for:?}");
        assert!(!text(response).await.contains(ALICE));
    }
}

#[tokio::test]
async fn trusted_header_is_accepted_from_a_trusted_proxy() {
    let site = header_site("");
    let request = from(PROXY, Method::GET, "/admin/")
        .header("x-forwarded-email", "Alice@Example.com")
        .body(Body::empty())
        .unwrap();
    let (status, response) = send(&site.router, request).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        text(response)
            .await
            .contains("Signed in as alice@example.com")
    );
    // From the proxy without the header, or with something not an address.
    for value in [None, Some("Alice <alice@example.com>"), Some("alice")] {
        let mut request = from(PROXY, Method::GET, "/admin/");
        if let Some(value) = value {
            request = request.header("x-forwarded-email", value);
        }
        let (status, _) = send(&site.router, request.body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{value:?}");
    }
}

#[tokio::test]
async fn trusted_header_matches_an_ipv4_mapped_peer_against_either_family() {
    // A dual-stack listener sees an IPv4 proxy as ::ffff:10.1.2.3.
    let mapped = "[::ffff:10.1.2.3]:41000";
    for (networks, admitted) in [
        (r#"["10.0.0.0/8"]"#, true),
        (r#"["::ffff:10.0.0.0/104"]"#, true),
        (r#"["::/0"]"#, true),
        (r#"["::ffff:11.0.0.0/104"]"#, false),
        (r#"["11.0.0.0/8"]"#, false),
    ] {
        let config = config(&format!(
            "mode = \"trusted_header\"\ntrusted_proxies = {networks}"
        ));
        let admin = AdminAuth::from_config(&config).unwrap();
        let site = site(config, admin);
        let request = from(mapped, Method::GET, "/admin/")
            .header("x-forwarded-email", ALICE)
            .body(Body::empty())
            .unwrap();
        let expected = if admitted {
            StatusCode::OK
        } else {
            StatusCode::UNAUTHORIZED
        };
        assert_eq!(send(&site.router, request).await.0, expected, "{networks}");
    }
}

#[tokio::test]
async fn trusted_header_reads_only_the_configured_header() {
    let site = header_site("header = \"X-Auth-Request-Email\"");
    let request = from(PROXY, Method::GET, "/admin/")
        .header("x-forwarded-email", ALICE)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        send(&site.router, request).await.0,
        StatusCode::UNAUTHORIZED
    );
    let request = from(PROXY, Method::GET, "/admin/")
        .header("x-auth-request-email", ALICE)
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&site.router, request).await.0, StatusCode::OK);
}

#[tokio::test]
async fn admin_is_404_when_auth_is_off() {
    for config in [config("mode = \"none\""), config("")] {
        assert!(AdminAuth::from_config(&config).unwrap().is_none());
        let site = site(config, None);
        for (method, uri) in [
            (Method::GET, "/admin/"),
            (Method::GET, "/admin"),
            (Method::GET, "/admin/incidents/1"),
            (Method::POST, "/admin/incidents"),
            (Method::GET, "/admin/timeline"),
        ] {
            let request = from(PROXY, method, uri)
                .header("x-forwarded-email", ALICE)
                .body(Body::empty())
                .unwrap();
            assert_eq!(
                send(&site.router, request).await.0,
                StatusCode::NOT_FOUND,
                "{uri}"
            );
        }
    }
}

// --- CSRF ----------------------------------------------------------------

fn new_incident(token: &str) -> String {
    form(&[
        ("csrf", token),
        ("title", "API errors"),
        ("impact", "major"),
        ("status", "investigating"),
        ("message", "Looking into it."),
    ])
}

#[tokio::test]
async fn forms_without_a_csrf_token_are_refused() {
    let site = header_site("");
    let body = form(&[
        ("title", "API errors"),
        ("impact", "major"),
        ("status", "investigating"),
        ("message", "Looking into it."),
    ]);
    let (status, _) = send(&site.router, post_form("/admin/incidents", ORIGIN, body)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(incident_count(&site.state).await, 0);
}

#[tokio::test]
async fn forms_with_a_wrong_csrf_token_are_refused() {
    let site = header_site("");
    let bob = csrf_token(&site, "bob@example.com").await;
    let alice = csrf_token(&site, ALICE).await;
    let (expiry, tag) = alice.split_once('.').unwrap();
    let later = format!("{}.{tag}", expiry.parse::<u64>().unwrap() + 3600);
    for wrong in [
        "nonsense".to_owned(),
        String::new(),
        bob,   // another person's token
        later, // alice's signature with a later expiry
        format!("{expiry}.AAAA"),
    ] {
        let (status, _) = send(
            &site.router,
            post_form("/admin/incidents", ORIGIN, new_incident(&wrong)),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{wrong}");
    }
    // A token from a different server key (a restart) is refused too.
    let other = header_site("");
    let stale = csrf_token(&other, ALICE).await;
    let (status, _) = send(
        &site.router,
        post_form("/admin/incidents", ORIGIN, new_incident(&stale)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(incident_count(&site.state).await, 0);
}

#[tokio::test]
async fn forms_from_another_origin_are_refused() {
    let site = header_site("");
    let token = csrf_token(&site, ALICE).await;
    for origin in [
        "https://evil.example",
        "https://status.example.com.evil.example",
        // The right host over plain http, or on another port.
        "http://status.example.com",
        "https://status.example.com:8443",
        "null",
    ] {
        let (status, _) = send(
            &site.router,
            post_form("/admin/incidents", origin, new_incident(&token)),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{origin}");
    }
    // Neither Origin nor Referer.
    let request = from(PROXY, Method::POST, "/admin/incidents")
        .header("x-forwarded-email", ALICE)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(new_incident(&token)))
        .unwrap();
    assert_eq!(send(&site.router, request).await.0, StatusCode::FORBIDDEN);
    // The browser says the request is cross-site, whatever Origin claims.
    let request = from(PROXY, Method::POST, "/admin/incidents")
        .header("x-forwarded-email", ALICE)
        .header(header::ORIGIN, ORIGIN)
        .header("sec-fetch-site", "cross-site")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(new_incident(&token)))
        .unwrap();
    assert_eq!(send(&site.router, request).await.0, StatusCode::FORBIDDEN);
    assert_eq!(incident_count(&site.state).await, 0);

    // An Origin naming the request's own Host is not enough: only
    // `public_url` counts, so a proxy rewriting Host changes nothing.
    let request = from(PROXY, Method::POST, "/admin/incidents")
        .header(header::HOST, "tocsin.internal:8080")
        .header("x-forwarded-email", ALICE)
        .header(header::ORIGIN, "http://tocsin.internal:8080")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(new_incident(&token)))
        .unwrap();
    assert_eq!(send(&site.router, request).await.0, StatusCode::FORBIDDEN);
    assert_eq!(incident_count(&site.state).await, 0);

    // The same form from this site goes through, with or without the
    // default port spelled out.
    for (origin, id) in [(ORIGIN, 1), ("https://status.example.com:443", 2)] {
        let (status, response) = send(
            &site.router,
            post_form("/admin/incidents", origin, new_incident(&token)),
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{origin}");
        assert_eq!(
            response.headers()[header::LOCATION],
            format!("/admin/incidents/{id}").as_str()
        );
    }
}

#[tokio::test]
async fn admin_pages_are_plain_forms_with_no_script() {
    let site = header_site("");
    let request = from(PROXY, Method::GET, "/admin/")
        .header("x-forwarded-email", ALICE)
        .body(Body::empty())
        .unwrap();
    let page = text(send(&site.router, request).await.1).await;
    assert_ne!(hidden_csrf(&page), "");
    // No boosted forms whose failures go unseen, and nothing to handle them.
    assert!(!page.contains("hx-"), "{page}");
    assert!(!page.contains("<script"), "{page}");
}

#[tokio::test]
async fn get_on_a_form_path_answers_with_a_page_never_json() {
    let site = header_site("");
    for (uri, expected) in [
        ("/admin/incidents", Some("/admin/")),
        ("/admin/incidents/7/updates", Some("/admin/incidents/7")),
        ("/admin/incidents/seven/updates", None),
    ] {
        let request = from(PROXY, Method::GET, uri)
            .header("x-forwarded-email", ALICE)
            .body(Body::empty())
            .unwrap();
        let (status, response) = send(&site.router, request).await;
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .map(|value| value.to_str().unwrap().to_owned());
        if let Some(location) = expected {
            assert_eq!(status, StatusCode::SEE_OTHER, "{uri}");
            assert_eq!(response.headers()[header::LOCATION], location, "{uri}");
        } else {
            assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
            assert!(
                content_type
                    .as_deref()
                    .is_some_and(|value| value.starts_with("text/html")),
                "{uri}: {content_type:?}"
            );
        }
        assert!(
            !content_type.is_some_and(|value| value.contains("json")),
            "{uri}"
        );
    }
}

#[tokio::test]
async fn the_resolve_form_starts_from_wording_for_the_kind() {
    let site = header_site("");
    for (kind, status, message) in [
        (
            IncidentKind::Incident,
            "resolved",
            "This incident has been resolved.",
        ),
        (
            IncidentKind::Maintenance,
            "completed",
            "This maintenance has been completed.",
        ),
    ] {
        let scheduled = (kind == IncidentKind::Maintenance).then_some((
            Timestamp::from_millis(4_000_000_000_000),
            Timestamp::from_millis(4_000_003_600_000),
        ));
        let incident = incidents::open(
            &site.state,
            NewIncident {
                kind,
                title: "Database upgrade".into(),
                impact: Impact::Minor,
                components: Vec::new(),
                status: None,
                message: "Planned.".into(),
                scheduled_start: scheduled.map(|(start, _)| start),
                scheduled_end: scheduled.map(|(_, end)| end),
            },
            Actor::person(ALICE),
        )
        .await
        .unwrap();
        let request = from(
            PROXY,
            Method::GET,
            &format!("/admin/incidents/{}", incident.id),
        )
        .header("x-forwarded-email", ALICE)
        .body(Body::empty())
        .unwrap();
        let page = text(send(&site.router, request).await.1).await;
        assert!(
            page.contains(&format!(
                r#"<input type="hidden" name="status" value="{status}">"#
            )),
            "{kind:?}"
        );
        assert!(
            page.contains(&format!("required>{message}</textarea>")),
            "{kind:?}: {page}"
        );
    }
}

// --- Allow-lists ---------------------------------------------------------

async fn admitted(site: &Site, email: &str) -> StatusCode {
    let request = from(PROXY, Method::GET, "/admin/")
        .header("x-forwarded-email", email)
        .body(Body::empty())
        .unwrap();
    send(&site.router, request).await.0
}

#[tokio::test]
async fn allowed_emails_admits_only_the_listed_addresses() {
    let site = header_site("allowed_emails = [\"Alice@Example.com\"]");
    assert_eq!(admitted(&site, ALICE).await, StatusCode::OK);
    assert_eq!(admitted(&site, "ALICE@example.com").await, StatusCode::OK);
    for other in [
        "bob@example.com",
        "alice@example.com.evil.example",
        "alice@evil.example",
    ] {
        assert_eq!(
            admitted(&site, other).await,
            StatusCode::FORBIDDEN,
            "{other}"
        );
    }
}

#[tokio::test]
async fn allowed_domains_admits_only_the_listed_domains() {
    let site = header_site("allowed_domains = [\"example.com\"]");
    assert_eq!(admitted(&site, ALICE).await, StatusCode::OK);
    for other in [
        "alice@evil.example",
        "alice@notexample.com",
        "alice@sub.example.com",
        "alice@example.com.evil.example",
    ] {
        assert_eq!(
            admitted(&site, other).await,
            StatusCode::FORBIDDEN,
            "{other}"
        );
    }
}

#[tokio::test]
async fn allow_lists_combine_as_either() {
    let site = header_site(
        "allowed_emails = [\"contractor@agency.example\"]\nallowed_domains = [\"example.com\"]",
    );
    assert_eq!(admitted(&site, ALICE).await, StatusCode::OK);
    assert_eq!(
        admitted(&site, "contractor@agency.example").await,
        StatusCode::OK
    );
    assert_eq!(
        admitted(&site, "other@agency.example").await,
        StatusCode::FORBIDDEN
    );
}

// --- Escaping ------------------------------------------------------------

#[tokio::test]
async fn script_in_a_title_or_body_never_renders_live() {
    let site = header_site("");
    let hostile = "<script>alert(1)</script>";
    let incident = incidents::open(
        &site.state,
        NewIncident {
            kind: IncidentKind::Incident,
            title: format!("{hostile} \" onmouseover=\"alert(2)"),
            impact: Impact::Minor,
            components: Vec::new(),
            status: None,
            message: format!(
                "{hostile}\n\n<img src=x onerror=alert(3)>\n\n[x](javascript:alert(4))"
            ),
            scheduled_start: None,
            scheduled_end: None,
        },
        Actor::person("<b>mallory</b>@example.com"),
    )
    .await
    .unwrap();
    for uri in [
        "/admin/".to_owned(),
        format!("/admin/incidents/{}", incident.id),
    ] {
        let request = from(PROXY, Method::GET, &uri)
            .header("x-forwarded-email", ALICE)
            .body(Body::empty())
            .unwrap();
        let (status, response) = send(&site.router, request).await;
        assert_eq!(status, StatusCode::OK);
        let page = text(response).await;
        for live in [
            "<script>alert",
            "<img",
            "\" onmouseover=",
            "<a href=\"javascript",
            "<b>mallory",
        ] {
            assert!(
                !page.contains(live),
                "{uri} rendered {live:?} live:\n{page}"
            );
        }
        // askama escapes the title; the body is markdown, sanitized.
        assert!(
            page.contains(
                "&#60;script&#62;alert(1)&#60;/script&#62; &#34; onmouseover=&#34;alert(2)"
            ),
            "{uri}"
        );
    }
    let request = from(
        PROXY,
        Method::GET,
        &format!("/admin/incidents/{}", incident.id),
    )
    .header("x-forwarded-email", ALICE)
    .body(Body::empty())
    .unwrap();
    let page = text(send(&site.router, request).await.1).await;
    assert!(page.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
}

// --- Assets and headers --------------------------------------------------

#[tokio::test]
async fn assets_are_served_by_content_hash_and_cached_forever() {
    let site = site(config(""), None);
    for (name, content_type) in [
        ("tocsin.css", "text/css; charset=utf-8"),
        ("htmx.min.js", "text/javascript; charset=utf-8"),
        ("htmx-ext-sse.min.js", "text/javascript; charset=utf-8"),
    ] {
        let url = super::assets::url(name);
        let request = Request::get(url).body(Body::empty()).unwrap();
        let (status, response) = send(&site.router, request).await;
        assert_eq!(status, StatusCode::OK, "{url}");
        assert_eq!(response.headers()[header::CONTENT_TYPE], content_type);
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        // A stale or made-up hash is not the current file.
        let stale = format!("/static/0000000000000000/{name}");
        let request = Request::get(&stale).body(Body::empty()).unwrap();
        assert_eq!(send(&site.router, request).await.0, StatusCode::NOT_FOUND);
    }
}

#[test]
fn vendored_htmx_is_the_pinned_release() {
    // static/README.md: htmx 2.0.11, from the npm tarball. Upgrading means
    // replacing the file and this hash together.
    let digest = ring::digest::digest(
        &ring::digest::SHA256,
        include_bytes!("../../static/htmx.min.js"),
    );
    assert_eq!(super::assets::hex(digest.as_ref()), HTMX_SHA256);
}

const HTMX_SHA256: &str = "d6fdc75f204e6bdefa99b69bf1e6d4ac69b8a364f77929f45c13476b4000f717";

#[test]
fn vendored_htmx_sse_extension_is_the_pinned_release() {
    // static/README.md: htmx-ext-sse 2.2.4, from the npm tarball. Upgrading
    // means replacing the file and this hash together.
    let digest = ring::digest::digest(
        &ring::digest::SHA256,
        include_bytes!("../../static/htmx-ext-sse.min.js"),
    );
    assert_eq!(super::assets::hex(digest.as_ref()), HTMX_SSE_SHA256);
}

const HTMX_SSE_SHA256: &str = "98a46496de0c3605fbffdce9167ba427bdd9553184f83f149c261891a92c0136";

#[tokio::test]
async fn pages_carry_security_headers() {
    let site = header_site("");
    let request = from(PROXY, Method::GET, "/admin/")
        .header("x-forwarded-email", ALICE)
        .body(Body::empty())
        .unwrap();
    let (_, response) = send(&site.router, request).await;
    let headers = response.headers();
    assert_eq!(
        headers[header::CONTENT_SECURITY_POLICY],
        super::page::CONTENT_SECURITY_POLICY
    );
    assert_eq!(headers[header::X_FRAME_OPTIONS], "DENY");
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    let page = text(response).await;
    // Everything the page loads is its own, under a content hash.
    assert!(page.contains(super::assets::url("tocsin.css")));
    assert!(!page.contains("https://"));
    // Refusals are pages too, with the same headers.
    let request = from(OUTSIDER, Method::GET, "/admin/")
        .body(Body::empty())
        .unwrap();
    let (status, response) = send(&site.router, request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(response.headers()[header::X_FRAME_OPTIONS], "DENY");
    assert!(text(response).await.contains("You are not signed in"));
}

// --- Forms ---------------------------------------------------------------

#[tokio::test]
async fn invalid_forms_come_back_with_errors_and_what_was_typed() {
    let site = header_site("");
    let token = csrf_token(&site, ALICE).await;
    let body = form(&[
        ("csrf", &token),
        ("title", ""),
        ("impact", "major"),
        ("status", "investigating"),
        ("components", "api"),
        ("components", "nonexistent"),
        ("message", "Typed <carefully>"),
    ]);
    let (status, response) = send(&site.router, post_form("/admin/incidents", ORIGIN, body)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let page = text(response).await;
    assert!(page.contains("The incident was not opened"));
    assert!(page.contains("title"), "{page}");
    assert!(page.contains("nonexistent"), "{page}");
    assert!(page.contains("Typed &#60;carefully&#62;"));
    assert!(page.contains(r#"value="api" checked"#));
    assert!(page.contains(r#"<option value="major" selected>"#));
    assert_eq!(incident_count(&site.state).await, 0);

    let body = form(&[("csrf", &token), ("status", "sideways"), ("message", "x")]);
    let (status, response) = send(
        &site.router,
        post_form("/admin/incidents/1/updates", ORIGIN, body),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{}", text(response).await);
}

// --- End to end ----------------------------------------------------------

/// alice's browser, reaching the site over a real socket through a proxy
/// on 127.0.0.1.
struct Browser {
    client: reqwest::Client,
    base: String,
}

impl Browser {
    async fn open(site: &Site) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = site
            .router
            .clone()
            .into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let _ = rustls::crypto::ring::default_provider().install_default();
        Browser {
            client: reqwest::Client::new(),
            base,
        }
    }

    /// Status, final path after redirects, and page.
    async fn read(response: reqwest::Response) -> (StatusCode, String, String) {
        let status = response.status();
        let path = response.url().path().to_owned();
        (status, path, response.text().await.unwrap())
    }

    async fn get(&self, path: &str) -> (StatusCode, String, String) {
        let response = self
            .client
            .get(format!("{}{path}", self.base))
            .header("x-forwarded-email", ALICE)
            .send()
            .await
            .unwrap();
        Self::read(response).await
    }

    async fn post(&self, path: &str, fields: &[(&str, &str)]) -> (StatusCode, String, String) {
        let response = self
            .client
            .post(format!("{}{path}", self.base))
            .header("x-forwarded-email", ALICE)
            // The browser is on the public site; the proxy forwards here.
            .header(header::ORIGIN, ORIGIN)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(form(fields))
            .send()
            .await
            .unwrap();
        Self::read(response).await
    }
}

#[tokio::test]
async fn admin_creates_updates_and_resolves_over_http() {
    let config = config("mode = \"trusted_header\"\ntrusted_proxies = [\"127.0.0.1/32\"]");
    let admin = AdminAuth::from_config(&config).unwrap();
    let site = site(config, admin);
    let mut events = site.state.events.subscribe();
    let browser = Browser::open(&site).await;
    let (status, _, page) = browser.get("/admin/").await;
    assert_eq!(status, StatusCode::OK);
    let token = hidden_csrf(&page);

    // Open: the redirect lands on the new incident's page.
    let (status, path, page) = browser
        .post(
            "/admin/incidents",
            &[
                ("csrf", &token),
                ("title", "Checkout is failing"),
                ("impact", "major"),
                ("components", "api"),
                ("components", "web"),
                ("status", "investigating"),
                ("message", "We are **looking into it**."),
            ],
        )
        .await;
    assert_eq!(
        (status, path.as_str()),
        (StatusCode::OK, "/admin/incidents/1")
    );
    assert!(page.contains("<strong>looking into it</strong>"));

    // Update.
    let update = [
        ("csrf", token.as_str()),
        ("status", "monitoring"),
        ("message", "A fix is deployed."),
    ];
    let (status, path, page) = browser.post("/admin/incidents/1/updates", &update).await;
    assert_eq!(
        (status, path.as_str()),
        (StatusCode::OK, "/admin/incidents/1")
    );

    // Resolve, with the form the page offers.
    assert!(page.contains(r#"<input type="hidden" name="status" value="resolved">"#));
    let resolve = [
        ("csrf", token.as_str()),
        ("status", "resolved"),
        ("message", "All clear."),
    ];
    let (status, _, page) = browser.post("/admin/incidents/1/updates", &resolve).await;
    assert_eq!(status, StatusCode::OK);
    // A reload of a form's address lands on a page, not the JSON API.
    for (path, lands_on) in [
        ("/admin/incidents", "/admin/"),
        ("/admin/incidents/1/updates", "/admin/incidents/1"),
    ] {
        let (status, landed, _) = browser.get(path).await;
        assert_eq!((status, landed.as_str()), (StatusCode::OK, lands_on));
    }
    assert!(
        !page.contains("<h2>Resolve</h2>"),
        "a resolved incident offers no resolve form"
    );

    // The store has what the JSON API would have written, by alice.
    let (incident, updates) = rows(&site.state).await;
    assert_eq!(
        incident,
        (
            "Checkout is failing".to_owned(),
            "major".to_owned(),
            "resolved".to_owned(),
            "person".to_owned(),
            ALICE.to_owned(),
            incident.5,
        )
    );
    assert!(incident.5.is_some(), "resolved_at is set");
    let expected = [
        ("investigating", "We are **looking into it**."),
        ("monitoring", "A fix is deployed."),
        ("resolved", "All clear."),
    ]
    .map(|(status, message)| {
        (
            status.to_owned(),
            message.to_owned(),
            "person".to_owned(),
            ALICE.to_owned(),
        )
    });
    assert_eq!(updates, expected);
    let stored = site.state.store.incident(1).await.unwrap().unwrap();
    assert_eq!(stored.components, ["api", "web"]);

    // And the same events as the JSON API: one opened, two updated.
    let received: Vec<_> = std::iter::from_fn(|| events.try_recv().ok()).collect();
    assert!(
        matches!(
            received.as_slice(),
            [
                Event::IncidentCreated { .. },
                Event::IncidentUpdated { .. },
                Event::IncidentUpdated { .. }
            ]
        ),
        "{} events",
        received.len()
    );
}

type IncidentRow = (String, String, String, String, String, Option<i64>);
type UpdateRow = (String, String, String, String);

/// Incident 1 and its updates, straight from the database.
async fn rows(state: &AppState) -> (IncidentRow, Vec<UpdateRow>) {
    state
        .store
        .read(|conn| {
            let incident: (String, String, String, String, String, Option<i64>) = conn.query_row(
                "SELECT title, impact, status, created_by_kind, created_by, resolved_at \
                 FROM incident WHERE id = 1",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )?;
            let mut statement = conn.prepare(
                "SELECT status, message, created_by_kind, created_by FROM incident_update \
                 WHERE incident_id = 1 ORDER BY id",
            )?;
            let updates = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok((incident, updates))
        })
        .await
        .unwrap()
}

// --- Timeline ------------------------------------------------------------

/// The admin timeline shows every project's deploys, private ones too;
/// the public release pages beside it do not.
#[tokio::test]
async fn the_admin_timeline_shows_private_deploys_to_an_admin_only() {
    let config = config_with(
        "mode = \"trusted_header\"\ntrusted_proxies = [\"10.0.0.0/8\"]",
        "[[project]]\nid = \"internal\"\nname = \"Internal\"\npublic = false\ncomponents = [\"api\"]\n",
    );
    let admin = AdminAuth::from_config(&config).unwrap();
    let site = site(config, admin);
    http::releases::deployed(
        &site.state,
        "internal".into(),
        "v9.9.9".into(),
        tocsin_core::release::DeployStamp::default(),
        Actor::token("deploys"),
    )
    .await
    .unwrap();

    for uri in ["/admin/timeline", "/admin/timeline?component=api"] {
        let request = from(PROXY, Method::GET, uri)
            .header("x-forwarded-email", ALICE)
            .body(Body::empty())
            .unwrap();
        let (status, response) = send(&site.router, request).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        let page = text(response).await;
        assert!(
            page.contains("Internal v9.9.9 to production"),
            "{uri}: {page}"
        );
        assert!(page.contains("deployed before it was published"), "{uri}");
    }
    let request = from(PROXY, Method::GET, "/admin/timeline?component=web")
        .header("x-forwarded-email", ALICE)
        .body(Body::empty())
        .unwrap();
    let page = text(send(&site.router, request).await.1).await;
    assert!(page.contains("Nothing in this window."), "{page}");

    let request = from(OUTSIDER, Method::GET, "/admin/timeline")
        .body(Body::empty())
        .unwrap();
    let (status, response) = send(&site.router, request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(!text(response).await.contains("v9.9.9"));
    let request = from(OUTSIDER, Method::GET, "/api/v1/timeline")
        .body(Body::empty())
        .unwrap();
    let body = text(send(&site.router, request).await.1).await;
    assert!(!body.contains("v9.9.9"), "{body}");
}

/// More than a page of events: the admin sees the newest, and is told so.
/// An outage whose check was removed while out reads as withdrawn, not
/// as one that ended.
#[tokio::test]
async fn the_admin_timeline_tells_a_withdrawn_outage_from_a_recovery() {
    let config = config_with(
        "mode = \"trusted_header\"\ntrusted_proxies = [\"10.0.0.0/8\"]",
        "",
    );
    let admin = AdminAuth::from_config(&config).unwrap();
    let site = site(config, admin);
    let store = &site.state.store;
    let at = |minutes: i64| http::now().as_millis() - minutes * 60_000;
    let outage = |check: &str, minutes: i64| tocsin_store::NewOutage {
        node_id: "node-a".into(),
        subject_kind: tocsin_store::SubjectKind::Check,
        check_id: check.into(),
        component_id: Some("api".into()),
        health: tocsin_core::Health::Down,
        started_at_ms: at(minutes),
        alerted: true,
    };
    let recovered = store.open_outage(outage("db", 60)).await.unwrap();
    store.end_outage(recovered, at(50)).await.unwrap();
    let withdrawn = store.open_outage(outage("legacy", 40)).await.unwrap();
    store.withdraw_outage(withdrawn, at(30)).await.unwrap();

    let request = from(PROXY, Method::GET, "/admin/timeline")
        .header("x-forwarded-email", ALICE)
        .body(Body::empty())
        .unwrap();
    let (status, response) = send(&site.router, request).await;
    assert_eq!(status, StatusCode::OK);
    let page = text(response).await;
    let line = |check: &str| {
        let start = page.find(&format!("{check} down")).expect(check);
        let item = &page[start..];
        item[..item.find("</li>").unwrap()].to_owned()
    };
    assert!(line("legacy").contains("· withdrawn "), "{page}");
    assert!(!line("legacy").contains("· ended "), "{page}");
    assert!(line("db").contains("· ended "), "{page}");
}

#[tokio::test]
async fn the_admin_timeline_shows_the_newest_page_and_says_so() {
    let config = config_with(
        "mode = \"trusted_header\"\ntrusted_proxies = [\"10.0.0.0/8\"]",
        "[[project]]\nid = \"api\"\nname = \"API\"\n",
    );
    let admin = AdminAuth::from_config(&config).unwrap();
    let site = site(config, admin);
    for index in 0..510 {
        http::releases::deployed(
            &site.state,
            "api".into(),
            format!("r{index}"),
            tocsin_core::release::DeployStamp::default(),
            Actor::token("deploys"),
        )
        .await
        .unwrap();
    }
    let request = from(PROXY, Method::GET, "/admin/timeline")
        .header("x-forwarded-email", ALICE)
        .body(Body::empty())
        .unwrap();
    let (status, response) = send(&site.router, request).await;
    assert_eq!(status, StatusCode::OK);
    let page = text(response).await;
    assert!(page.contains("these are the newest 500."), "{page}");
    let newest = page.find("API r509 to production").unwrap();
    let oldest_shown = page.find("API r10 to production").unwrap();
    assert!(newest < oldest_shown, "newest first");
    for hidden in 0..10 {
        assert!(
            !page.contains(&format!("API r{hidden} to production")),
            "r{hidden}"
        );
    }
}
