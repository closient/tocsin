// SPDX-License-Identifier: AGPL-3.0-only

//! The federation routes in process: a signed push applied and answered
//! with its cursor, every refusal before the signature checks out the same
//! 401, a retired node's 403, a conflicting store's 409, a rebuilt node's
//! new store replacing its old one, a row that is not
//! the pusher's refused whole, and the node list with what the last push
//! said.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::Value;
use tocsin_core::Health;
use tocsin_core::config::{Environment, Source};
use tocsin_core::federation::NodeKey;
use tocsin_core::federation::wire::{
    self, Batch, BatchOutageChange, BatchResult, BatchRollup, Positions,
};
use tocsin_core::incident::Timestamp;
use tocsin_core::state::Confirmed;
use tocsin_core::status::{CheckKind, OutageEnding};
use tocsin_engine::{CheckSnapshot, HeartbeatHandle, MemberKind, Snapshot};
use tocsin_store::{RemoteStore, Store};
use tower::ServiceExt;

use super::events::EventBus;
use super::federation::backlog;
use super::{AppState, now, router};
use crate::web::identity::AdminAuth;

const READER: &str = "reader-secret-0123456789";

/// The spoke's store, as its database names itself.
const STORE: &str = "0123456789abcdef0123456789abcdef";

const BATCH: &str = wire::BATCH_PATH;
const NODES: &str = "/api/v1/federation/nodes";

/// `[server.auth]` admitting Alice behind a proxy in 10.0.0.0/8, so a
/// test can read the internal board.
const BOARD_AUTH: &str = "\n[server.auth]\nmode = \"trusted_header\"\n\
    trusted_proxies = [\"10.0.0.0/8\"]\nallowed_emails = [\"alice@example.com\"]\n";

fn spoke_key() -> NodeKey {
    NodeKey::from_seed([2; 32])
}

fn old_key() -> NodeKey {
    NodeKey::from_seed([3; 32])
}

/// `hub` takes pushes; `spoke` pushes to it; `old` was retired.
fn config_text() -> String {
    format!(
        r#"
[server]
public_url = "https://status.example.com/"
node_id = "hub"

[[node]]
id = "hub"
url = "https://hub.example.com"

[[node]]
id = "spoke"
public_keys = ["{spoke}"]
push_to = ["hub"]

[[node]]
id = "old"
public_keys = ["{old}"]
retired = true

[federation]
default_nodes = ["hub", "spoke"]

[[component]]
id = "api"
name = "API"

[[check]]
id = "api-port"
component = "api"
type = "tcp"
target = "api.example.com:443"

[[token]]
id = "reader"
secret = "${{ENV:READER_TOKEN}}"
scopes = ["status:read"]
"#,
        spoke = spoke_key().public_key(),
        old = old_key().public_key(),
    )
}

struct Api {
    dir: tempfile::TempDir,
    state: AppState,
    router: Router,
}

fn load(text: String) -> tocsin_core::config::Config {
    let variable = |name: &str| match name {
        "READER_TOKEN" => Some(READER.to_owned()),
        "PAGER_SIGNING_SECRET" => Some("pager-signing-secret-0123456789".to_owned()),
        _ => None,
    };
    tocsin_core::config::load(
        &[Source {
            name: "tocsin.toml".into(),
            text: Ok(text),
        }],
        &Environment {
            hostname: "hub".into(),
            node_id: None,
            variable: &variable,
        },
    )
    .unwrap()
}

/// The hub of [`config_text`], second in `alert_order` behind `first`, and
/// paging every alert to `pager`.
fn alerting_config_text() -> String {
    config_text().replace(
        "[federation]\ndefault_nodes = [\"hub\", \"spoke\"]\n",
        "[[node]]\nid = \"first\"\nurl = \"https://first.example.com\"\n\n\
             [federation]\ndefault_nodes = [\"hub\", \"spoke\"]\n\
             alert_order = [\"first\", \"hub\"]\n\n\
             [[notifier]]\nid = \"pager\"\ntype = \"webhook\"\n\n\
             [notifier.webhook]\nurl = \"https://pager.example.com/hook\"\n\
             signing_secret = \"${ENV:PAGER_SIGNING_SECRET}\"\n\n\
             [[route]]\nid = \"all\"\nnotifiers = [\"pager\"]\n",
    )
}

