// SPDX-License-Identifier: AGPL-3.0-only

//! A quorum check across three nodes, end to end through the engine: `hub`
//! runs it and evaluates it, `a` and `b` run it and push their results to
//! `hub`, which this file stands in for by writing them into the history
//! the way an applied push batch does. One node down pages nobody, two
//! open one `*` outage and one alert, two stale lose the quorum, and a node
//! that only runs the check never pages for it.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Events, FakeHistory, FakeNotices, FakeRunner, at, parts};
use tocsin_core::Health::{self, Down, Up};
use tocsin_core::config::{Config, Environment, QUORUM_NODE, Source, load};
use tocsin_core::federation::NodeKey;
use tocsin_engine::{Engine, Plan};
use tocsin_store::{NewCheckResult, Outage, SubjectKind};

const KEY_VARIABLE: &str = "TOCSIN_TEST_NODE_KEY";

fn key(seed: u8) -> NodeKey {
    NodeKey::from_seed([seed; 32])
}

/// `api` runs on `hub`, `a` and `b` every 10 seconds and needs two of them
/// to agree; `a` and `b` push to `hub`, which evaluates it. A node's
/// results count for 30 seconds: two intervals, a push interval and grace.
fn federation() -> String {
    format!(
        r#"
[server]
node_key = "${{ENV:{KEY_VARIABLE}}}"

[[node]]
id = "hub"
url = "https://hub.example.com"
public_keys = ["{hub}"]

[[node]]
id = "a"
public_keys = ["{a}"]
push_to = ["hub"]

[[node]]
id = "b"
public_keys = ["{b}"]
push_to = ["hub"]

[federation]
default_nodes = ["hub"]
push_interval = "5s"
grace = "5s"

[[component]]
id = "site"
name = "Site"

[[check]]
id = "api"
type = "tcp"
target = "api.example.com:443"
interval = "10s"
timeout = "5s"
confirm = 1
component = "site"
nodes = ["hub", "a", "b"]
"#,
        hub = key(1).public_key(),
        a = key(2).public_key(),
        b = key(3).public_key(),
    )
}

/// The configuration as node `node` runs it, with the quorum heartbeats
/// `tocsin serve` adds for the checks it evaluates.
fn config(node: &str) -> Config {
    let seed = match node {
        "hub" => 1,
        "a" => 2,
        _ => 3,
    };
    let variable = move |name: &str| (name == KEY_VARIABLE).then(|| key(seed).expose());
    let mut config = load(
        &[Source {
            name: "tocsin.toml".to_owned(),
            text: Ok(federation()),
        }],
        &Environment {
            hostname: node.to_owned(),
            node_id: Some(node.to_owned()),
            variable: &variable,
        },
    )
    .unwrap_or_else(|errors| panic!("bad config: {errors:?}"));
    let quorum = config.quorum_heartbeats(node);
    config.heartbeats.extend(quorum);
    config
}

/// A spoke: its node, the health it reports, and how many times it pushes
/// before it goes silent.
type Spoke = (&'static str, Health, usize);

/// Writes each spoke's result into `history` every 10 seconds from the
/// start, as pushes would, opening an outage under the spoke when it
/// reports down and ending it when it reports up.
fn push(history: &Arc<FakeHistory>, runner: &Arc<FakeRunner>, spokes: Vec<Spoke>) {
    let history = Arc::clone(history);
    let clock = Arc::clone(&runner.clock);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(10));
        for round in 0.. {
            tick.tick().await;
            let now = clock.now_ms();
            for &(node, health, rounds) in &spokes {
                if round >= rounds {
                    continue;
                }
                history.results.lock().unwrap().push(NewCheckResult {
                    node_id: node.to_owned(),
                    check_id: "api".to_owned(),
                    started_at_ms: now,
                    duration_ms: 1,
                    health,
                    detail: None,
                    error: None,
                    status_code: None,
                });
                let mut outages = history.outages.lock().unwrap();
                let open = outages
                    .iter_mut()
                    .find(|outage| outage.node_id == node && outage.ended_at_ms.is_none());
                match (open, health) {
                    (Some(open), Up) => open.ended_at_ms = Some(now),
                    (None, Up) | (Some(_), _) => {}
                    (None, health) => {
                        let id = i64::try_from(outages.len()).unwrap() + 1;
                        outages.push(Outage {
                            id,
                            node_id: node.to_owned(),
                            subject_kind: SubjectKind::Check,
                            check_id: "api".to_owned(),
                            component_id: Some("site".to_owned()),
                            health,
                            started_at_ms: now,
                            ended_at_ms: None,
                            alerted: false,
                        });
                    }
                }
            }
        }
    });
}

