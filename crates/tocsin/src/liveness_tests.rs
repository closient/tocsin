// SPDX-License-Identifier: AGPL-3.0-only

//! Which liveness heartbeats a node keeps, and how often a push pings one.
//! That a missed one opens an outage and alerts is shown against real
//! pushes in `push_tests`.

use std::sync::Arc;
use std::time::Duration;

use tocsin_core::Health;
use tocsin_core::config::{Config, Environment, Id, Source};
use tocsin_core::federation::NodeKey;
use tocsin_engine::Ping;
use tocsin_store::Store;

use super::{NODE, Pulse, UPSTREAM, watch};
use crate::http::events::EventBus;
use crate::metrics::Metrics;
use crate::serve::Monitor;

const KEY_VARIABLE: &str = "TOCSIN_TEST_NODE_KEY";

fn key(seed: u8) -> NodeKey {
    NodeKey::from_seed([seed; 32])
}

/// This node is `hub`. `spoke` pushes to it, and so does `old`, which is
/// retired; `edge` pushes to `spoke` alone.
fn text(federation: &str) -> String {
    format!(
        r#"
[server]
node_id = "hub"
node_key = "${{ENV:{KEY_VARIABLE}}}"

[[node]]
id = "hub"
url = "https://hub.example.com"
public_keys = ["{hub}"]

[[node]]
id = "spoke"
url = "https://spoke.example.com"
public_keys = ["{spoke}"]
push_to = ["hub"]

[[node]]
id = "edge"
public_keys = ["{edge}"]
push_to = ["spoke"]

[[node]]
id = "old"
public_keys = ["{old}"]
push_to = ["hub"]
retired = true

[[heartbeat]]
id = "backup"
interval = "1h"
nodes = ["hub"]
{federation}"#,
        hub = key(1).public_key(),
        spoke = key(2).public_key(),
        edge = key(3).public_key(),
        old = key(4).public_key(),
    )
}

const FEDERATION: &str = r#"
[federation]
default_nodes = ["hub", "spoke"]
push_interval = "20s"
grace = "45s"
"#;

fn load(text: &str) -> Config {
    let secret = key(1).expose();
    let variable = |name: &str| (name == KEY_VARIABLE).then(|| secret.clone());
    tocsin_core::config::load(
        &[Source {
            name: "tocsin.toml".into(),
            text: Ok(text.into()),
        }],
        &Environment {
            hostname: "hub".into(),
            node_id: None,
            variable: &variable,
        },
    )
    .unwrap()
}

fn ids(config: &Config) -> Vec<&str> {
    config
        .heartbeats
        .iter()
        .map(|beat| beat.id.as_str())
        .collect()
}

#[test]
fn a_node_watches_each_live_node_pushing_to_it_and_each_node_it_pushes_to() {
    let mut config = load(&text(FEDERATION));
    watch(&mut config, ["spoke", "not an id"]);

    assert_eq!(ids(&config), ["backup", "node:spoke", "upstream:spoke"]);
    for (id, name) in [
        ("node:spoke", "Pushes from spoke"),
        ("upstream:spoke", "Pushes to spoke"),
    ] {
        let beat = config
            .heartbeats
            .iter()
            .find(|b| b.id.as_str() == id)
            .unwrap();
        assert_eq!(beat.name.as_deref(), Some(name));
        assert_eq!(beat.component, None, "{id} stays off the public page");
        assert_eq!(beat.interval.get(), Duration::from_secs(20));
        assert_eq!(beat.grace.get(), Duration::from_secs(45));
    }
}

/// `api` runs on `default_nodes`, hub and spoke, whose results all reach
/// hub: a quorum check hub evaluates, so it watches `quorum:api`, and does
/// so on every reload too, since a reload runs `watch` again.
#[test]
fn a_node_watches_the_quorum_of_each_quorum_check_it_evaluates() {
    let quorum = format!(
        "{FEDERATION}\n[[check]]\nid = \"api\"\ntype = \"tcp\"\ntarget = \"api.example.com:443\"\ninterval = \"10s\"\ntimeout = \"5s\"\n"
    );
    let mut config = load(&text(&quorum));
    watch(&mut config, []);

    assert_eq!(ids(&config), ["backup", "node:spoke", "quorum:api"]);
    let beat = config.heartbeats.last().unwrap();
    assert_eq!(beat.component, None, "quorum:api stays off the public page");
    assert_eq!(beat.interval.get(), Duration::from_secs(10));
    assert_eq!(beat.grace.get(), config.quorum_window(&config.checks[0]));
}

