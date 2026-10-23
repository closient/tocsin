// SPDX-License-Identifier: AGPL-3.0-only

//! `Engine::reconcile` on a paused clock with fake ports: what a reload
//! starts, stops and restarts, what a changed check or heartbeat keeps, and
//! what happens to the open outage of one that is removed.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{
    FakeHistory, FakeNotices, FakeRunner, Step, at, check, component, heartbeat, parts, plan,
};
use tocsin_core::Health::{self, Down, Up};
use tocsin_core::state::{ComponentStatus, jitter};
use tocsin_engine::{Engine, MemberKind, Ping, PingError, StartError};

const MINUTE: Duration = Duration::from_secs(60);
const MS: Duration = Duration::from_millis(1);

async fn start(
    toml: &str,
    runner: &Arc<FakeRunner>,
    history: &Arc<FakeHistory>,
) -> (Engine, Arc<common::Events>) {
    let events = common::Events::new();
    let engine = Engine::start(
        plan(toml),
        parts(runner, history, &FakeNotices::new(Vec::new()), &events),
    )
    .await
    .expect("the engine starts");
    (engine, events)
}

/// Sleeps until `offset` after the runner's clock started.
async fn until(runner: &FakeRunner, offset: Duration) {
    let wait = at(offset) - runner.clock.now_ms();
    tokio::time::sleep(Duration::from_millis(u64::try_from(wait).unwrap())).await;
}

/// A runner reporting `script[n]` on a check's `n`th run (the last entry
/// after that), across reloads.
fn scripted(script: &'static [Health]) -> Arc<FakeRunner> {
    FakeRunner::new(Duration::ZERO, move |_, index, _| {
        Step::Report(script[index.min(script.len() - 1)])
    })
}

fn ids(engine: &Engine) -> Vec<(String, MemberKind)> {
    engine
        .snapshot()
        .checks
        .into_iter()
        .map(|entry| (entry.id, entry.kind))
        .collect()
}

fn up() -> Ping {
    Ping {
        health: Up,
        message: None,
    }
}

#[tokio::test(start_paused = true)]
async fn a_reload_starts_the_added_stops_the_removed_and_leaves_the_rest() {
    let runner = FakeRunner::always(Up);
    let history = FakeHistory::new();
    let (mut engine, _) = start(
        &(check("kept", None, 60, 1) + &check("removed", None, 60, 1)),
        &runner,
        &history,
    )
    .await;
    let reload = MINUTE * 2 + MINUTE / 2;
    until(&runner, reload).await;
    engine
        .reconcile(&plan(
            &(check("kept", None, 60, 1) + &check("added", None, 60, 1)),
        ))
        .await
        .expect("the reload applies");
    assert_eq!(
        ids(&engine),
        [
            ("added".to_owned(), MemberKind::Check),
            ("kept".to_owned(), MemberKind::Check)
        ]
    );
    until(&runner, MINUTE * 5).await;
    engine.shutdown().await;

    let every_minute = |first: Duration, count: u32| -> Vec<Duration> {
        (0..count).map(|n| first + MINUTE * n).collect()
    };
    assert_eq!(
        runner.runs("kept"),
        every_minute(jitter("kept", MINUTE), 5),
        "an unchanged check keeps its schedule"
    );
    assert_eq!(
        runner.runs("removed"),
        every_minute(jitter("removed", MINUTE), 3),
        "a removed check stops at the reload"
    );
    assert_eq!(
        runner.runs("added"),
        every_minute(reload + jitter("added", MINUTE), 3),
        "an added check starts at the reload"
    );
    let removed = history
        .results()
        .into_iter()
        .filter(|(id, _, _)| id == "removed")
        .count();
    assert_eq!(removed, 3, "a removed check's results stay in history");
}