/// What the hub sees of `first`: its `node:first` heartbeat confirmed at
/// `health`, or never heard when `None`.
fn first_node(health: Option<Health>) -> Snapshot {
    heard("first", health)
}

/// What the hub sees of `node`: its `node:<id>` heartbeat confirmed at
/// `health`, or never heard when `None`.
fn heard(node: &str, health: Option<Health>) -> Snapshot {
    Snapshot {
        checks: health
            .map(|health| CheckSnapshot {
                id: format!("node:{node}"),
                kind: MemberKind::Heartbeat,
                component_id: None,
                state: Some(Confirmed {
                    health,
                    since_ms: 0,
                }),
                last: None,
            })
            .into_iter()
            .collect(),
        components: Vec::new(),
    }
}

fn api() -> Api {
    api_with(config_text(), Snapshot::default())
}

fn api_with(text: String, live: Snapshot) -> Api {
    let config = load(text);
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join(tocsin_store::DB_FILE)).unwrap();
    let metrics = crate::metrics::Metrics::new();
    metrics.configured(&config);
    let admin = AdminAuth::from_config(&config).unwrap().map(Arc::new);
    let state = AppState {
        store: Arc::new(store),
        config: Arc::new(config),
        events: EventBus::new(),
        admin,
        live: Arc::new(live),
        heartbeats: HeartbeatHandle::default(),
        liveness: crate::liveness::Pulse::default(),
        alert_wake: Arc::default(),
        config_status: crate::reload::steady_status(),
        metrics: Arc::new(metrics),
        subscribers: None,
        slack: None,
    };
    Api {
        dir,
        router: router(state.clone()),
        state,
    }
}

/// An empty batch from the spoke, sent now.
fn batch() -> Batch {
    Batch {
        node: "spoke".into(),
        store: STORE.into(),
        sent_at: now(),
        config_digest: Some("digest-1".into()),
        cursor: Positions::default(),
        head: Positions::default(),
        results: Vec::new(),
        outages: Vec::new(),
        rollups: Vec::new(),
    }
}

fn ago(seconds: i64) -> Timestamp {
    Timestamp::from_millis(now().as_millis() - seconds * 1_000)
}

fn result(origin_id: i64, node: &str) -> BatchResult {
    BatchResult {
        origin_id,
        node_id: node.into(),
        kind: CheckKind::Check,
        check_id: "api-port".into(),
        started_at: ago(120 - origin_id),
        duration_ms: 12,
        health: Health::Up,
        detail: None,
        error: None,
        status_code: None,
    }
}

fn open(seq: i64, origin_id: i64, started_at: Timestamp) -> BatchOutageChange {
    BatchOutageChange::Open {
        seq,
        origin_id,
        node_id: "spoke".into(),
        kind: CheckKind::Check,
        check_id: "api-port".into(),
        component_id: Some("api".into()),
        health: Health::Down,
        started_at,
        alerted: true,
    }
}

fn end(seq: i64, origin_id: i64, ended_at: Timestamp) -> BatchOutageChange {
    BatchOutageChange::End {
        seq,
        origin_id,
        ended_at,
        ending: OutageEnding::Recovered,
    }
}

/// How a push is sent: by whom, from which store, signed with which key,
/// and over which store when not the one named.
struct Push<'a> {
    node: Option<&'a str>,
    store: Option<&'a str>,
    key: Option<NodeKey>,
    signed_store: Option<&'a str>,
}

impl Default for Push<'_> {
    fn default() -> Self {
        Push {
            node: Some("spoke"),
            store: Some(STORE),
            key: Some(spoke_key()),
            signed_store: None,
        }
    }
}

