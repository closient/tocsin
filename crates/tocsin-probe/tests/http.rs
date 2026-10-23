// SPDX-License-Identifier: AGPL-3.0-only

//! The `http` probe against a local axum server, and HTTPS against a local
//! TLS server with an rcgen certificate.

mod support;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use axum::Router;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use tocsin_core::Health;
use tocsin_core::probe::{BODY_LIMIT, Facts, HttpFacts, ProbeErrorKind, ProbeOutcome};
use tocsin_probe::{ProbeContext, run};
use tokio::net::TcpListener;

use support::{Authority, check, https_server, https_server_redirecting};

const COMPONENTS: &str = r#"{
  "page": {"id": "kctbh9vrtdwd", "name": "GitHub"},
  "components": [
    {"id": "8l4ygp009s5s", "name": "Git Operations", "status": "operational"},
    {"id": "brv1bkgrwx7q", "name": "API Requests", "status": "degraded_performance"}
  ]
}"#;

/// The marker at the very end of `/big`, past the body limit.
const TAIL: &str = "TAIL-PAST-THE-LIMIT";

async fn server() -> u16 {
    let app = Router::new()
        .route(
            "/components.json",
            get(|| async {
                (
                    [
                        (header::CONTENT_TYPE, "application/json"),
                        (header::HeaderName::from_static("x-served-by"), "fixture"),
                    ],
                    COMPONENTS,
                )
            }),
        )
        .route(
            "/moved",
            get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/components.json")]) }),
        )
        .route(
            "/big",
            get(|| async { format!("{}{TAIL}", "a".repeat(BODY_LIMIT)) }),
        )
        .route(
            "/unavailable",
            get(|| async { (StatusCode::SERVICE_UNAVAILABLE, "down for maintenance") }),
        )
        .route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                "late"
            }),
        )
        .route("/whoami", get(whoami).post(echo))
        .route("/host", get(host));
    serve(app, TcpListener::bind("127.0.0.1:0").await.expect("bind"))
}

fn serve(app: Router, listener: TcpListener) -> u16 {
    let port = listener.local_addr().expect("address").port();
    tokio::spawn(async move { axum::serve(listener, app).await });
    port
}

/// A server answering `/host` with the request's `Host`, on one address.
async fn host_server(address: SocketAddr) -> u16 {
    let listener = TcpListener::bind(address)
        .await
        .unwrap_or_else(|error| panic!("cannot listen on {address}: {error}"));
    serve(Router::new().route("/host", get(host)), listener)
}

async fn host(headers: HeaderMap) -> String {
    headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("-")
        .to_owned()
}

/// Echoes the request headers the probe controls.
async fn whoami(headers: HeaderMap) -> impl IntoResponse {
    let get = |name: header::HeaderName| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("-")
            .to_owned()
    };
    format!(
        "authorization={} user-agent={} x-custom={}",
        get(header::AUTHORIZATION),
        get(header::USER_AGENT),
        headers
            .get("x-custom")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("-"),
    )
}

async fn echo(body: String) -> String {
    format!("posted {body}")
}

fn context() -> ProbeContext {
    ProbeContext::with_roots(rustls::RootCertStore::empty())
}

fn http(outcome: &ProbeOutcome) -> &HttpFacts {
    match &outcome.facts {
        Some(Facts::Http(facts)) => facts,
        other => panic!("expected HTTP facts, got {other:?} ({:?})", outcome.error),
    }
}

