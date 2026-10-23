// SPDX-License-Identifier: AGPL-3.0-only

//! The engine against a real store, in real time: the outage rows it
//! writes, and a restart that resumes an open outage instead of tripping
//! the one-open-outage-per-check index.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Events, FakeNotices, FakeRunner, config};
use tocsin_core::Health::{Down, Up};
use tocsin_engine::ports::{History, Notices, Runner, Sink, SystemClock};
use tocsin_engine::{Engine, Parts, Plan};
use tocsin_store::Store;

const CHECK: &str = "[[check]]\nid = \"api\"\ntype = \"tcp\"\ntarget = \"api.example.com:443\"\n\
                     interval = \"1s\"\ntimeout = \"500ms\"\nconfirm = 1\n";

/// Runs the `api` check against `store` for a second and a half, every
/// probe reporting `health`.
async fn run(store: &Arc<Store>, health: tocsin_core::Health) -> Arc<Events> {
    let events = Events::new();
    let parts = Parts {
        runner: FakeRunner::always(health) as Arc<dyn Runner>,
        history: Arc::clone(store) as Arc<dyn History>,
        notices: FakeNotices::new(Vec::new()) as Arc<dyn Notices>,
        sink: Arc::clone(&events) as Arc<dyn Sink>,
        clock: Arc::new(SystemClock),
    };
    let plan = Plan::from_config(&config(CHECK)).unwrap();
    let engine = Engine::start(plan, parts).await.expect("the engine starts");
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    engine.shutdown().await;
    events
}

async fn rows(store: &Store, table: &str) -> i64 {
    let info = store.info().await.unwrap();
    info.rows
        .iter()
        .find(|(name, _)| *name == table)
        .map(|(_, count)| *count)
        .unwrap()
}

#[tokio::test]
async fn a_restart_resumes_and_then_ends_the_open_outage() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("tocsin.db")).unwrap());

    let first = run(&store, Down).await;
    let open = store.open_outages("node-a").await.unwrap();
    assert_eq!(open.len(), 1, "{open:?}");
    let outage = &open[0];
    assert_eq!((outage.check_id.as_str(), outage.health), ("api", Down));
    let opened = first.checks();
    assert_eq!(opened.len(), 1, "{opened:?}");
    assert_eq!(opened[0].opened_outage_id, Some(outage.id));
    assert_eq!(opened[0].since_ms, outage.started_at_ms);
    let results = rows(&store, "check_result").await;
    assert!(results >= 1, "{results} results");

    let second = run(&store, Up).await;
    let changes: Vec<_> = second
        .checks()
        .iter()
        .map(|c| (c.from, c.to, c.ended_outage_id, c.opened_outage_id))
        .collect();
    assert_eq!(changes, [(Some(Down), Up, Some(outage.id), None)]);
    assert_eq!(store.open_outages("node-a").await.unwrap(), []);
    assert_eq!(rows(&store, "outage").await, 1, "no second outage opened");
    assert!(rows(&store, "check_result").await > results);

    Arc::try_unwrap(store)
        .unwrap_or_else(|_| panic!("the engines released the store"))
        .close()
        .await
        .unwrap();
}
