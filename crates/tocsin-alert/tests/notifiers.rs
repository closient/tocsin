// SPDX-License-Identifier: AGPL-3.0-only

//! Every notifier against a local mock of its service: each sends a fire
//! and then its resolve, and the receiver can tell they are the same alert.
//! Nothing here touches the network beyond the loopback interface.

use std::time::Duration;

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tocsin_alert::notifiers::alertmanager::Alertmanager;
use tocsin_alert::notifiers::discord::Discord;
use tocsin_alert::notifiers::email::Email;
use tocsin_alert::notifiers::ntfy::Ntfy;
use tocsin_alert::notifiers::pushover::{Priorities, Pushover};
use tocsin_alert::notifiers::slack::Slack;
use tocsin_alert::notifiers::telegram::Telegram;
use tocsin_alert::notifiers::webhook::{self, SIGNATURE_HEADER, TIMESTAMP_HEADER, Webhook};
use tocsin_alert::{Alert, AlertEvent, Notifier};
use tocsin_core::Health;
use tocsin_store::SubjectKind;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

const STARTED: i64 = 1_800_000_000_000;
const ENDED: i64 = STARTED + 90_000;
const IDENTITY: &str = "node-a/check/api";
const IDENTITY_LINE: &str = "Alert: node-a/check/api";

fn alert(event: AlertEvent) -> Alert {
    let resolve = event == AlertEvent::Resolve;
    Alert {
        event,
        node_id: "node-a".into(),
        subject_kind: SubjectKind::Check,
        subject_id: "api".into(),
        subject_name: "Public API".into(),
        component_id: Some("web".into()),
        component_name: Some("Website".into()),
        health: Health::Down,
        started_at_ms: STARTED,
        ended_at_ms: resolve.then_some(ENDED),
        sent_at_ms: if resolve { ENDED } else { STARTED },
        delivery_id: Some(if resolve { 2 } else { 1 }),
        test: false,
    }
}

/// Sends the fire and then the resolve, both of which must be accepted.
async fn fire_then_resolve(notifier: &dyn Notifier) {
    for event in [AlertEvent::Fire, AlertEvent::Resolve] {
        notifier
            .send(&alert(event))
            .await
            .unwrap_or_else(|error| panic!("{event:?} refused: {}", error.message));
    }
}

/// A client like the binary's, for loopback mocks.
fn client() -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

/// One request the mock received.
#[derive(Debug)]
struct Request {
    method: Method,
    path: String,
    headers: HeaderMap,
    body: Bytes,
}

impl Request {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }

    fn text(&self) -> String {
        String::from_utf8(self.body.to_vec()).unwrap()
    }

    fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .unwrap_or_else(|| panic!("no {name} header"))
            .to_str()
            .unwrap()
    }

    /// A form field of a urlencoded body.
    fn field(&self, name: &str) -> String {
        url::form_urlencoded::parse(&self.body)
            .find(|(key, _)| key == name)
            .map_or_else(
                || panic!("no {name} field"),
                |(_, value)| value.into_owned(),
            )
    }
}

/// An HTTP server on a loopback port that answers every request with
/// `status` and `answer`, and hands each request to the receiver.
async fn mock(
    status: StatusCode,
    answer: &'static str,
) -> (String, mpsc::UnboundedReceiver<Request>) {
    let (sender, receiver) = mpsc::unbounded_channel();
    let app = axum::Router::new().fallback(
        move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
            let sender = sender.clone();
            async move {
                let _ = sender.send(Request {
                    method,
                    path: uri.to_string(),
                    headers,
                    body,
                });
                (status, answer)
            }
        },
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    (base, receiver)
}

async fn accepting() -> (String, mpsc::UnboundedReceiver<Request>) {
    mock(StatusCode::OK, "{\"ok\":true,\"status\":1}").await
}

/// The next two requests: the fire's and the resolve's.
async fn two(receiver: &mut mpsc::UnboundedReceiver<Request>) -> (Request, Request) {
    let fire = receiver.recv().await.unwrap();
    let resolve = receiver.recv().await.unwrap();
    (fire, resolve)
}