#[tokio::test]
async fn facts_and_assertions_including_a_statuspage_filter() {
    let port = server().await;
    let check = check(&format!(
        r#"
[[check]]
id = "github"
type = "http"
target = "http://127.0.0.1:{port}/components.json"
timeout = "5s"

[[check.assert]]
status = [200]

[[check.assert]]
header = {{ name = "X-Served-By", eq = "fixture" }}

[[check.assert]]
json = {{ path = "$.components[?@.name=='Git Operations'].status", eq = "operational" }}

[[check.assert]]
json = {{ path = "$.components[?@.name=='API Requests'].status", eq = "operational" }}
on_fail = "degraded"

[[check.assert]]
body = {{ regex = "kctbh9vrtdwd" }}

[[check.assert]]
latency_ms = {{ lt = 5000 }}
"#
    ));
    let outcome = run(&check, &context()).await;
    let facts = http(&outcome);
    assert_eq!(facts.status, 200);
    assert_eq!(facts.headers["x-served-by"], vec!["fixture".to_owned()]);
    assert_eq!(facts.body, COMPONENTS);
    assert_eq!(facts.body_bytes, COMPONENTS.len());
    assert!(!facts.body_truncated);
    assert_eq!(facts.certificate, None);
    assert_eq!(facts.redirect_location, None);
    let passed: Vec<bool> = outcome
        .assertions
        .iter()
        .map(|verdict| verdict.passed)
        .collect();
    assert_eq!(passed, [true, true, true, false, true, true]);
    assert_eq!(outcome.assertions[3].actual, "degraded_performance");
    assert_eq!(outcome.health, Health::Degraded);
    assert_eq!(outcome.error, None);
}

#[tokio::test]
async fn redirects_are_not_followed_unless_asked() {
    let port = server().await;
    let not_followed = check(&format!(
        r#"
[[check]]
id = "moved"
type = "http"
target = "http://127.0.0.1:{port}/moved"
"#
    ));
    let outcome = run(&not_followed, &context()).await;
    let facts = http(&outcome);
    assert_eq!(facts.status, 302);
    assert_eq!(facts.redirect_location.as_deref(), Some("/components.json"));
    assert!(facts.url.ends_with("/moved"));
    // The implicit status verdict accepts 3xx.
    assert_eq!(outcome.health, Health::Up);
    assert!(outcome.assertions[0].implicit);

    let followed = check(&format!(
        r#"
[[check]]
id = "moved"
type = "http"
target = "http://127.0.0.1:{port}/moved"

[check.http]
follow_redirects = true
"#
    ));
    let outcome = run(&followed, &context()).await;
    let facts = http(&outcome);
    assert_eq!(facts.status, 200);
    assert_eq!(facts.redirect_location, None);
    assert!(facts.url.ends_with("/components.json"));
}

#[tokio::test]
async fn certificate_facts_follow_the_final_scheme() {
    let authority = Authority::new("Probe Test CA");
    let leaf = authority.leaf(&["localhost"], 30);
    let trusted = ProbeContext::with_roots(authority.roots());
    let plain = server().await;
    let secure = https_server_redirecting(
        &leaf,
        Some(format!("http://127.0.0.1:{plain}/components.json")),
    )
    .await;
    let followed = |target: String| {
        check(&format!(
            "[[check]]\nid = \"moved\"\ntype = \"http\"\ntarget = \"{target}\"\n\n[check.http]\nfollow_redirects = true\n"
        ))
    };

    // https -> http: the response came over plain HTTP, so no certificate,
    // although the first hop had one.
    let outcome = run(
        &followed(format!("https://localhost:{secure}/away")),
        &trusted,
    )
    .await;
    let facts = http(&outcome);
    assert_eq!(facts.status, 200);
    assert!(facts.url.starts_with("http://"), "{}", facts.url);
    assert_eq!(facts.certificate, None);

    // http -> https: the response came over TLS, so its certificate shows.
    let plain_to_secure = Router::new().route(
        "/up",
        get(move || async move {
            (
                StatusCode::FOUND,
                [(header::LOCATION, format!("https://localhost:{secure}/"))],
            )
        }),
    );
    let redirecting = serve(
        plain_to_secure,
        TcpListener::bind("127.0.0.1:0").await.expect("bind"),
    );
    let outcome = run(
        &followed(format!("http://127.0.0.1:{redirecting}/up")),
        &trusted,
    )
    .await;
    let facts = http(&outcome);
    assert_eq!(facts.status, 200);
    assert!(facts.url.starts_with("https://"), "{}", facts.url);
    let certificate = facts.certificate.as_ref().expect("certificate facts");
    assert_eq!(certificate.days_remaining, Some(30));
}

#[tokio::test]
async fn server_name_with_an_ipv6_literal_target() {
    let port = host_server(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 0)).await;
    let check = check(&format!(
        r#"
[[check]]
id = "v6"
type = "http"
target = "http://[::1]:{port}/host"

[check.http]
server_name = "status.example.test"
"#
    ));
    let outcome = run(&check, &context()).await;
    assert_eq!(outcome.error, None);
    assert_eq!(http(&outcome).body, format!("status.example.test:{port}"));
}