impl Api {
    async fn send(&self, request: Request<Body>) -> (StatusCode, Value) {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, value)
    }

    async fn push(&self, batch: &Batch) -> (StatusCode, Value) {
        self.push_as(Push::default(), batch).await
    }

    async fn push_as(&self, push: Push<'_>, batch: &Batch) -> (StatusCode, Value) {
        let body = serde_json::to_vec(batch).unwrap();
        let mut request = Request::builder()
            .method(Method::POST)
            .uri(BATCH)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(node) = push.node {
            request = request.header(wire::NODE_HEADER, node);
        }
        if let Some(store) = push.store {
            request = request.header(wire::STORE_HEADER, store);
        }
        if let Some(key) = push.key {
            let signature = key.sign(
                push.node.unwrap_or_default(),
                push.signed_store.or(push.store).unwrap_or_default(),
                &body,
            );
            request = request.header(wire::SIGNATURE_HEADER, signature);
        }
        self.send(request.body(Body::from(body)).unwrap()).await
    }

    /// Sets the state of every store pushed here directly, without the
    /// pushes that would lead to it.
    fn mark_stores(&self, state: &str) -> rusqlite::Connection {
        let database =
            rusqlite::Connection::open(self.dir.path().join(tocsin_store::DB_FILE)).unwrap();
        database
            .execute("UPDATE node_store SET state = ?1 WHERE local = 0", [state])
            .unwrap();
        database
    }

    /// The internal board's live fragment, as an operator behind the
    /// trusted proxy of [`BOARD_AUTH`].
    async fn board(&self) -> String {
        let mut request = Request::builder()
            .method(Method::GET)
            .uri("/internal/fragments/board")
            .header(header::HOST, "status.example.com")
            .header("x-forwarded-email", "alice@example.com");
        request
            .extensions_mut()
            .unwrap()
            .insert(ConnectInfo("10.1.2.3:41000".parse::<SocketAddr>().unwrap()));
        let response = self
            .router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    async fn nodes(&self, token: Option<&str>) -> (StatusCode, Value) {
        let mut request = Request::builder().method(Method::GET).uri(NODES);
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        self.send(request.body(Body::empty()).unwrap()).await
    }
}

#[tokio::test]
async fn a_signed_batch_is_applied_and_answered_with_its_cursor() {
    let api = api();
    let started = ago(60);
    let mut sent = batch();
    sent.results = vec![result(1, "spoke"), result(2, "spoke")];
    sent.outages = vec![open(1, 7, started)];
    sent.rollups = vec![BatchRollup {
        check_id: "api-port".into(),
        node_id: "spoke".into(),
        date: "2026-01-05".into(),
        kind: CheckKind::Check,
        total: 2,
        up: 2,
        degraded: 0,
        down: 0,
        worst: Health::Up,
    }];
    sent.head = Positions {
        results: Some(5),
        outages: Some(1),
    };
    let (status, receipt) = api.push(&sent).await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["results_applied"], 2);
    assert_eq!(receipt["outage_changes_applied"], 1);
    assert_eq!(receipt["rollups_applied"], 1);
    assert_eq!(receipt["cursor"]["results"], 2);
    assert_eq!(receipt["cursor"]["outages"], 1);

    let stores = api.state.store.remote_stores().await.unwrap();
    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0].node_id, "spoke");
    assert_eq!(stores[0].store_id, STORE);
    assert!(stores[0].last_push_at_ms.is_some());

    // The same rows again change nothing, and the cursor stays where it is.
    let mut again = sent.clone();
    again.cursor = Positions {
        results: Some(2),
        outages: Some(1),
    };
    let (status, receipt) = api.push(&again).await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["results_applied"], 0);
    assert_eq!(receipt["outage_changes_applied"], 0);
    assert_eq!(receipt["cursor"]["results"], 2);
}

#[tokio::test]
async fn a_row_that_is_not_the_pushers_refuses_the_whole_batch() {
    let api = api();
    let mut sent = batch();
    sent.results = vec![result(1, "spoke"), result(2, "hub")];
    let (status, body) = api.push(&sent).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["fields"][0]["path"], "results[1].node_id", "{body}");
    assert_eq!(
        api.state.store.remote_stores().await.unwrap(),
        Vec::<RemoteStore>::new()
    );
    assert!(api.state.metrics.federation.last_push("spoke").is_none());
}