#[tokio::test]
async fn slack_posts_the_fire_and_its_resolve_for_one_alert() {
    let (base, mut requests) = accepting().await;
    let slack = Slack::new(client(), format!("{base}/services/T0/B0/token"));
    fire_then_resolve(&slack).await;

    let (fire, resolve) = two(&mut requests).await;
    for request in [&fire, &resolve] {
        assert_eq!(request.method, Method::POST);
        assert_eq!(request.path, "/services/T0/B0/token");
        let text = request.json()["blocks"][0]["text"]["text"].to_string();
        assert!(text.contains(IDENTITY_LINE), "{text}");
    }
    assert!(
        fire.json()["text"]
            .as_str()
            .unwrap()
            .contains("[DOWN] Public API")
    );
    assert!(
        resolve.json()["text"]
            .as_str()
            .unwrap()
            .contains("[RESOLVED] Public API")
    );
}

#[tokio::test]
async fn discord_posts_the_fire_and_its_resolve_without_mentions() {
    let (base, mut requests) = accepting().await;
    let discord = Discord::new(client(), format!("{base}/api/webhooks/1/token"));
    fire_then_resolve(&discord).await;

    let (fire, resolve) = two(&mut requests).await;
    for request in [&fire, &resolve] {
        let body = request.json();
        assert!(body["content"].as_str().unwrap().contains(IDENTITY_LINE));
        assert_eq!(body["allowed_mentions"]["parse"], Value::Array(Vec::new()));
    }
    assert!(
        fire.json()["content"]
            .as_str()
            .unwrap()
            .starts_with("**[DOWN]")
    );
    assert!(
        resolve.json()["content"]
            .as_str()
            .unwrap()
            .starts_with("**[RESOLVED]")
    );
}

#[tokio::test]
async fn ntfy_publishes_the_fire_and_its_resolve_with_the_token() {
    let (base, mut requests) = accepting().await;
    let ntfy = Ntfy::new(client(), format!("{base}/tocsin"), Some("tk_secret".into()));
    fire_then_resolve(&ntfy).await;

    let (fire, resolve) = two(&mut requests).await;
    for request in [&fire, &resolve] {
        assert_eq!(request.path, "/tocsin");
        assert_eq!(request.header("authorization"), "Bearer tk_secret");
        assert!(request.text().contains(IDENTITY_LINE));
    }
    assert_eq!(
        (fire.header("priority"), fire.header("tags")),
        ("4", "rotating_light")
    );
    assert_eq!(
        (resolve.header("priority"), resolve.header("tags")),
        ("3", "white_check_mark")
    );
    assert!(resolve.header("title").starts_with("[RESOLVED]"));
}

#[tokio::test]
async fn telegram_sends_the_fire_and_its_resolve_to_the_chat() {
    let (base, mut requests) = accepting().await;
    let telegram = Telegram::new(client(), &base, "123:bot-token", "-1001".into());
    fire_then_resolve(&telegram).await;

    let (fire, resolve) = two(&mut requests).await;
    for request in [&fire, &resolve] {
        assert_eq!(request.path, "/bot123:bot-token/sendMessage");
        assert_eq!(request.json()["chat_id"], "-1001");
        assert!(
            request.json()["text"]
                .as_str()
                .unwrap()
                .contains(IDENTITY_LINE)
        );
    }
    assert!(
        resolve.json()["text"]
            .as_str()
            .unwrap()
            .contains("after 1m 30s")
    );
}

#[tokio::test]
async fn pushover_maps_priorities_for_the_fire_and_its_resolve() {
    let (base, mut requests) = accepting().await;
    let pushover = Pushover::new(
        client(),
        &base,
        "app-token".into(),
        "user-key".into(),
        Priorities {
            down: 1,
            degraded: 0,
            resolved: -1,
        },
    );
    fire_then_resolve(&pushover).await;

    let (fire, resolve) = two(&mut requests).await;
    for request in [&fire, &resolve] {
        assert_eq!(request.path, "/1/messages.json");
        assert_eq!(request.field("token"), "app-token");
        assert_eq!(request.field("user"), "user-key");
        assert!(request.field("message").contains(IDENTITY_LINE));
    }
    assert_eq!(fire.field("priority"), "1");
    assert_eq!(resolve.field("priority"), "-1");
    assert!(resolve.field("title").starts_with("[RESOLVED]"));
}