/// With `server_name`, a target name that resolves to several addresses
/// is reached on any of them: here `localhost`, with a server on only its
/// IPv4 address, then on only its IPv6 one.
#[tokio::test]
async fn server_name_tries_every_address_of_the_target() {
    let families: Vec<bool> = tokio::net::lookup_host(("localhost", 80))
        .await
        .expect("localhost resolves")
        .map(|address| address.is_ipv6())
        .collect();
    if !(families.contains(&true) && families.contains(&false)) {
        assert!(
            std::env::var_os("CI").is_none(),
            "CI must resolve localhost to both 127.0.0.1 and ::1, but got {families:?}"
        );
        eprintln!("skipped: localhost does not resolve to both IPv4 and IPv6 here");
        return;
    }
    for ip in [
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(Ipv6Addr::LOCALHOST),
    ] {
        let port = host_server(SocketAddr::new(ip, 0)).await;
        let check = check(&format!(
            "[[check]]\nid = \"any\"\ntype = \"http\"\ntarget = \"http://localhost:{port}/host\"\n\n[check.http]\nserver_name = \"status.example.test\"\n"
        ));
        let outcome = run(&check, &context()).await;
        assert_eq!(outcome.error, None, "server on {ip} only");
        assert_eq!(http(&outcome).body, format!("status.example.test:{port}"));
    }
}

#[tokio::test]
async fn the_body_is_capped_at_64_kib() {
    let port = server().await;
    let check = check(&format!(
        r#"
[[check]]
id = "big"
type = "http"
target = "http://127.0.0.1:{port}/big"

[[check.assert]]
body = {{ contains = "{TAIL}" }}
"#
    ));
    let outcome = run(&check, &context()).await;
    let facts = http(&outcome);
    assert_eq!(BODY_LIMIT, 64 * 1024);
    assert_eq!(facts.body_bytes, BODY_LIMIT);
    assert_eq!(facts.body.len(), BODY_LIMIT);
    assert!(facts.body_truncated);
    assert!(!outcome.assertions[0].passed);
    assert_eq!(outcome.health, Health::Down);
}

#[tokio::test]
async fn an_error_status_fails_the_implicit_verdict() {
    let port = server().await;
    let check = check(&format!(
        r#"
[[check]]
id = "unavailable"
type = "http"
target = "http://127.0.0.1:{port}/unavailable"
"#
    ));
    let outcome = run(&check, &context()).await;
    assert_eq!(http(&outcome).status, 503);
    assert_eq!(outcome.health, Health::Down);
    assert_eq!(outcome.assertions[0].condition, "status");
    assert!(outcome.assertions[0].implicit);
    assert!(!outcome.assertions[0].passed);
}

#[tokio::test]
async fn headers_and_credentials_come_from_settings_and_secrets() {
    let port = server().await;
    let bearer = check(&format!(
        r#"
[[check]]
id = "bearer"
type = "http"
target = "http://127.0.0.1:{port}/whoami"

[check.http]
bearer_token = "${{ENV:API_TOKEN}}"
headers = {{ X-Custom = "yes" }}
"#
    ));
    let outcome = run(&bearer, &context()).await;
    let body = &http(&outcome).body;
    assert!(body.contains("authorization=Bearer s3cret-token"), "{body}");
    assert!(body.contains("x-custom=yes"), "{body}");
    assert!(
        body.contains(&format!("user-agent=tocsin/{}", env!("CARGO_PKG_VERSION"))),
        "{body}"
    );

    let basic = check(&format!(
        r#"
[[check]]
id = "basic"
type = "http"
target = "http://127.0.0.1:{port}/whoami"

[check.http]
basic_auth = {{ username = "monitor", password = "${{ENV:API_PASSWORD}}" }}
"#
    ));
    let outcome = run(&basic, &context()).await;
    // monitor:hunter2
    assert!(
        http(&outcome)
            .body
            .contains("authorization=Basic bW9uaXRvcjpodW50ZXIy"),
        "{}",
        http(&outcome).body
    );

    let posting = check(&format!(
        r#"
[[check]]
id = "post"
type = "http"
target = "http://127.0.0.1:{port}/whoami"

[check.http]
method = "POST"
body = "ping"
"#
    ));
    let outcome = run(&posting, &context()).await;
    assert_eq!(http(&outcome).body, "posted ping");
}