#[test]
fn without_federation_a_node_watches_nothing() {
    let mut config = load("[[heartbeat]]\nid = \"backup\"\ninterval = \"1h\"\n");
    watch(&mut config, ["spoke"]);
    assert_eq!(ids(&config), ["backup"]);
}

#[test]
fn no_configured_id_can_be_a_liveness_one() {
    let spoke = Id::parse("spoke").unwrap();
    for kind in [NODE, UPSTREAM] {
        let synthetic = Id::synthetic(kind, &spoke);
        assert_eq!(synthetic.as_str(), format!("{kind}:spoke"));
        assert!(Id::parse(synthetic.as_str()).is_err(), "{synthetic}");
    }
    let named = text(FEDERATION).replace("id = \"backup\"", "id = \"node:spoke\"");
    let refused = tocsin_core::config::load(
        &[Source {
            name: "tocsin.toml".into(),
            text: Ok(named),
        }],
        &Environment {
            hostname: "hub".into(),
            node_id: None,
            variable: &|name| (name == KEY_VARIABLE).then(|| key(1).expose()),
        },
    );
    let refused = refused.expect_err("a heartbeat named node:spoke loaded");
    assert!(
        refused
            .iter()
            .any(|diagnostic| diagnostic.path.as_deref() == Some("heartbeat[0].id")),
        "{refused:?}"
    );
}

/// A push pings at most twice a `push_interval`: one straight after
/// another is not stored, one half an interval later is.
#[tokio::test]
async fn pushes_back_to_back_ping_once_per_half_interval() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(tocsin_store::DB_FILE);
    let store = Arc::new(Store::open(&path).unwrap());
    let config = load(&text(FEDERATION));
    let metrics = Metrics::new();
    let monitor = Monitor::start(&config, &store, &EventBus::new(), &metrics)
        .await
        .unwrap();
    let heartbeats = monitor.heartbeats();
    let pulse = Pulse::default();
    let interval = Duration::from_millis(400);
    let pings = || {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.busy_timeout(Duration::from_secs(5)).unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM check_result WHERE check_id = 'backup'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap()
    };

    pulse
        .beat(&heartbeats, &metrics.checks, "backup", interval)
        .await;
    pulse
        .beat(&heartbeats, &metrics.checks, "backup", interval)
        .await;
    assert_eq!(pings(), 1, "the second ping came too soon");

    tokio::time::sleep(interval / 2).await;
    pulse
        .beat(&heartbeats, &metrics.checks, "backup", interval)
        .await;
    assert_eq!(pings(), 2, "half an interval later it pings again");

    // A change of health is never held back: a conflict reported straight
    // after a push still lands, and so does the next good push. An hour's
    // interval keeps the same health held back however slow the run.
    let hour = Duration::from_secs(3600);
    let down = || Ping {
        health: Health::Down,
        message: Some("conflicted".into()),
    };
    pulse
        .pulse(&heartbeats, &metrics.checks, "backup", down(), hour)
        .await;
    assert_eq!(
        pings(),
        3,
        "a down ping straight after an up one was held back"
    );
    pulse
        .pulse(&heartbeats, &metrics.checks, "backup", down(), hour)
        .await;
    assert_eq!(pings(), 3, "a second down ping came too soon");
    pulse
        .beat(&heartbeats, &metrics.checks, "backup", hour)
        .await;
    assert_eq!(
        pings(),
        4,
        "an up ping straight after a down one was held back"
    );

    pulse
        .beat(&heartbeats, &metrics.checks, "nonesuch", interval)
        .await;
    monitor.stop().await;
}