#[tokio::test]
async fn an_emergency_pushover_alert_is_tagged_and_cancelled_by_its_tag() {
    let (base, mut requests) = accepting().await;
    let pushover = Pushover::new(
        client(),
        &base,
        "app-token".into(),
        "user-key".into(),
        Priorities {
            down: 2,
            degraded: 0,
            resolved: 0,
        },
    );
    // An FQDN node: dots and slashes in the identity.
    let fire = Alert {
        node_id: "node-a.example.com".into(),
        ..alert(AlertEvent::Fire)
    };
    let resolve = Alert {
        node_id: "node-a.example.com".into(),
        ..alert(AlertEvent::Resolve)
    };
    pushover.send(&fire).await.unwrap();
    pushover.cancel(&fire).await.unwrap();
    pushover.send(&resolve).await.unwrap();

    let (fired, cancel, resolved) = (
        requests.recv().await.unwrap(),
        requests.recv().await.unwrap(),
        requests.recv().await.unwrap(),
    );
    // What Pushover's cancel_by_tag needs: the tag the emergency carried,
    // as the last path segment, before `.json`.
    let tag = format!(
        "tocsin-{}",
        hex::encode(Sha256::digest(b"user-key\0node-a.example.com/check/api"))
    );
    assert_eq!(pushover.tag(&fire), tag);
    assert_eq!(fired.path, "/1/messages.json");
    assert_eq!(fired.field("priority"), "2");
    assert_eq!(fired.field("tags"), tag);
    assert!(fired.field("retry").parse::<u32>().unwrap() >= 30);
    assert_eq!(cancel.method, Method::POST);
    assert_eq!(cancel.path, format!("/1/receipts/cancel_by_tag/{tag}.json"));
    assert_eq!(cancel.field("token"), "app-token");
    // The resolve is a message of its own; it neither cancels nor is tagged.
    assert_eq!(resolved.path, "/1/messages.json");
    assert_eq!(resolved.field("priority"), "0");
    assert!(url::form_urlencoded::parse(&resolved.body).all(|(key, _)| key != "tags"));
}

#[tokio::test]
async fn a_pushover_tag_belongs_to_one_user_and_one_alert() {
    let pushover = |user: &str| {
        Pushover::new(
            client(),
            "http://127.0.0.1:9",
            "app-token".into(),
            user.into(),
            Priorities::default(),
        )
    };
    let api = alert(AlertEvent::Fire);
    let web = Alert {
        subject_id: "web".into(),
        ..alert(AlertEvent::Fire)
    };
    assert_ne!(pushover("alice").tag(&api), pushover("bob").tag(&api));
    assert_ne!(pushover("alice").tag(&api), pushover("alice").tag(&web));
    assert_eq!(
        pushover("alice").tag(&api),
        pushover("alice").tag(&alert(AlertEvent::Resolve))
    );
}

#[tokio::test]
async fn a_webhook_carries_one_identity_and_a_verifiable_signature() {
    let (base, mut requests) = accepting().await;
    let hook = Webhook::new(
        client(),
        format!("{base}/hooks/tocsin"),
        "whsec-test".into(),
        Some("bearer-test".into()),
    );
    fire_then_resolve(&hook).await;

    let (fire, resolve) = two(&mut requests).await;
    for request in [&fire, &resolve] {
        assert_eq!(request.header("authorization"), "Bearer bearer-test");
        // What a receiver does: recompute over the exact bytes received.
        let timestamp: i64 = request.header(TIMESTAMP_HEADER).parse().unwrap();
        assert_eq!(
            request.header(SIGNATURE_HEADER),
            webhook::signature(b"whsec-test", timestamp, &request.body)
        );
        assert_ne!(
            request.header(SIGNATURE_HEADER),
            webhook::signature(b"whsec-test", timestamp + 1, &request.body),
            "the timestamp is signed"
        );
        assert_eq!(request.json()["identity"], IDENTITY);
    }
    let (fire, resolve) = (fire.json(), resolve.json());
    assert_eq!(
        (fire["event"].as_str(), fire["status"].as_str()),
        (Some("fire"), Some("firing"))
    );
    assert_eq!(
        (resolve["event"].as_str(), resolve["status"].as_str()),
        (Some("resolve"), Some("resolved"))
    );
    assert_eq!(fire["started_at_ms"], resolve["started_at_ms"]);
    assert_eq!(resolve["duration_ms"], 90_000);
    assert_eq!(fire["duration_ms"], Value::Null);
}