#[tokio::test]
async fn every_push_that_does_not_prove_its_node_gets_the_same_401() {
    let api = api();
    let sent = batch();
    let refusals = [
        Push {
            node: None,
            ..Push::default()
        },
        Push {
            store: None,
            ..Push::default()
        },
        Push {
            key: None,
            ..Push::default()
        },
        // A node the configuration does not list.
        Push {
            node: Some("stranger"),
            ..Push::default()
        },
        // Signed with another node's key.
        Push {
            key: Some(old_key()),
            ..Push::default()
        },
        // Signed over another store than the one named.
        Push {
            store: Some("fedcba9876543210fedcba9876543210"),
            signed_store: Some(STORE),
            ..Push::default()
        },
    ];
    let mut bodies = Vec::new();
    for push in refusals {
        let (status, body) = api.push_as(push, &sent).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        bodies.push(body);
    }
    assert!(
        bodies.windows(2).all(|pair| pair[0] == pair[1]),
        "{bodies:?}"
    );
    assert_eq!(
        api.state.store.remote_stores().await.unwrap(),
        Vec::<RemoteStore>::new()
    );
}

#[tokio::test]
async fn a_signature_over_a_different_body_is_refused() {
    let api = api();
    let sent = batch();
    let signed = serde_json::to_vec(&sent).unwrap();
    let mut tampered = sent.clone();
    tampered.results = vec![result(1, "spoke")];
    let request = Request::builder()
        .method(Method::POST)
        .uri(BATCH)
        .header(header::CONTENT_TYPE, "application/json")
        .header(wire::NODE_HEADER, "spoke")
        .header(wire::STORE_HEADER, STORE)
        .header(
            wire::SIGNATURE_HEADER,
            spoke_key().sign("spoke", STORE, &signed),
        )
        .body(Body::from(serde_json::to_vec(&tampered).unwrap()))
        .unwrap();
    let (status, body) = api.send(request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
}

#[tokio::test]
async fn a_retired_node_that_proves_itself_is_told_so() {
    let api = api();
    let mut sent = batch();
    sent.node = "old".into();
    let (status, body) = api
        .push_as(
            Push {
                node: Some("old"),
                key: Some(old_key()),
                ..Push::default()
            },
            &sent,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(
        body["message"].as_str().unwrap().contains("retired"),
        "{body}"
    );
    assert_eq!(
        api.state.store.remote_stores().await.unwrap(),
        Vec::<RemoteStore>::new()
    );
}

#[tokio::test]
async fn a_body_naming_another_node_or_store_than_its_headers_is_invalid() {
    let api = api();
    let mut sent = batch();
    sent.node = "hub".into();
    sent.store = "fedcba9876543210fedcba9876543210".into();
    let (status, body) = api.push(&sent).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let paths: Vec<&str> = body["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|field| field["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["node", "store"], "{body}");
}

#[tokio::test]
async fn a_store_resending_an_outage_differently_conflicts() {
    let api = api();
    let started = ago(60);
    let mut first = batch();
    first.outages = vec![open(1, 3, started), end(2, 3, ago(30))];
    let (status, body) = api.push(&first).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let mut differing = batch();
    differing.outages = vec![end(2, 3, ago(20))];
    let (status, body) = api.push(&differing).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "conflict", "{body}");
}

#[tokio::test]
async fn a_conflicted_node_is_refused_until_one_machine_is_retired() {
    let api = api();
    let (status, body) = api.push(&batch()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let database = api.mark_stores("conflicted");

    let mut sent = batch();
    sent.results = vec![result(1, "spoke")];
    let (status, body) = api.push(&sent).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "conflict", "{body}");
    assert!(body["message"].as_str().unwrap().contains(STORE), "{body}");
    assert_eq!(stored_results(&database), 0);
}

#[tokio::test]
async fn a_retired_store_is_refused_though_its_node_is_not() {
    let api = api();
    let (status, body) = api.push(&batch()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let database = api.mark_stores("retired");

    let mut sent = batch();
    sent.results = vec![result(1, "spoke")];
    let (status, body) = api.push(&sent).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(
        body["message"].as_str().unwrap().contains("retired"),
        "{body}"
    );
    assert_eq!(stored_results(&database), 0);
}

fn stored_results(database: &rusqlite::Connection) -> i64 {
    database
        .query_row("SELECT count(*) FROM check_result", [], |row| row.get(0))
        .unwrap()
}

#[tokio::test]
async fn a_push_without_its_headers_is_refused_before_its_body_is_read() {
    let api = api();
    let request = Request::builder()
        .method(Method::POST)
        .uri(BATCH)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(vec![b' '; super::BODY_LIMIT + 1]))
        .unwrap();
    let (status, body) = api.send(request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
}

#[tokio::test]
async fn a_batch_sent_before_the_last_does_not_replace_it() {
    let api = api();
    let (status, body) = api.push(&batch()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let last = api.state.metrics.federation.last_push("spoke").unwrap();

    let mut replayed = batch();
    replayed.sent_at = ago(3_600);
    replayed.config_digest = Some("digest-0".into());
    let (status, body) = api.push(&replayed).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let now_last = api.state.metrics.federation.last_push("spoke").unwrap();
    assert_eq!(now_last.sent_at, last.sent_at);
    assert_eq!(now_last.config_digest.as_deref(), Some("digest-1"));
}

#[tokio::test]
async fn retiring_a_node_drops_its_last_push() {
    let api = api();
    let (status, body) = api.push(&batch()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(api.state.metrics.federation.last_push("spoke").is_some());

    let retired = load(
        config_text()
            .replace("push_to = [\"hub\"]", "push_to = [\"hub\"]\nretired = true")
            .replace(
                "default_nodes = [\"hub\", \"spoke\"]",
                "default_nodes = [\"hub\"]",
            ),
    );
    api.state.metrics.configured(&retired);
    assert!(api.state.metrics.federation.last_push("spoke").is_none());
}

#[tokio::test]
async fn a_push_records_the_nodes_clock_offset_and_backlog() {
    let api = api();
    let mut sent = batch();
    sent.sent_at = Timestamp::from_millis(now().as_millis() + 30_000);
    sent.head = Positions {
        results: Some(40),
        outages: Some(2),
    };
    let (status, body) = api.push(&sent).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let text = api
        .state
        .metrics
        .render(&Snapshot::default(), &api.state.config, 0, None);
    let value = |name: &str| -> f64 {
        let prefix = format!("{name}{{node=\"spoke\"}} ");
        text.lines()
            .find_map(|line| line.strip_prefix(&prefix))
            .unwrap_or_else(|| panic!("no {prefix} in\n{text}"))
            .parse()
            .unwrap()
    };
    let offset = value("tocsin_node_clock_offset_seconds");
    assert!((offset - 30.0).abs() < 1.0, "{offset}");
    assert!((value("tocsin_node_backlog_rows") - 42.0).abs() < f64::EPSILON);
    // The hub pushes nowhere, so it has no series.
    assert!(!text.contains("node=\"hub\""), "{text}");
}

#[tokio::test]
async fn the_node_list_needs_status_read() {
    let api = api();
    assert_eq!(api.nodes(None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(
        api.nodes(Some("nonsense")).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn the_node_list_shows_each_node_its_stores_and_last_push() {
    let api = api();
    let mut sent = batch();
    sent.results = vec![result(1, "spoke")];
    sent.head = Positions {
        results: Some(3),
        outages: None,
    };
    let (status, body) = api.push(&sent).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, list) = api.nodes(Some(READER)).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let nodes = list["nodes"].as_array().unwrap();
    let ids: Vec<&str> = nodes
        .iter()
        .map(|node| node["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["hub", "spoke", "old"]);

    let hub = &nodes[0];
    assert_eq!(hub["local"], true);
    assert_eq!(hub["stores"], Value::Array(Vec::new()));
    assert!(hub.get("last_push").is_none(), "{hub}");

    let spoke = &nodes[1];
    assert_eq!(spoke["local"], false);
    assert_eq!(spoke["retired"], false);
    assert_eq!(spoke["stores"][0]["store"], STORE);
    assert_eq!(spoke["stores"][0]["state"], "active");
    assert!(spoke["stores"][0]["last_push_at"].is_string(), "{spoke}");
    let push = &spoke["last_push"];
    assert_eq!(push["store"], STORE);
    assert_eq!(push["backlog_rows"], 2);
    assert_eq!(push["config_digest"], "digest-1");
    assert!(push["clock_offset_seconds"].as_f64().unwrap().abs() < 5.0);

    assert_eq!(nodes[2]["retired"], true);
}

#[test]
fn the_backlog_counts_what_each_head_is_past_the_cursor() {
    let at = |results, outages| Positions { results, outages };
    assert_eq!(backlog(at(None, None), at(None, None)), 0);
    assert_eq!(backlog(at(Some(10), Some(4)), at(None, None)), 14);
    assert_eq!(backlog(at(Some(10), Some(4)), at(Some(7), Some(4))), 3);
    // A head behind the cursor, as after the sender's database was
    // replaced, is no backlog rather than a negative one.
    assert_eq!(backlog(at(Some(2), None), at(Some(9), Some(3))), 0);
}

#[tokio::test]
async fn a_pushed_outage_pages_only_while_every_node_ahead_is_down() {
    let mut sent = batch();
    sent.outages = vec![open(1, 7, ago(5))];

    let behind_up = api_with(alerting_config_text(), first_node(Some(Health::Up)));
    let (status, receipt) = behind_up.push(&sent).await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["outage_changes_applied"], 1);
    assert_eq!(behind_up.state.store.alerts().await.unwrap(), []);

    for ahead in [Some(Health::Down), None] {
        let api = api_with(alerting_config_text(), first_node(ahead));
        let (status, receipt) = api.push(&sent).await;
        assert_eq!(status, StatusCode::OK, "{receipt}");
        let fired = api.state.store.alerts().await.unwrap();
        assert_eq!(fired.len(), 1, "first {ahead:?}");
        assert_eq!(fired[0].node_id, "spoke");
        assert_eq!(fired[0].subject_id, "api-port");
        assert_eq!(fired[0].notifier_id, "pager");
        assert_eq!(fired[0].ended_at_ms, None);
        assert_eq!(fired[0].arrived_late_at_ms, None);

        let mut ended = batch();
        ended.cursor.outages = Some(1);
        let ended_at = now();
        ended.outages = vec![end(2, 7, ended_at)];
        let (status, receipt) = api.push(&ended).await;
        assert_eq!(status, StatusCode::OK, "{receipt}");
        let resolved = api.state.store.alerts().await.unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].id, fired[0].id);
        assert_eq!(resolved[0].ended_at_ms, Some(ended_at.as_millis()));
    }
}

#[tokio::test]
async fn the_node_that_pushed_counts_as_up_before_its_heartbeat_is_heard() {
    // `spoke` is ahead of the hub and unheard: its push is the first word
    // from it, recorded only after the batch. It pages for itself.
    let text = alerting_config_text().replace(
        "alert_order = [\"first\", \"hub\"]",
        "alert_order = [\"spoke\", \"hub\"]",
    );
    let api = api_with(text, heard("spoke", None));
    let mut sent = batch();
    sent.outages = vec![open(1, 7, ago(5))];
    let (status, receipt) = api.push(&sent).await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["outage_changes_applied"], 1);
    assert_eq!(api.state.store.alerts().await.unwrap(), []);
}

#[tokio::test]
async fn a_pushed_outage_that_already_ended_pages_no_one() {
    let api = api_with(alerting_config_text(), first_node(Some(Health::Down)));
    let mut sent = batch();
    sent.outages = vec![open(1, 7, ago(60)), end(2, 7, ago(30))];
    let (status, receipt) = api.push(&sent).await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["outage_changes_applied"], 2);
    assert_eq!(api.state.store.alerts().await.unwrap(), []);
}

#[tokio::test]
async fn a_pushed_outage_of_a_heartbeat_tocsin_keeps_pages_no_one() {
    let api = api_with(alerting_config_text(), first_node(None));
    let mut sent = batch();
    sent.outages = vec![BatchOutageChange::Open {
        seq: 1,
        origin_id: 7,
        node_id: "spoke".into(),
        kind: CheckKind::Heartbeat,
        check_id: "upstream:hub".into(),
        component_id: None,
        health: Health::Down,
        started_at: ago(5),
        alerted: true,
    }];
    let (status, receipt) = api.push(&sent).await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["outage_changes_applied"], 1);
    assert_eq!(api.state.store.alerts().await.unwrap(), []);
}

#[tokio::test]
async fn a_pushed_outage_that_reaches_the_hub_late_says_so() {
    let api = api_with(alerting_config_text(), first_node(None));
    let mut sent = batch();
    sent.outages = vec![open(1, 7, ago(3_600))];
    let before = now().as_millis();
    let (status, receipt) = api.push(&sent).await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    let fired = api.state.store.alerts().await.unwrap();
    assert_eq!(fired.len(), 1);
    let arrived = fired[0].arrived_late_at_ms.expect("marked late");
    assert!(arrived >= before, "{arrived} < {before}");
}

#[tokio::test]
async fn a_remote_check_reads_unknown_while_its_node_is_not_heard_from() {
    let mut sent = batch();
    sent.results = vec![result(1, "spoke")];
    let mut opened = batch();
    opened.cursor.results = Some(1);
    opened.outages = vec![open(1, 7, ago(30))];

    let spoke_check = |list: &Value| list["nodes"][1]["checks"].clone();
    let text = config_text() + BOARD_AUTH;

    let api = api_with(text.clone(), heard("spoke", Some(Health::Up)));
    let (status, body) = api.push(&sent).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, list) = api.nodes(Some(READER)).await;
    assert_eq!(list["nodes"][0]["local"], true);
    assert_eq!(list["nodes"][0]["checks"], Value::Array(Vec::new()));
    assert_eq!(list["nodes"][1]["liveness"], "up");
    let checks = spoke_check(&list);
    assert_eq!(checks.as_array().unwrap().len(), 1, "{checks}");
    assert_eq!(checks[0]["id"], "api-port");
    assert_eq!(checks[0]["health"], "up");
    assert!(checks[0]["since"].is_string(), "{checks}");
    let board = api.board().await;
    assert!(board.contains("Nodes"), "{board}");
    assert!(board.contains("api-port: Up"), "{board}");

    let (status, body) = api.push(&opened).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, list) = api.nodes(Some(READER)).await;
    assert_eq!(spoke_check(&list)[0]["health"], "down");

    for liveness in [Some(Health::Down), None] {
        let api = api_with(text.clone(), heard("spoke", liveness));
        for push in [&sent, &opened] {
            let (status, body) = api.push(push).await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        let (_, list) = api.nodes(Some(READER)).await;
        let checks = spoke_check(&list);
        assert_eq!(checks[0]["id"], "api-port", "{liveness:?}");
        assert_eq!(checks[0]["health"], Value::Null, "{liveness:?}");
        assert_eq!(checks[0]["since"], Value::Null, "{liveness:?}");
        assert!(checks[0]["last_result_at"].is_string(), "{checks}");
        let board = api.board().await;
        assert!(board.contains("api-port: Unknown"), "{board}");
    }
}

/// A rebuilt node's new store is taken and replaces the old one; the old
/// one pushing again is refused as a conflict, and from then on so is the
/// new one.
#[tokio::test]
async fn a_rebuilt_node_replaces_its_store_and_the_old_one_pushing_again_conflicts() {
    const REBUILT: &str = "fedcba9876543210fedcba9876543210";
    let api = api();
    let mut first = batch();
    first.outages = vec![open(1, 3, ago(60))];
    let (status, body) = api.push(&first).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let rebuilt = Push {
        store: Some(REBUILT),
        ..Push::default()
    };
    let fresh = Batch {
        store: REBUILT.into(),
        ..batch()
    };
    let (status, body) = api.push_as(rebuilt, &fresh).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let database = rusqlite::Connection::open(api.dir.path().join(tocsin_store::DB_FILE)).unwrap();
    let ending: String = database
        .query_row("SELECT ending FROM outage WHERE origin_id = 3", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(ending, "withdrawn");

    let (status, body) = api.push(&batch()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["message"].as_str().unwrap().contains("replaced"),
        "{body}"
    );
    let rebuilt = Push {
        store: Some(REBUILT),
        ..Push::default()
    };
    let (status, body) = api.push_as(rebuilt, &fresh).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}
