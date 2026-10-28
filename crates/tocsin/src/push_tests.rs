// SPDX-License-Identifier: AGPL-3.0-only

//! The pusher against a real hub: the federation routes served on
//! loopback, behind a gate that can refuse a push before the hub sees it,
//! lose the hub's answer after it committed, or refuse a batch as too
//! large. Every row reaches the hub once and in order, the cursor follows
//! only what the hub committed, and a restart resumes from it.

use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use tocsin_alert::{Backoff, Jitter};
use tocsin_core::Health;
use tocsin_core::config::{Config, Environment, Source};
use tocsin_core::federation::NodeKey;
use tocsin_core::federation::wire::Batch;
use tocsin_engine::{HeartbeatHandle, Snapshot};
use tocsin_store::{NewCheckResult, NewOutage, PushCursor, Retention, Store, SubjectKind};
use tower::ServiceExt;

use super::{Pacing, PushSettings, Pushers, Upstreams};
use crate::http::events::EventBus;
use crate::http::{self, AppState};
use crate::liveness;
use crate::metrics::Metrics;
use crate::serve::Monitor;

const KEY_VARIABLE: &str = "TOCSIN_TEST_NODE_KEY";

fn spoke_key() -> NodeKey {
    NodeKey::from_seed([2; 32])
}

fn hub_key() -> NodeKey {
    NodeKey::from_seed([4; 32])
}

/// `hub` takes pushes at `hub_url`; `spoke` pushes to it.
fn config_text(node_id: &str, hub_url: &str) -> String {
    format!(
        r#"
[server]
node_id = "{node_id}"
node_key = "${{ENV:{KEY_VARIABLE}}}"

[[node]]
id = "hub"
url = "{hub_url}"
public_keys = ["{hub}"]

[[node]]
id = "spoke"
public_keys = ["{spoke}"]
push_to = ["hub"]

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
"#,
        hub = hub_key().public_key(),
        spoke = spoke_key().public_key(),
    )
}

fn load(node_id: &str, hub_url: &str, key: &NodeKey) -> Config {
    load_text(node_id, config_text(node_id, hub_url), Some(key))
}

fn load_text(node_id: &str, text: String, key: Option<&NodeKey>) -> Config {
    let secret = key.map(NodeKey::expose);
    let variable = |name: &str| secret.clone().filter(|_| name == KEY_VARIABLE);
    tocsin_core::config::load(
        &[Source {
            name: "tocsin.toml".into(),
            text: Ok(text),
        }],
        &Environment {
            hostname: node_id.into(),
            node_id: None,
            variable: &variable,
        },
    )
    .unwrap()
}

/// What the gate does with the next push.
#[derive(Debug, Clone, Copy)]
enum Turn {
    /// Answers this without the hub seeing the push.
    Refuse(StatusCode),
    /// Lets the hub commit the push, then answers this instead of its
    /// receipt: a crash between the hub's commit and the pusher's.
    LoseAnswer(StatusCode),
}

#[derive(Debug, Default)]
struct Gate {
    turns: VecDeque<Turn>,
    /// Answers 413 to a batch with more results than this.
    max_results: Option<usize>,
    /// Every push that reached the gate, with what it answered.
    seen: Vec<(Batch, StatusCode)>,
}

type SharedGate = Arc<Mutex<Gate>>;

fn lock(gate: &SharedGate) -> MutexGuard<'_, Gate> {
    gate.lock().unwrap_or_else(PoisonError::into_inner)
}

async fn gated(State((gate, hub)): State<(SharedGate, Router)>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = to_bytes(body, http::BODY_LIMIT).await.unwrap();
    let batch: Batch = serde_json::from_slice(&bytes).unwrap();
    let (turn, too_large) = {
        let mut gate = lock(&gate);
        let too_large = gate
            .max_results
            .is_some_and(|max| batch.results.len() > max);
        let turn = if too_large {
            None
        } else {
            gate.turns.pop_front()
        };
        (turn, too_large)
    };
    let request = Request::from_parts(parts, Body::from(bytes));
    let status = match (too_large, turn) {
        (true, _) => StatusCode::PAYLOAD_TOO_LARGE,
        (false, Some(Turn::Refuse(status))) => status,
        (false, Some(Turn::LoseAnswer(status))) => {
            let answer = hub.oneshot(request).await.unwrap();
            assert_eq!(answer.status(), StatusCode::OK, "the hub took the push");
            status
        }
        (false, None) => {
            let answer = hub.oneshot(request).await.unwrap();
            lock(&gate).seen.push((batch, answer.status()));
            return answer;
        }
    };
    lock(&gate).seen.push((batch, status));
    status.into_response()
}