#[tokio::test]
async fn alertmanager_holds_the_alert_open_and_ends_it_on_resolve() {
    let (base, mut requests) = accepting().await;
    let alertmanager = Alertmanager::new(client(), &format!("{base}/"), None);
    fire_then_resolve(&alertmanager).await;

    let (fire, resolve) = two(&mut requests).await;
    assert_eq!(fire.path, "/api/v2/alerts");
    let (fire, resolve) = (fire.json(), resolve.json());
    assert_eq!(fire[0]["labels"], resolve[0]["labels"]);
    assert_eq!(fire[0]["labels"]["alertname"], "TocsinCheckFailing");
    assert_eq!(fire[0]["labels"]["node"], "node-a");
    assert_eq!(fire[0]["labels"]["subject_kind"], "check");
    assert_eq!(fire[0]["labels"]["subject_id"], "api");
    assert_eq!(fire[0]["annotations"]["component"], "web");
    assert_eq!(fire[0]["startsAt"], resolve[0]["startsAt"]);
    // Firing: held open five minutes past the post, re-posted before then.
    assert_eq!(fire[0]["endsAt"], "2027-01-15T08:05:00.000Z");
    assert_eq!(resolve[0]["endsAt"], "2027-01-15T08:01:30.000Z");
}

#[tokio::test]
async fn alertmanager_labels_are_identity_only_and_survive_a_component_change() {
    let (base, mut requests) = accepting().await;
    let alertmanager = Alertmanager::new(client(), &format!("{base}/"), None);
    let before = alert(AlertEvent::Fire);
    let after = Alert {
        event: AlertEvent::Repeat,
        component_id: Some("api".into()),
        component_name: Some("API".into()),
        health: Health::Degraded,
        subject_name: "Public API v2".into(),
        ..alert(AlertEvent::Fire)
    };
    alertmanager.send(&before).await.unwrap();
    alertmanager.send(&after).await.unwrap();

    let (first, second) = two(&mut requests).await;
    let (first, second) = (first.json(), second.json());
    // Alertmanager keys an alert by its label set: a changed label would
    // open a second alert and leave the first to time out.
    assert_eq!(
        serde_json::to_string(&first[0]["labels"]).unwrap(),
        serde_json::to_string(&second[0]["labels"]).unwrap()
    );
    let mut keys: Vec<&String> = first[0]["labels"].as_object().unwrap().keys().collect();
    keys.sort_unstable();
    assert_eq!(keys, ["alertname", "node", "subject_id", "subject_kind"]);
    assert_eq!(first[0]["annotations"]["component"], "web");
    assert_eq!(second[0]["annotations"]["component"], "api");
    assert_eq!(second[0]["annotations"]["health"], "degraded");
}