#[tokio::test(start_paused = true)]
async fn a_removed_checks_open_outage_is_withdrawn_without_a_change() {
    let runner = FakeRunner::new(Duration::ZERO, |id, _, _| {
        Step::Report(if id == "api" { Down } else { Up })
    });
    let history = FakeHistory::new();
    let (mut engine, events) = start(
        &(check("api", None, 60, 1) + &check("other", None, 60, 1)),
        &runner,
        &history,
    )
    .await;
    let reload = MINUTE + MINUTE / 2;
    until(&runner, reload).await;
    engine
        .reconcile(&plan(&check("other", None, 60, 1)))
        .await
        .unwrap();
    until(&runner, MINUTE * 5).await;
    engine.shutdown().await;

    let opened = at(jitter("api", MINUTE));
    assert_eq!(
        history.outages(),
        [("api".to_owned(), Down, opened, Some(at(reload)))],
        "ended at the reload, not left open"
    );
    assert_eq!(history.withdrawals(), [("api".to_owned(), at(reload))]);
    let api: Vec<_> = events
        .checks()
        .into_iter()
        .filter(|change| change.check_id == "api")
        .map(|change| (change.from, change.to, change.ended_outage_id))
        .collect();
    assert_eq!(
        api,
        [(None, Down, None)],
        "no recovery is claimed for a check that was removed"
    );
    assert_eq!(runner.runs("api").len(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_changed_check_keeps_its_state_and_its_outage() {
    static SCRIPT: [Health; 3] = [Down, Down, Up];
    let runner = scripted(&SCRIPT);
    let history = FakeHistory::new();
    let (mut engine, events) = start(&check("api", None, 60, 1), &runner, &history).await;
    let reload = MINUTE + MINUTE / 2;
    until(&runner, reload).await;
    let before = engine.snapshot().checks[0].clone();
    engine
        .reconcile(&plan(&check("api", None, 30, 1)))
        .await
        .unwrap();
    let after = engine.snapshot().checks[0].clone();
    assert_eq!(after, before, "state and latest result carry over");
    until(&runner, MINUTE * 2 + MINUTE / 2).await;
    engine.shutdown().await;

    let half_minute = MINUTE / 2;
    let recovered = reload + jitter("api", half_minute);
    assert_eq!(
        runner.runs("api"),
        [
            jitter("api", MINUTE),
            jitter("api", MINUTE) + MINUTE,
            recovered,
            recovered + half_minute,
        ],
        "runs on its new interval from the reload"
    );
    let changes: Vec<_> = events
        .checks()
        .iter()
        .map(|change| (change.from, change.to, change.ended_outage_id))
        .collect();
    assert_eq!(
        changes,
        [(None, Down, None), (Some(Down), Up, Some(1))],
        "the outage opened before the reload is the one that ends"
    );
    assert_eq!(
        history.outages(),
        [(
            "api".to_owned(),
            Down,
            at(jitter("api", MINUTE)),
            Some(at(recovered))
        )]
    );
    assert_eq!(history.withdrawals(), []);
}

#[tokio::test(start_paused = true)]
async fn a_new_confirm_keeps_the_state_and_restarts_the_count() {
    static SCRIPT: [Health; 4] = [Up, Up, Up, Down];
    let runner = scripted(&SCRIPT);
    let history = FakeHistory::new();
    let (mut engine, events) = start(&check("api", None, 60, 3), &runner, &history).await;
    let first = jitter("api", MINUTE);
    // Five runs: up three times, then two of the three downs a change
    // needs.
    let reload = first + MINUTE * 4 + MINUTE / 2;
    until(&runner, reload).await;
    engine
        .reconcile(&plan(&check("api", None, 60, 2)))
        .await
        .unwrap();
    until(&runner, reload + MINUTE * 3).await;
    engine.shutdown().await;

    let restarted = reload + first;
    let changes: Vec<_> = events
        .checks()
        .iter()
        .map(|change| {
            (
                change.from,
                change.to,
                change.since_ms,
                change.confirmed_at_ms,
            )
        })
        .collect();
    assert_eq!(
        changes,
        [
            (None, Up, at(first), at(first + MINUTE * 2)),
            (Some(Up), Down, at(restarted), at(restarted + MINUTE)),
        ],
        "two downs after the reload, not one, confirm the change"
    );
}

#[tokio::test(start_paused = true)]
async fn a_failed_read_leaves_the_old_plan_running() {
    let runner = FakeRunner::always(Up);
    let history = FakeHistory::new();
    let (mut engine, _) = start(&check("api", None, 60, 1), &runner, &history).await;
    let reload = MINUTE / 2;
    until(&runner, reload).await;
    let both = plan(&(check("api", None, 60, 1) + &check("db", None, 60, 1)));
    history.fail_reads(1);
    let error = engine.reconcile(&both).await.expect_err("the read fails");
    assert!(matches!(error, StartError::Outages(_)), "{error:?}");
    assert_eq!(ids(&engine), [("api".to_owned(), MemberKind::Check)]);
    until(&runner, MINUTE * 3).await;
    assert_eq!(runner.runs("db"), Vec::<Duration>::new());
    assert_eq!(runner.runs("api").len(), 3, "the old plan keeps running");

    engine.reconcile(&both).await.expect("a retry applies");
    until(&runner, MINUTE * 4).await;
    engine.shutdown().await;
    assert_eq!(runner.runs("db"), [MINUTE * 3 + jitter("db", MINUTE)]);
}

#[tokio::test(start_paused = true)]
async fn a_changed_heartbeat_measures_its_new_allowance_from_its_latest_ping() {
    let runner = FakeRunner::always(Up);
    let history = FakeHistory::new();
    let (mut engine, _) = start(&heartbeat("backup", None, "1h", "10m"), &runner, &history).await;
    // Taken before the reload, and still good after it.
    let pings = engine.heartbeats();
    let pinged = MINUTE * 10;
    until(&runner, pinged).await;
    pings.ping("backup", up()).await.unwrap();
    until(&runner, MINUTE * 20).await;
    engine
        .reconcile(&plan(&heartbeat("backup", None, "2h", "10m")))
        .await
        .unwrap();
    let deadline = pinged + MINUTE * 130;
    until(&runner, deadline.checked_sub(MS).unwrap()).await;
    assert_eq!(
        history.results(),
        [("backup".to_owned(), Up, at(pinged))],
        "not missed at the old deadline, nor the new one measured from the reload"
    );
    until(&runner, deadline + MINUTE).await;
    pings
        .ping("backup", up())
        .await
        .expect("accepted after the reload");
    engine.shutdown().await;

    assert_eq!(
        history.results(),
        [
            ("backup".to_owned(), Up, at(pinged)),
            ("backup".to_owned(), Down, at(deadline)),
            ("backup".to_owned(), Up, at(deadline + MINUTE)),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn pings_follow_the_reload() {
    let runner = FakeRunner::always(Up);
    let history = FakeHistory::new();
    let (mut engine, _) = start(&heartbeat("backup", None, "1h", "10m"), &runner, &history).await;
    let pings = engine.heartbeats();
    engine
        .reconcile(&plan(&heartbeat("nightly", None, "1d", "1h")))
        .await
        .unwrap();
    assert_eq!(
        pings.ping("backup", up()).await,
        Err(PingError::Unknown),
        "a removed heartbeat refuses pings"
    );
    pings
        .ping("nightly", up())
        .await
        .expect("an added one takes them");
    engine.shutdown().await;
    assert_eq!(
        history.results(),
        [("nightly".to_owned(), Up, at(Duration::ZERO))]
    );
}

#[tokio::test(start_paused = true)]
async fn a_check_that_becomes_a_heartbeat_starts_afresh() {
    let runner = FakeRunner::always(Down);
    let history = FakeHistory::new();
    let (mut engine, events) = start(&check("job", None, 60, 1), &runner, &history).await;
    let reload = MINUTE + MINUTE / 2;
    until(&runner, reload).await;
    engine
        .reconcile(&plan(&heartbeat("job", None, "1h", "10m")))
        .await
        .unwrap();
    let snapshot = engine.snapshot().checks[0].clone();
    assert_eq!(
        (snapshot.kind, snapshot.state, snapshot.last),
        (MemberKind::Heartbeat, None, None)
    );
    let pinged = MINUTE * 2;
    until(&runner, pinged).await;
    engine.heartbeats().ping("job", up()).await.unwrap();
    engine.shutdown().await;

    assert_eq!(
        history.outages(),
        [(
            "job".to_owned(),
            Down,
            at(jitter("job", MINUTE)),
            Some(at(reload))
        )],
        "the check's outage is withdrawn; the heartbeat opens none"
    );
    assert_eq!(history.withdrawals(), [("job".to_owned(), at(reload))]);
    let changes: Vec<_> = events
        .checks()
        .iter()
        .map(|change| (change.kind, change.from, change.to))
        .collect();
    assert_eq!(
        changes,
        [
            (MemberKind::Check, None, Down),
            (MemberKind::Heartbeat, None, Up),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn components_are_rebuilt_from_the_new_plan() {
    let runner = FakeRunner::new(Duration::ZERO, |id, _, _| {
        Step::Report(if id == "api" { Down } else { Up })
    });
    let history = FakeHistory::new();
    let (mut engine, events) = start(
        &(component("api", "worst")
            + &component("web", "worst")
            + &check("api", Some("api"), 60, 1)
            + &check("web", Some("web"), 60, 1)),
        &runner,
        &history,
    )
    .await;
    let reload = MINUTE + MINUTE / 2;
    until(&runner, reload).await;
    // `api` loses its down check and gains `web`; `web` goes; `extra` is
    // new.
    engine
        .reconcile(&plan(
            &(component("api", "worst")
                + &component("extra", "worst")
                + &check("web", Some("api"), 60, 1)
                + &check("x", Some("extra"), 60, 1)),
        ))
        .await
        .unwrap();
    let statuses: Vec<_> = engine
        .snapshot()
        .components
        .into_iter()
        .map(|component| (component.id, component.status))
        .collect();
    engine.shutdown().await;

    assert_eq!(
        statuses,
        [
            ("api".to_owned(), ComponentStatus::Operational),
            ("extra".to_owned(), ComponentStatus::Unknown),
        ]
    );
    let at_reload: Vec<_> = events
        .components()
        .into_iter()
        .filter(|change| change.at_ms == at(reload))
        .map(|change| (change.component_id, change.from, change.to))
        .collect();
    assert_eq!(
        at_reload,
        [(
            "api".to_owned(),
            ComponentStatus::Down,
            ComponentStatus::Operational
        )],
        "a status the reload moves is emitted; a new component's first is not"
    );
}