struct Hub {
    _dir: tempfile::TempDir,
    path: PathBuf,
    store: Arc<Store>,
    url: String,
    gate: SharedGate,
}

async fn hub() -> Hub {
    let (dir, store) = open();
    let config = load("hub", "http://hub.invalid", &hub_key());
    serve_hub(dir, store, config, HeartbeatHandle::default()).await
}

fn open() -> (tempfile::TempDir, Arc<Store>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join(tocsin_store::DB_FILE)).unwrap();
    (dir, Arc::new(store))
}

/// The federation routes for `config` on `store`, served on loopback
/// behind the gate, pinging heartbeats through `heartbeats`.
async fn serve_hub(
    dir: tempfile::TempDir,
    store: Arc<Store>,
    config: Config,
    heartbeats: HeartbeatHandle,
) -> Hub {
    let path = dir.path().join(tocsin_store::DB_FILE);
    let metrics = Metrics::new();
    metrics.configured(&config);
    let state = AppState {
        store: Arc::clone(&store),
        config: Arc::new(config),
        events: EventBus::new(),
        admin: None,
        live: Arc::new(Snapshot::default()),
        heartbeats,
        liveness: crate::liveness::Pulse::default(),
        alert_wake: Arc::default(),
        config_status: crate::reload::steady_status(),
        metrics: Arc::new(metrics),
        subscribers: None,
        slack: None,
    };
    let gate = SharedGate::default();
    let app = Router::new()
        .fallback(gated)
        .with_state((Arc::clone(&gate), http::router(state)));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Hub {
        _dir: dir,
        path,
        store,
        url,
        gate,
    }
}

impl Hub {
    fn read(&self) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(&self.path).unwrap();
        conn.busy_timeout(Duration::from_secs(5)).unwrap();
        conn
    }

    /// The local ids of the spoke's results the hub holds, in order.
    fn results(&self) -> Vec<i64> {
        let conn = self.read();
        let mut statement = conn
            .prepare(
                "SELECT origin_id FROM check_result WHERE node_id = 'spoke' ORDER BY origin_id",
            )
            .unwrap();
        statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    /// The spoke's outages the hub holds, as (local id, ended).
    fn outages(&self) -> Vec<(i64, bool)> {
        let conn = self.read();
        let mut statement = conn
            .prepare(
                "SELECT origin_id, ended_at IS NOT NULL FROM outage
                 WHERE node_id = 'spoke' ORDER BY origin_id",
            )
            .unwrap();
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn turns(&self, turns: &[Turn]) {
        lock(&self.gate).turns.extend(turns);
    }

    fn max_results(&self, max: usize) {
        lock(&self.gate).max_results = Some(max);
    }

    /// Every push the gate saw, with what it answered.
    fn seen(&self) -> Vec<(Batch, StatusCode)> {
        lock(&self.gate).seen.clone()
    }
}

struct Spoke {
    _dir: tempfile::TempDir,
    store: Arc<Store>,
    config: Config,
    metrics: Metrics,
}

fn spoke(hub: &Hub) -> Spoke {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join(tocsin_store::DB_FILE)).unwrap();
    Spoke {
        _dir: dir,
        store: Arc::new(store),
        config: load("spoke", &hub.url, &spoke_key()),
        metrics: Metrics::new(),
    }
}

/// Fast enough for a test; long enough between idle pushes that each push
/// the gate sees was caused by a backlog, a failure or a restart.
fn pacing(batch_rows: u32) -> Pacing {
    Pacing {
        interval: Duration::from_secs(3600),
        backoff: Backoff {
            base: Duration::from_millis(5),
            cap: Duration::from_millis(20),
            jitter: Jitter::None,
        },
        batch_rows,
        timeout: Duration::from_secs(5),
    }
}

impl Spoke {
    fn upstreams(&self, batch_rows: u32) -> Upstreams {
        Upstreams::from_config(&self.config, &self.metrics)
            .unwrap()
            .paced(pacing(batch_rows))
    }

    fn start(&self, batch_rows: u32) -> Pushers {
        self.upstreams(batch_rows)
            .start(&self.store, &HeartbeatHandle::default())
    }