#[tokio::test]
async fn a_failure_is_classified_and_never_repeats_the_secret_url() {
    let (base, mut requests) = mock(
        StatusCode::NOT_FOUND,
        "no hook at /services/T0/B0/hook-secret",
    )
    .await;
    let slack = Slack::new(client(), format!("{base}/services/T0/B0/hook-secret"));
    let error = slack.send(&alert(AlertEvent::Fire)).await.unwrap_err();
    requests.recv().await.unwrap();
    assert!(error.permanent);
    assert!(!error.message.contains("hook-secret"), "{}", error.message);
    assert!(error.message.starts_with("HTTP 404"), "{}", error.message);

    let (base, _requests) = mock(StatusCode::SERVICE_UNAVAILABLE, "busy").await;
    let slack = Slack::new(client(), format!("{base}/services/T0/B0/hook-secret"));
    assert!(
        !slack
            .send(&alert(AlertEvent::Fire))
            .await
            .unwrap_err()
            .permanent
    );

    // Nothing listens: a transport error, which names no URL either.
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = closed.local_addr().unwrap();
    drop(closed);
    let slack = Slack::new(
        client(),
        format!("http://{address}/services/T0/B0/hook-secret"),
    );
    let error = slack.send(&alert(AlertEvent::Fire)).await.unwrap_err();
    assert!(!error.permanent);
    assert!(!error.message.contains("hook-secret"), "{}", error.message);
}

/// A minimal SMTP server: every message it accepts, raw.
async fn smtp_mock() -> (u16, mpsc::UnboundedReceiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, receiver) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let sender = sender.clone();
            tokio::spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut lines = BufReader::new(read);
                write.write_all(b"220 mock ESMTP\r\n").await.unwrap();
                let mut line = String::new();
                loop {
                    line.clear();
                    if lines.read_line(&mut line).await.unwrap() == 0 {
                        return;
                    }
                    let command = line.trim_end().to_ascii_uppercase();
                    if command == "DATA" {
                        write.write_all(b"354 go ahead\r\n").await.unwrap();
                        let mut message = String::new();
                        while !message.ends_with("\r\n.\r\n") {
                            if lines.read_line(&mut message).await.unwrap() == 0 {
                                return;
                            }
                        }
                        message.truncate(message.len() - 3);
                        let _ = sender.send(message);
                        write.write_all(b"250 queued\r\n").await.unwrap();
                    } else if command == "QUIT" {
                        write.write_all(b"221 bye\r\n").await.unwrap();
                        return;
                    } else {
                        write.write_all(b"250 ok\r\n").await.unwrap();
                    }
                }
            });
        }
    });
    (port, receiver)
}

/// A header's value in a raw message.
fn header<'a>(message: &'a str, name: &str) -> &'a str {
    let prefix = format!("{name}: ");
    message
        .split("\r\n")
        .take_while(|line| !line.is_empty())
        .find_map(|line| line.strip_prefix(prefix.as_str()))
        .unwrap_or_else(|| panic!("no {name} header in {message}"))
}

#[tokio::test]
async fn email_threads_the_fire_and_its_resolve_and_can_be_answered() {
    let (port, mut messages) = smtp_mock().await;
    let email = Email::new(
        &format!("smtp://127.0.0.1:{port}?tls=none"),
        "Tocsin <alerts@example.com>",
        "ops@example.com",
        &["oncall@example.com".to_owned()],
    )
    .unwrap();
    fire_then_resolve(&email).await;

    let fire = messages.recv().await.unwrap();
    let resolve = messages.recv().await.unwrap();
    for message in [&fire, &resolve] {
        assert_eq!(header(message, "From"), "\"Tocsin\" <alerts@example.com>");
        assert_eq!(header(message, "Reply-To"), "ops@example.com");
        assert_eq!(header(message, "To"), "oncall@example.com");
    }
    assert_eq!(
        header(&fire, "X-Tocsin-Identity"),
        header(&resolve, "X-Tocsin-Identity")
    );
    assert_eq!(
        header(&fire, "In-Reply-To"),
        header(&resolve, "In-Reply-To")
    );
    assert_eq!(header(&fire, "References"), header(&resolve, "References"));
    assert_ne!(header(&fire, "Message-ID"), header(&resolve, "Message-ID"));
    assert_eq!(
        (
            header(&fire, "X-Tocsin-Event"),
            header(&resolve, "X-Tocsin-Event")
        ),
        ("fire", "resolve")
    );
    assert!(header(&fire, "Subject").starts_with("[DOWN]"));
    assert!(header(&resolve, "Subject").starts_with("[RESOLVED]"));
}