/// Runs `node`'s engine for 80 seconds, its own probe of `api` reporting
/// `own` and the spokes pushing `spokes`.
async fn run(
    node: &str,
    own: Health,
    spokes: Vec<Spoke>,
) -> (Arc<FakeHistory>, Arc<Events>, tocsin_engine::Snapshot) {
    let runner = FakeRunner::always(own);
    let history = FakeHistory::new();
    let events = Events::new();
    let notices = FakeNotices::new(Vec::new());
    push(&history, &runner, spokes);
    let plan = Plan::from_config(&config(node)).expect("a runnable plan");
    let engine = Engine::start(plan, parts(&runner, &history, &notices, &events))
        .await
        .expect("the engine starts");
    tokio::time::sleep(Duration::from_secs(80)).await;
    let snapshot = engine.snapshot();
    engine.shutdown().await;
    (history, events, snapshot)
}

/// `(check, node, health, alerted)` for every outage.
fn outages(history: &FakeHistory) -> Vec<(String, String, Health, bool)> {
    history
        .outages
        .lock()
        .unwrap()
        .iter()
        .map(|outage| {
            (
                outage.check_id.clone(),
                outage.node_id.clone(),
                outage.health,
                outage.alerted,
            )
        })
        .collect()
}

/// `(check, to)` for every change alerting notifies about.
fn alerts(events: &Events) -> Vec<(String, Health)> {
    events
        .checks()
        .into_iter()
        .filter(|change| change.alert)
        .map(|change| (change.check_id, change.to))
        .collect()
}

#[tokio::test(start_paused = true)]
async fn one_node_down_of_three_leaves_the_check_up_and_pages_nobody() {
    let (history, events, snapshot) = run(
        "hub",
        Up,
        vec![("a", Down, usize::MAX), ("b", Up, usize::MAX)],
    )
    .await;

    assert_eq!(
        outages(&history),
        [("api".to_owned(), "a".to_owned(), Down, false)],
        "only a's own outage, pushed to the hub"
    );
    assert_eq!(alerts(&events), []);
    let evaluated: Vec<Health> = history
        .results
        .lock()
        .unwrap()
        .iter()
        .filter(|result| result.node_id == QUORUM_NODE)
        .map(|result| result.health)
        .collect();
    assert!(evaluated.len() >= 4, "{evaluated:?}");
    assert!(
        evaluated.iter().all(|health| *health == Up),
        "{evaluated:?}"
    );
    let api = snapshot
        .checks
        .iter()
        .find(|check| check.id == "api")
        .unwrap();
    assert_eq!(api.state.map(|state| state.health), Some(Up));
}

#[tokio::test(start_paused = true)]
async fn two_nodes_down_open_one_quorum_outage_and_one_alert() {
    let (history, events, snapshot) = run(
        "hub",
        Down,
        vec![("a", Down, usize::MAX), ("b", Up, usize::MAX)],
    )
    .await;

    let mut found = outages(&history);
    found.sort();
    assert_eq!(
        found,
        [
            ("api".to_owned(), QUORUM_NODE.to_owned(), Down, true),
            ("api".to_owned(), "a".to_owned(), Down, false),
            ("api".to_owned(), "hub".to_owned(), Down, false),
        ],
        "the hub's own run is evidence and never alerts; the quorum's outage does"
    );
    assert_eq!(alerts(&events), [("api".to_owned(), Down)]);
    let api = snapshot
        .checks
        .iter()
        .find(|check| check.id == "api")
        .unwrap();
    assert_eq!(api.state.map(|state| state.health), Some(Down));
}

#[tokio::test(start_paused = true)]
async fn two_stale_nodes_lose_the_quorum_rather_than_find_it_down() {
    // a and b push at 0s and 10s and then stop; their results count until
    // 40s, so the evaluations at 30s and 40s find api up and the one at
    // 50s cannot decide.
    let (history, events, snapshot) = run("hub", Up, vec![("a", Up, 2), ("b", Up, 2)]).await;

    let evaluated: Vec<(Health, i64)> = history
        .results
        .lock()
        .unwrap()
        .iter()
        .filter(|result| result.node_id == QUORUM_NODE)
        .map(|result| (result.health, result.started_at_ms))
        .collect();
    assert_eq!(
        evaluated,
        [
            (Up, at(Duration::from_secs(30))),
            (Up, at(Duration::from_secs(40)))
        ],
        "no evaluation without a quorum of fresh results"
    );
    assert_eq!(
        outages(&history),
        [("quorum:api".to_owned(), "hub".to_owned(), Down, true)],
        "quorum lost is its own alert, not an outage of api"
    );
    assert_eq!(alerts(&events), [("quorum:api".to_owned(), Down)]);
    let api = snapshot
        .checks
        .iter()
        .find(|check| check.id == "api")
        .unwrap();
    assert_eq!(api.state, None, "api's health is unknown");
}

#[tokio::test(start_paused = true)]
async fn a_node_that_only_runs_a_quorum_check_never_pages_for_it() {
    let (history, events, _) = run("a", Down, Vec::new()).await;

    assert_eq!(
        outages(&history),
        [("api".to_owned(), "a".to_owned(), Down, false)]
    );
    assert_eq!(alerts(&events), []);
    assert!(
        history
            .results
            .lock()
            .unwrap()
            .iter()
            .all(|result| result.node_id == "a"),
        "a records its own results and evaluates nothing"
    );
}