    /// `count` results, a second apart and ending a minute ago, with their
    /// local ids.
    async fn results(&self, count: i64) -> Vec<i64> {
        let mut ids = Vec::new();
        for n in 0..count {
            let started_at_ms = tocsin_store::now_ms() - 60_000 - (count - n) * 1_000;
            ids.push(self.store.append(result(started_at_ms)).await.unwrap());
        }
        ids
    }

    async fn cursor(&self) -> PushCursor {
        self.store
            .push_cursor("hub".into(), "spoke".into())
            .await
            .unwrap()
    }

    /// The push series in the metrics.
    fn series(&self) -> String {
        self.metrics
            .encode()
            .lines()
            .filter(|line| line.starts_with("tocsin_push_"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn result(started_at_ms: i64) -> NewCheckResult {
    NewCheckResult {
        node_id: "spoke".into(),
        check_id: "api-port".into(),
        started_at_ms,
        duration_ms: 12,
        health: Health::Up,
        detail: None,
        error: None,
        status_code: None,
    }
}

fn outage(started_at_ms: i64) -> NewOutage {
    NewOutage {
        node_id: "spoke".into(),
        subject_kind: SubjectKind::Check,
        check_id: "api-port".into(),
        component_id: Some("api".into()),
        health: Health::Down,
        started_at_ms,
        alerted: true,
    }
}

/// Waits until `done` holds, failing the test after ten seconds.
async fn eventually<F, Fut>(what: &str, done: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !done().await {
        assert!(tokio::time::Instant::now() < deadline, "never: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The ids of each batch's results, in the order the gate saw them.
fn batches(seen: &[(Batch, StatusCode)]) -> Vec<Vec<i64>> {
    seen.iter()
        .map(|(batch, _)| batch.results.iter().map(|row| row.origin_id).collect())
        .collect()
}

#[tokio::test]
async fn a_backlog_reaches_the_hub_in_batches_in_order_and_the_cursor_follows() {
    let hub = hub().await;
    let spoke = spoke(&hub);
    let ids = spoke.results(25).await;
    let opened_at = tocsin_store::now_ms() - 30_000;
    let open = spoke.store.open_outage(outage(opened_at)).await.unwrap();
    spoke
        .store
        .end_outage(open, opened_at + 1_000)
        .await
        .unwrap();
    let upstreams = spoke.upstreams(10);
    // A cursor made after the rollups starts at their first day.
    spoke.store.rollup_recent().await.unwrap();
    assert!(spoke.cursor().await.rollups_from.is_some());

    let pushers = upstreams.start(&spoke.store, &HeartbeatHandle::default());
    eventually("the hub has every row and every rollup", || async {
        hub.results() == ids && spoke.cursor().await.rollups_from.is_none()
    })
    .await;
    pushers.stop().await;

    let seen = hub.seen();
    assert!(seen.iter().all(|(_, status)| *status == StatusCode::OK));
    // At most ten at a time, each batch starting where the last one ended.
    assert_eq!(batches(&seen).concat(), ids);
    assert!(batches(&seen).iter().all(|batch| batch.len() <= 10));
    assert!(batches(&seen).len() >= 3);
    for pair in seen.windows(2) {
        if let Some(last) = pair[0].0.results.last() {
            assert_eq!(pair[1].0.cursor.results, Some(last.origin_id));
        }
    }
    assert!(seen.iter().any(|(batch, _)| !batch.rollups.is_empty()));
    assert_eq!(hub.outages(), [(open, true)]);
    let cursor = spoke.cursor().await;
    assert_eq!(cursor.results_through, *ids.last().unwrap());
    assert!(cursor.outages_through > 0);
    assert!(cursor.acked_at_ms.is_some());
}

#[tokio::test]
async fn rollups_rewritten_after_the_hub_had_them_go_again() {
    let hub = hub().await;
    let spoke = spoke(&hub);
    let ids = spoke.results(3).await;
    let rewritten = spoke.store.rollup_recent().await.unwrap().since;
    assert!(rewritten.is_some());
    let upstreams = spoke.upstreams(10);
    let pushers = upstreams.start(&spoke.store, &HeartbeatHandle::default());
    eventually("the hub has everything", || async {
        let cursor = spoke.cursor().await;
        cursor.results_through == *ids.last().unwrap() && cursor.rollups_from.is_none()
    })
    .await;
    pushers.stop().await;
    let before = hub.seen().len();

    // Maintenance rewrote a day the hub already has: the cursor goes back
    // to it, and the next push carries its rollups again.
    spoke.store.rollup_since(rewritten.unwrap()).await.unwrap();
    assert_eq!(spoke.cursor().await.rollups_from, rewritten);
    let pushers = upstreams.start(&spoke.store, &HeartbeatHandle::default());
    eventually("the rewritten day is pushed again", || async {
        spoke.cursor().await.rollups_from.is_none()
    })
    .await;
    pushers.stop().await;
    let seen = hub.seen();
    assert!(
        seen[before..]
            .iter()
            .any(|(batch, _)| !batch.rollups.is_empty())
    );
}

#[tokio::test]
async fn a_refused_push_is_retried_and_counted() {
    let hub = hub().await;
    let spoke = spoke(&hub);
    let ids = spoke.results(3).await;
    hub.turns(&[
        Turn::Refuse(StatusCode::SERVICE_UNAVAILABLE),
        Turn::Refuse(StatusCode::BAD_GATEWAY),
    ]);

    let pushers = spoke.start(10);
    eventually("the hub has every result", || async {
        hub.results() == ids
    })
    .await;
    pushers.stop().await;

    let seen = hub.seen();
    let statuses: Vec<_> = seen.iter().map(|(_, status)| status.as_u16()).collect();
    assert_eq!(statuses, [503, 502, 200]);
    // Nothing moved while the hub refused: the same rows went each time.
    assert_eq!(batches(&seen), [ids.clone(), ids.clone(), ids.clone()]);
    assert!(
        spoke
            .series()
            .contains(r#"tocsin_push_failures_total{upstream="hub"} 2"#),
        "{}",
        spoke.series()
    );
}

#[tokio::test]
async fn rows_the_hub_committed_before_its_answer_was_lost_go_again_and_land_once() {
    let hub = hub().await;
    let spoke = spoke(&hub);
    let ids = spoke.results(4).await;
    hub.turns(&[Turn::LoseAnswer(StatusCode::BAD_GATEWAY)]);

    let pushers = spoke.start(10);
    eventually("the cursor is at the head", || async {
        spoke.cursor().await.results_through == *ids.last().unwrap()
    })
    .await;
    pushers.stop().await;

    let seen = hub.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].1, StatusCode::BAD_GATEWAY);
    // The second push is the first again, from the same cursor.
    assert_eq!(seen[1].0.cursor, seen[0].0.cursor);
    assert_eq!(batches(&seen), [ids.clone(), ids.clone()]);
    assert_eq!(hub.results(), ids);
}

#[tokio::test]
async fn a_batch_too_large_is_halved_until_the_hub_takes_it() {
    let hub = hub().await;
    let spoke = spoke(&hub);
    let ids = spoke.results(10).await;
    hub.max_results(3);

    let pushers = spoke.start(16);
    eventually("the hub has every result", || async {
        hub.results() == ids
    })
    .await;
    pushers.stop().await;

    let sizes: Vec<_> = hub
        .seen()
        .iter()
        .map(|(batch, status)| (batch.results.len(), status.as_u16()))
        .collect();
    // 16 and 8 both hold all ten, 4 is still too many, then 2 at a time
    // while the backlog lasts.
    assert_eq!(
        sizes,
        [
            (10, 413),
            (8, 413),
            (4, 413),
            (2, 200),
            (2, 200),
            (2, 200),
            (2, 200),
            (2, 200)
        ]
    );
    assert!(
        spoke
            .series()
            .contains(r#"tocsin_push_failures_total{upstream="hub"} 3"#),
        "{}",
        spoke.series()
    );
}

#[tokio::test]
async fn a_restart_resumes_from_the_stored_cursor() {
    let hub = hub().await;
    let spoke = spoke(&hub);
    let first = spoke.results(3).await;
    let pushers = spoke.start(10);
    // Until the spoke stored the receipt, not only the hub its rows.
    eventually("the cursor is past the first results", || async {
        spoke.cursor().await.results_through == *first.last().unwrap()
    })
    .await;
    pushers.stop().await;

    let second = spoke.results(2).await;
    let pushers = spoke.start(10);
    eventually("every result is on the hub", || async {
        hub.results() == [first.clone(), second.clone()].concat()
    })
    .await;
    pushers.stop().await;

    let seen = hub.seen();
    let (resumed, _) = seen.last().unwrap();
    assert_eq!(resumed.cursor.results, first.last().copied());
    assert_eq!(batches(&seen), [first.clone(), second.clone()]);
}

#[tokio::test]
async fn results_retention_deletes_before_the_hub_has_them_are_counted() {
    let hub = hub().await;
    let spoke = spoke(&hub);
    let upstreams = spoke.upstreams(10);
    assert!(
        spoke
            .series()
            .contains(r#"tocsin_push_rows_expired_total{upstream="hub"} 0"#),
        "{}",
        spoke.series()
    );
    // The hub has acknowledged nothing, and these are past a 30-day
    // retention.
    spoke.cursor().await;
    let old = tocsin_store::now_ms() - 40 * 86_400_000;
    for n in 0..3 {
        spoke.store.append(result(old + n * 1_000)).await.unwrap();
    }

    let report = spoke
        .store
        .apply_retention(Retention::days(30), tocsin_store::now_ms())
        .await
        .unwrap();
    upstreams.expired(&report.unpushed);

    assert_eq!(report.deleted, 3);
    assert!(
        spoke
            .series()
            .contains(r#"tocsin_push_rows_expired_total{upstream="hub"} 3"#),
        "{}",
        spoke.series()
    );
}

#[test]
fn a_node_that_pushes_nowhere_starts_no_pushers() {
    // Without a node key too: only a node that pushes needs one.
    let text = config_text("hub", "http://hub.invalid")
        .replace(&format!("node_key = \"${{ENV:{KEY_VARIABLE}}}\"\n"), "");
    assert!(!text.contains("node_key"));
    let config = load_text("hub", text, None);
    let metrics = Metrics::new();
    let upstreams = Upstreams::from_config(&config, &metrics).unwrap();
    assert_eq!(upstreams.inner.upstreams.len(), 0);
    assert_eq!(
        metrics
            .encode()
            .lines()
            .filter(|line| line.starts_with("tocsin_push_") && !line.starts_with('#'))
            .count(),
        0
    );
}

#[test]
fn a_reload_tells_whether_the_push_settings_moved() {
    let running = load("spoke", "http://hub.invalid", &spoke_key());
    let same = load("spoke", "http://hub.invalid", &spoke_key());
    assert_eq!(PushSettings::of(&running), PushSettings::of(&same));

    let moved = load("spoke", "http://elsewhere.invalid", &spoke_key());
    assert_ne!(PushSettings::of(&running), PushSettings::of(&moved));

    let text = config_text("spoke", "http://hub.invalid")
        .replace("[federation]\n", "[federation]\npush_interval = \"1m\"\n");
    assert!(text.contains("push_interval"));
    let slower = load_text("spoke", text, Some(&spoke_key()));
    assert_ne!(PushSettings::of(&running), PushSettings::of(&slower));

    let text = config_text("spoke", "http://hub.invalid")
        .replace("push_to = [\"hub\"]\n", "")
        // Pushing nowhere, a check on both nodes has no node to derive it.
        .replace("default_nodes = [\"hub\", \"spoke\"]", "default_nodes = [\"spoke\"]");
    let nowhere = load_text("spoke", text, Some(&spoke_key()));
    assert_ne!(PushSettings::of(&running), PushSettings::of(&nowhere));
}

/// `text` with pushes 200ms apart, 300ms of grace and every outage routed
/// to a pager, and without its check, which would probe the network.
fn watched(text: &str) -> String {
    let (text, _check) = text.split_once("[[check]]").unwrap();
    let text = text.replace(
        "[federation]\n",
        "[federation]\npush_interval = \"200ms\"\ngrace = \"300ms\"\n",
    );
    format!(
        "{text}[[notifier]]\nid = \"pager\"\ntype = \"ntfy\"\n\n\
         [notifier.ntfy]\nurl = \"http://127.0.0.1:9/pager\"\n\n\
         [[route]]\nid = \"all\"\nnotifiers = [\"pager\"]\n"
    )
}

/// `config` with the liveness heartbeats `tocsin serve` adds to it.
fn watching(mut config: Config) -> Config {
    let push = PushSettings::of(&config);
    liveness::watch(&mut config, push.upstreams());
    config
}

/// Pushes as often as [`watched`] expects them.
fn watched_pacing() -> Pacing {
    Pacing {
        interval: Duration::from_millis(200),
        ..pacing(10)
    }
}

/// Whether heartbeat `id`'s latest result in `monitor` is up.
fn up(monitor: &Monitor, id: &str) -> bool {
    monitor
        .snapshot()
        .checks
        .iter()
        .find(|check| check.id == id)
        .and_then(|check| check.last)
        .is_some_and(|last| last.health == Health::Up)
}

/// Whether heartbeat `id` has an outage open on `node` that alerted, and
/// an alert open for it.
async fn alerting(store: &Store, node: &str, id: &str) -> bool {
    let open = store.open_outages(node).await.unwrap();
    let alerts = store.alerts().await.unwrap();
    open.iter()
        .any(|outage| outage.check_id == id && outage.alerted)
        && alerts
            .iter()
            .any(|alert| alert.subject_id == id && alert.ended_at_ms.is_none())
}

/// Whether heartbeat `id` alerted on `node` and is resolved: no outage
/// and no alert for it still open.
async fn resolved(store: &Store, node: &str, id: &str) -> bool {
    let open = store.open_outages(node).await.unwrap();
    let alerts: Vec<_> = store
        .alerts()
        .await
        .unwrap()
        .into_iter()
        .filter(|alert| alert.subject_id == id)
        .collect();
    !open.iter().any(|outage| outage.check_id == id)
        && !alerts.is_empty()
        && alerts.iter().all(|alert| alert.ended_at_ms.is_some())
}

#[tokio::test]
async fn a_node_that_stops_pushing_goes_down_on_the_hub_until_it_pushes_again() {
    let (dir, store) = open();
    let config = watching(load_text(
        "hub",
        watched(&config_text("hub", "http://hub.invalid")),
        Some(&hub_key()),
    ));
    let monitor = Monitor::start(&config, &store, &EventBus::new(), &Metrics::new())
        .await
        .unwrap();
    let hub = serve_hub(dir, Arc::clone(&store), config, monitor.heartbeats()).await;
    let spoke = spoke(&hub);
    let start = || {
        Upstreams::from_config(&spoke.config, &spoke.metrics)
            .unwrap()
            .paced(watched_pacing())
            .start(&spoke.store, &HeartbeatHandle::default())
    };

    let pushers = start();
    eventually("the hub hears the spoke", || async {
        up(&monitor, "node:spoke")
    })
    .await;
    pushers.stop().await;
    eventually("the silent spoke is down and alerting", || {
        alerting(&hub.store, "hub", "node:spoke")
    })
    .await;

    let pushers = start();
    eventually("its next push resolves it", || {
        resolved(&hub.store, "hub", "node:spoke")
    })
    .await;
    pushers.stop().await;
    monitor.stop().await;
}

#[tokio::test]
async fn an_upstream_that_refuses_pushes_goes_down_on_the_spoke_until_it_takes_them() {
    let hub = hub().await;
    let spoke = spoke(&hub);
    let config = watching(load_text(
        "spoke",
        watched(&config_text("spoke", &hub.url)),
        Some(&spoke_key()),
    ));
    let monitor = Monitor::start(&config, &spoke.store, &EventBus::new(), &Metrics::new())
        .await
        .unwrap();
    let pushers = Upstreams::from_config(&config, &spoke.metrics)
        .unwrap()
        .paced(watched_pacing())
        .start(&spoke.store, &monitor.heartbeats());

    eventually("the spoke reaches the hub", || async {
        up(&monitor, "upstream:hub")
    })
    .await;
    hub.turns(&vec![Turn::Refuse(StatusCode::SERVICE_UNAVAILABLE); 10_000]);
    eventually("the refusing hub is down and alerting", || {
        alerting(&spoke.store, "spoke", "upstream:hub")
    })
    .await;

    lock(&hub.gate).turns.clear();
    eventually("the next push it takes resolves it", || {
        resolved(&spoke.store, "spoke", "upstream:hub")
    })
    .await;
    // The hub takes the spoke's liveness rows like any other: an ingest
    // that refused a colon in an id would refuse every push after one.
    eventually("the hub holds the spoke's ended outage of it", || async {
        hub.read()
            .query_row(
                "SELECT count(*) FROM outage WHERE node_id = 'spoke'
                 AND check_id = 'upstream:hub' AND ended_at IS NOT NULL",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
            > 0
    })
    .await;
    pushers.stop().await;
    monitor.stop().await;
}

/// Two machines pushing as `spoke`: the second one's new store replaces
/// the first's, and when the first pushes again the hub refuses both and
/// its liveness heartbeat for the node goes down saying why, so it alerts
/// with the reason rather than as a silent node.
#[tokio::test]
async fn two_machines_pushing_as_one_node_alert_on_the_hub_with_the_reason() {
    let (dir, store) = open();
    let config = watching(load_text(
        "hub",
        watched(&config_text("hub", "http://hub.invalid")),
        Some(&hub_key()),
    ));
    let monitor = Monitor::start(&config, &store, &EventBus::new(), &Metrics::new())
        .await
        .unwrap();
    let hub = serve_hub(dir, Arc::clone(&store), config, monitor.heartbeats()).await;
    let (old, rebuilt) = (spoke(&hub), spoke(&hub));
    let start = |spoke: &Spoke| {
        Upstreams::from_config(&spoke.config, &spoke.metrics)
            .unwrap()
            .paced(watched_pacing())
            .start(&spoke.store, &HeartbeatHandle::default())
    };
    let stores = || {
        hub.read()
            .query_row(
                "SELECT count(*), sum(state = 'conflicted') FROM node_store WHERE local = 0",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?)),
            )
            .unwrap()
    };

    let pushers = start(&old);
    eventually("the hub hears the old machine", || async {
        up(&monitor, "node:spoke")
    })
    .await;
    pushers.stop().await;
    let rebuilt_pushers = start(&rebuilt);
    eventually("the rebuilt machine's store replaces it", || async {
        stores() == (2, Some(0))
    })
    .await;

    let old_pushers = start(&old);
    eventually("both stores are conflicted", || async {
        stores() == (2, Some(2))
    })
    .await;
    eventually("the node is down and alerting", || {
        alerting(&hub.store, "hub", "node:spoke")
    })
    .await;
    let told: i64 = hub
        .read()
        .query_row(
            "SELECT count(*) FROM check_result
             WHERE node_id = 'hub' AND check_id = 'node:spoke' AND error LIKE '%retire one%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(told > 0, "the node went down without the reason");
    old_pushers.stop().await;
    rebuilt_pushers.stop().await;
    monitor.stop().await;
}

/// When `[[node]] retired = true` takes effect.
#[derive(Debug, Clone, Copy)]
enum Retiring {
    AtStart,
    AtReload,
}

/// Retiring a node withdraws the outages it has open on the hub, whether
/// the hub starts with it retired or a reload retires it.
#[tokio::test]
async fn retiring_a_node_withdraws_its_open_outages_at_start_and_at_reload() {
    for when in [Retiring::AtStart, Retiring::AtReload] {
        let hub = hub().await;
        let spoke = spoke(&hub);
        let id = spoke
            .store
            .open_outage(outage(tocsin_store::now_ms() - 60_000))
            .await
            .unwrap();
        let pushers = spoke.start(10);
        eventually("the hub holds the open outage", || async {
            hub.outages() == [(id, false)]
        })
        .await;
        pushers.stop().await;

        let text = watched(&config_text("hub", "http://hub.invalid"));
        let retired = text
            .replace("push_to = [\"hub\"]", "push_to = [\"hub\"]\nretired = true")
            .replace(
                "default_nodes = [\"hub\", \"spoke\"]",
                "default_nodes = [\"hub\"]",
            );
        let retired = load_text("hub", retired, Some(&hub_key()));
        let monitor = match when {
            Retiring::AtStart => {
                Monitor::start(&retired, &hub.store, &EventBus::new(), &Metrics::new())
                    .await
                    .unwrap()
            }
            Retiring::AtReload => {
                let running = load_text("hub", text, Some(&hub_key()));
                let mut monitor =
                    Monitor::start(&running, &hub.store, &EventBus::new(), &Metrics::new())
                        .await
                        .unwrap();
                assert_eq!(hub.outages(), [(id, false)], "{when:?}: withdrawn too soon");
                monitor.reload(&retired).await.unwrap();
                monitor
            }
        };
        let ending: String = hub
            .read()
            .query_row(
                "SELECT ending FROM outage WHERE node_id = 'spoke' AND origin_id = ?1",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ending, "withdrawn", "{when:?}");
        monitor.stop().await;
    }
}