#[tokio::test]
async fn a_timeout_is_down() {
    let port = server().await;
    let check = check(&format!(
        r#"
[[check]]
id = "slow"
type = "http"
target = "http://127.0.0.1:{port}/slow"
timeout = "200ms"
"#
    ));
    let outcome = run(&check, &context()).await;
    assert_eq!(outcome.health, Health::Down);
    assert_eq!(outcome.facts, None);
    assert_eq!(outcome.assertions.len(), 0);
    let error = outcome.error.expect("a timeout error");
    assert_eq!(error.kind, ProbeErrorKind::Timeout);
    assert!(outcome.duration_ms < 2000, "took {}ms", outcome.duration_ms);
}

#[tokio::test]
async fn a_refused_connection_is_down() {
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        listener.local_addr().expect("address").port()
    };
    let check = check(&format!(
        r#"
[[check]]
id = "refused"
type = "http"
target = "http://127.0.0.1:{port}/"
"#
    ));
    let outcome = run(&check, &context()).await;
    assert_eq!(outcome.health, Health::Down);
    assert_eq!(
        outcome.error.expect("an error").kind,
        ProbeErrorKind::Connect
    );
}

#[tokio::test]
async fn https_reports_the_certificate_and_enforces_it_by_default() {
    let authority = Authority::new("Probe Test CA");
    let leaf = authority.leaf(&["localhost"], 30);
    let port = https_server(&leaf).await;
    let trusted = ProbeContext::with_roots(authority.roots());

    // server_name: connect to 127.0.0.1 but name localhost.
    let check = check(&format!(
        r#"
[[check]]
id = "secure"
type = "http"
target = "https://127.0.0.1:{port}/"

[check.http]
server_name = "localhost"

[[check.assert]]
certificate_days_remaining = {{ gte = 14 }}
"#
    ));
    let outcome = run(&check, &trusted).await;
    let facts = http(&outcome);
    assert_eq!(facts.body, format!("hello localhost:{port}"));
    let certificate = facts.certificate.as_ref().expect("certificate facts");
    assert_eq!(certificate.server_name, "localhost");
    assert!(certificate.chain_valid);
    assert!(certificate.hostname_valid);
    assert_eq!(certificate.days_remaining, Some(30));
    assert_eq!(certificate.chain.len(), 2);
    assert_eq!(
        certificate.chain[0].subject_alternative_names,
        ["localhost"]
    );
    assert_eq!(outcome.health, Health::Up);

    // The same server, from a probe that does not trust its authority.
    let untrusted = ProbeContext::with_roots(Authority::new("Other CA").roots());
    let outcome = run(&check, &untrusted).await;
    assert_eq!(outcome.health, Health::Down);
    assert_eq!(
        outcome.error.expect("a TLS error").kind,
        ProbeErrorKind::Tls
    );
}

#[tokio::test]
async fn https_without_verification_still_reports_the_certificate() {
    let authority = Authority::new("Probe Test CA");
    let leaf = authority.leaf(&["localhost"], 30);
    let port = https_server(&leaf).await;
    let untrusted = ProbeContext::with_roots(Authority::new("Other CA").roots());
    let check = check(&format!(
        r#"
[[check]]
id = "self-signed"
type = "http"
target = "https://127.0.0.1:{port}/"

[check.http]
verify_certificate = false
server_name = "localhost"

[[check.assert]]
chain_valid = true
on_fail = "degraded"
"#
    ));
    let outcome = run(&check, &untrusted).await;
    let facts = http(&outcome);
    assert_eq!(facts.status, 200);
    let certificate = facts.certificate.as_ref().expect("certificate facts");
    assert!(!certificate.chain_valid);
    assert!(certificate.hostname_valid);
    assert!(certificate.chain_error.is_some());
    assert_eq!(outcome.health, Health::Degraded);
}
