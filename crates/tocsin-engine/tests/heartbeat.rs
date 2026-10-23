// SPDX-License-Identifier: AGPL-3.0-only

//! Heartbeats on a paused clock with fake ports: pings on time, late within
//! grace, missed, reporting down, across a restart, and as component
//! members alongside checks.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{
    FakeHistory, FakeNotices, FakeRunner, SkewedClock, Step, at, check, component, heartbeat,
    parts, plan,
};
use tocsin_core::Health::{self, Down, Up};
use tocsin_core::state::{ComponentStatus, jitter};
use tocsin_engine::{Engine, MemberKind, Ping, PingError};
use tocsin_store::SubjectKind;

const MINUTE: Duration = Duration::from_secs(60);
/// The ticket's example: expected hourly, ten minutes' grace.
const INTERVAL: Duration = Duration::from_hours(1);
const GRACE: Duration = Duration::from_mins(10);
const ALLOWANCE: Duration = Duration::from_mins(70);
const MS: Duration = Duration::from_millis(1);

fn backup(component: Option<&str>) -> String {
    heartbeat("backup", component, "1h", "10m")
}

fn up() -> Ping {
    Ping {
        health: Up,
        message: None,
    }
}

async fn start(toml: &str, runner: &Arc<FakeRunner>, history: &Arc<FakeHistory>) -> Engine {
    start_with(toml, runner, history, &common::Events::new()).await
}

async fn start_with(
    toml: &str,
    runner: &Arc<FakeRunner>,
    history: &Arc<FakeHistory>,
    events: &Arc<common::Events>,
) -> Engine {
    Engine::start(
        plan(toml),
        parts(runner, history, &FakeNotices::new(Vec::new()), events),
    )
    .await
    .expect("the engine starts")
}

/// Sleeps until `offset` after the runner's clock started.
/// One millisecond before `offset`: the last moment a ping is on time.
fn just_before(offset: Duration) -> Duration {
    offset.checked_sub(MS).unwrap()
}

async fn until(runner: &FakeRunner, offset: Duration) {
    let now = runner.clock.now_ms();
    let wait = at(offset) - now;
    tokio::time::sleep(Duration::from_millis(u64::try_from(wait).unwrap())).await;
}

/// `(from, to, since, alert)` for every heartbeat change.
fn changes(events: &common::Events) -> Vec<(Option<Health>, Health, i64, bool)> {
    events
        .checks()
        .iter()
        .map(|change| {
            assert_eq!(change.kind, MemberKind::Heartbeat, "{change:?}");
            (change.from, change.to, change.since_ms, change.alert)
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_ping_on_time_keeps_the_heartbeat_up() {
    let runner = FakeRunner::always(Up);
    let history = FakeHistory::new();
    let events = common::Events::new();
    let engine = start_with(&backup(None), &runner, &history, &events).await;
    let pings = engine.heartbeats();
    for hour in 1..=3 {
        until(&runner, INTERVAL * hour).await;
        let received = pings.ping("backup", up()).await.expect("accepted");
        assert_eq!(received, at(INTERVAL * hour));
    }
    // Up to the last millisecond before the next deadline, nothing is missed.
    until(&runner, just_before(INTERVAL * 3 + ALLOWANCE)).await;
    let snapshot = engine.snapshot();
    engine.shutdown().await;

    let results = history.results();
    assert_eq!(
        results,
        (1..=3)
            .map(|hour| ("backup".to_owned(), Up, at(INTERVAL * hour)))
            .collect::<Vec<_>>()
    );
    assert_eq!(changes(&events), [(None, Up, at(INTERVAL), false)]);
    assert_eq!(history.outages(), []);
    let entry = &snapshot.checks[0];
    assert_eq!(entry.kind, MemberKind::Heartbeat);
    assert_eq!(entry.state.map(|state| state.health), Some(Up));
}

#[tokio::test(start_paused = true)]
async fn a_late_ping_within_grace_is_up() {
    let runner = FakeRunner::always(Up);
    let history = FakeHistory::new();
    let events = common::Events::new();
    let engine = start_with(&backup(None), &runner, &history, &events).await;
    let pings = engine.heartbeats();
    until(&runner, INTERVAL).await;
    pings.ping("backup", up()).await.unwrap();
    // An hour late is still within the grace, up to its last millisecond.
    until(&runner, just_before(INTERVAL + INTERVAL + GRACE)).await;
    pings.ping("backup", up()).await.unwrap();
    until(&runner, INTERVAL * 3).await;
    engine.shutdown().await;

    assert_eq!(
        history.results(),
        [
            ("backup".to_owned(), Up, at(INTERVAL)),
            (
                "backup".to_owned(),
                Up,
                at(just_before(INTERVAL + ALLOWANCE))
            ),
        ]
    );
    assert_eq!(changes(&events), [(None, Up, at(INTERVAL), false)]);
}

#[tokio::test(start_paused = true)]
async fn a_missed_deadline_is_a_down_result_at_the_deadline() {
    let runner = FakeRunner::always(Up);
    let history = FakeHistory::new();
    let events = common::Events::new();
    let engine = start_with(&backup(None), &runner, &history, &events).await;
    let pings = engine.heartbeats();
    until(&runner, MINUTE * 10).await;
    pings.ping("backup", up()).await.unwrap();
    let deadline = MINUTE * 10 + ALLOWANCE;
    until(&runner, deadline + MS).await;
    let state = engine.snapshot().checks[0].state.expect("a state");
    assert_eq!((state.health, state.since_ms), (Down, at(deadline)));
    // Silent on: missed again each allowance after the last miss.
    until(&runner, deadline + ALLOWANCE + MS).await;
    engine.shutdown().await;

    assert_eq!(
        history.results(),
        [
            ("backup".to_owned(), Up, at(MINUTE * 10)),
            ("backup".to_owned(), Down, at(deadline)),
            ("backup".to_owned(), Down, at(deadline + ALLOWANCE)),
        ]
    );
    let missed = history.results.lock().unwrap()[1].clone();
    assert_eq!(
        missed.error.as_deref(),
        Some("no ping within the interval of 1h plus 10m grace")
    );
    assert_eq!(
        changes(&events),
        [
            (None, Up, at(MINUTE * 10), false),
            (Some(Up), Down, at(deadline), true),
        ]
    );
    assert_eq!(
        history.outages(),
        [("backup".to_owned(), Down, at(deadline), None)]
    );
}

#[tokio::test(start_paused = true)]
async fn a_ping_after_a_miss_recovers_and_resolves_the_alert() {
    let runner = FakeRunner::always(Up);
    let history = FakeHistory::new();
    let events = common::Events::new();
    let engine = start_with(&backup(None), &runner, &history, &events).await;
    let pings = engine.heartbeats();
    until(&runner, ALLOWANCE + MINUTE * 5).await;
    pings.ping("backup", up()).await.unwrap();
    engine.shutdown().await;

    let recovered = at(ALLOWANCE + MINUTE * 5);
    assert_eq!(
        changes(&events),
        [
            (None, Down, at(ALLOWANCE), true),
            (Some(Down), Up, recovered, true)
        ]
    );
    assert_eq!(
        history.outages(),
        [("backup".to_owned(), Down, at(ALLOWANCE), Some(recovered))]
    );
}

#[tokio::test(start_paused = true)]
async fn a_ping_reporting_down_marks_the_heartbeat_down() {
    let runner = FakeRunner::always(Up);
    let history = FakeHistory::new();
    let events = common::Events::new();
    let engine = start_with(&backup(None), &runner, &history, &events).await;
    let pings = engine.heartbeats();
    until(&runner, MINUTE).await;
    pings.ping("backup", up()).await.unwrap();
    until(&runner, MINUTE * 2).await;
    let down = Ping {
        health: Down,
        message: Some("disk full".to_owned()),
    };
    pings.ping("backup", down).await.unwrap();
    engine.shutdown().await;

    assert_eq!(
        changes(&events),
        [
            (None, Up, at(MINUTE), false),
            (Some(Up), Down, at(MINUTE * 2), true)
        ]
    );
    let stored = history.results.lock().unwrap()[1].clone();
    assert_eq!(stored.health, Down);
    assert_eq!(stored.error.as_deref(), Some("disk full"));
    assert_eq!(
        stored.detail,
        Some(serde_json::json!({ "message": "disk full" }))
    );
    assert_eq!(
        history.outages(),
        [("backup".to_owned(), Down, at(MINUTE * 2), None)]
    );
}

#[tokio::test(start_paused = true)]
async fn a_restart_measures_the_deadline_from_the_last_ping() {
    let runner = FakeRunner::always(Up);
    let history = FakeHistory::new();
    let first = start(&backup(None), &runner, &history).await;
    until(&runner, MINUTE * 50).await;
    first.heartbeats().ping("backup", up()).await.unwrap();
    until(&runner, MINUTE * 60).await;
    first.shutdown().await;

    let events = common::Events::new();
    let second = start_with(&backup(None), &runner, &history, &events).await;
    let state = second.snapshot().checks[0].state.expect("seeded up");
    assert_eq!((state.health, state.since_ms), (Up, at(MINUTE * 50)));
    until(&runner, MINUTE * 50 + ALLOWANCE + MS).await;
    second.shutdown().await;

    // Missed 70 minutes after the ping, not 70 minutes after the restart.
    assert_eq!(
        history.results(),
        [
            ("backup".to_owned(), Up, at(MINUTE * 50)),
            ("backup".to_owned(), Down, at(MINUTE * 50 + ALLOWANCE)),
        ]
    );
    assert_eq!(
        changes(&events),
        [(Some(Up), Down, at(MINUTE * 50 + ALLOWANCE), true)],
        "seeding emits nothing; the miss is the only change"
    );
}

#[tokio::test(start_paused = true)]
async fn a_heartbeat_that_never_pinged_starts_unknown_and_counts_from_start() {
    let runner = FakeRunner::always(Up);
    let history = FakeHistory::new();
    let toml = component("backups", "worst") + &backup(Some("backups"));
    let engine = start(&toml, &runner, &history).await;
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.checks[0].state, None);
    assert_eq!(snapshot.components[0].status, ComponentStatus::Unknown);
    until(&runner, just_before(ALLOWANCE)).await;
    assert!(
        history.results().is_empty(),
        "not missed before its deadline"
    );
    until(&runner, ALLOWANCE + MS).await;
    engine.shutdown().await;
    assert_eq!(
        history.results(),
        [("backup".to_owned(), Down, at(ALLOWANCE))]
    );
}

#[tokio::test(start_paused = true)]
async fn a_restart_long_past_the_deadline_records_one_miss() {
    let runner = FakeRunner::always(Up);
    let history = FakeHistory::new();
    let first = start(&backup(None), &runner, &history).await;
    first.heartbeats().ping("backup", up()).await.unwrap();
    first.shutdown().await;
    // Stopped for ten allowances.
    let restart = ALLOWANCE * 10;
    until(&runner, restart).await;

    let second = start(&backup(None), &runner, &history).await;
    until(&runner, just_before(restart + ALLOWANCE)).await;
    second.shutdown().await;
    assert_eq!(
        history.results(),
        [
            ("backup".to_owned(), Up, at(Duration::ZERO)),
            ("backup".to_owned(), Down, at(ALLOWANCE)),
        ],
        "one miss, at the deadline it missed, not one per allowance slept through"
    );
}

#[tokio::test(start_paused = true)]
async fn a_heartbeat_outage_resumes_across_a_restart_and_ends_on_the_next_ping() {
    let runner = FakeRunner::always(Up);
    let history = FakeHistory::new();
    let first = start(&backup(None), &runner, &history).await;
    until(&runner, ALLOWANCE + MS).await;
    first.shutdown().await;

    let second = start(&backup(None), &runner, &history).await;
    let recovered = ALLOWANCE + MINUTE * 5;
    until(&runner, recovered).await;
    second.heartbeats().ping("backup", up()).await.unwrap();
    second.shutdown().await;

    assert_eq!(
        history.outages(),
        [(
            "backup".to_owned(),
            Down,
            at(ALLOWANCE),
            Some(at(recovered))
        )],
        "the outage opened before the restart is the one the ping ends"
    );
    let kinds: Vec<_> = history
        .outages
        .lock()
        .unwrap()
        .iter()
        .map(|outage| outage.subject_kind)
        .collect();
    assert_eq!(kinds, [SubjectKind::Heartbeat]);
}

/// A runner on a clock the test can push ahead of tokio's time.
fn skewed() -> (Arc<SkewedClock>, Arc<FakeRunner>) {
    let clock = SkewedClock::new();
    let runner = FakeRunner::with_clock(clock.clone(), Duration::ZERO, |_, _, _| Step::Report(Up));
    (clock, runner)
}

#[tokio::test(start_paused = true)]
async fn a_late_wake_stamps_the_miss_at_the_deadline() {
    let (clock, runner) = skewed();
    let history = FakeHistory::new();
    let engine = start(&backup(None), &runner, &history).await;
    until(&runner, ALLOWANCE.checked_sub(MINUTE).unwrap()).await;
    // The wall clock jumps six minutes while the task sleeps on, so it wakes
    // at the deadline's tokio time with the clock reading five minutes past.
    clock.skip(MINUTE * 6);
    tokio::time::sleep(MINUTE + MS).await;
    engine.shutdown().await;
    assert_eq!(
        history.results(),
        [("backup".to_owned(), Down, at(ALLOWANCE))],
        "stamped with the deadline, not with when the task woke"
    );
}

#[tokio::test(start_paused = true)]
async fn a_ping_is_stamped_when_it_arrived_not_when_it_was_taken() {
    let runner = FakeRunner::always(Up);
    let history = FakeHistory::new();
    let engine = start(&backup(None), &runner, &history).await;
    // Each write takes a minute, so a second ping waits behind the first.
    history.delay_writes(MINUTE);
    let pings = engine.heartbeats();
    let first = tokio::spawn({
        let pings = pings.clone();
        async move { pings.ping("backup", up()).await }
    });
    until(&runner, MINUTE / 2).await;
    let second = pings.ping("backup", up()).await;
    assert_eq!(first.await.unwrap(), Ok(at(Duration::ZERO)));
    assert_eq!(
        second,
        Ok(at(MINUTE / 2)),
        "stamped when it arrived, not when the first write let it be taken"
    );
    engine.shutdown().await;
    assert_eq!(
        history.results(),
        [
            ("backup".to_owned(), Up, at(Duration::ZERO)),
            ("backup".to_owned(), Up, at(MINUTE / 2)),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_ping_arriving_after_the_deadline_records_the_miss_first() {
    let (clock, runner) = skewed();
    let history = FakeHistory::new();
    let events = common::Events::new();
    let engine = start_with(&backup(None), &runner, &history, &events).await;
    until(&runner, ALLOWANCE.checked_sub(MINUTE).unwrap()).await;
    // The deadline passes on the wall clock but the task has not woken for
    // it when the ping arrives.
    clock.skip(MINUTE * 6);
    let arrived = ALLOWANCE + MINUTE * 5;
    let stamped = engine.heartbeats().ping("backup", up()).await.unwrap();
    assert_eq!(stamped, at(arrived), "stamped when it arrived");
    engine.shutdown().await;

    assert_eq!(
        history.results(),
        [
            ("backup".to_owned(), Down, at(ALLOWANCE)),
            ("backup".to_owned(), Up, at(arrived)),
        ]
    );
    assert_eq!(
        changes(&events),
        [
            (None, Down, at(ALLOWANCE), true),
            (Some(Down), Up, at(arrived), true),
        ]
    );
    assert_eq!(
        history.outages(),
        [("backup".to_owned(), Down, at(ALLOWANCE), Some(at(arrived)))]
    );
}

#[tokio::test(start_paused = true)]
async fn a_ping_whose_change_cannot_be_stored_is_refused_and_changes_nothing() {
    let runner = FakeRunner::always(Up);
    let history = FakeHistory::new();
    let events = common::Events::new();
    let engine = start_with(&backup(None), &runner, &history, &events).await;
    let pings = engine.heartbeats();
    until(&runner, ALLOWANCE + MS).await;
    let missed = || ("backup".to_owned(), Down, at(ALLOWANCE));

    // The recovery's write fails: the ping is refused and nothing moves.
    history.fail_changes(1);
    until(&runner, ALLOWANCE + MINUTE).await;
    assert_eq!(pings.ping("backup", up()).await, Err(PingError::NotStored));
    let snapshot = engine.snapshot();
    let state = snapshot.checks[0].state.expect("a state");
    assert_eq!((state.health, state.since_ms), (Down, at(ALLOWANCE)));
    assert_eq!(
        snapshot.checks[0]
            .last
            .map(|last| (last.health, last.started_at_ms)),
        Some((Down, at(ALLOWANCE)))
    );
    assert_eq!(
        history.results(),
        [missed()],
        "no result without its change"
    );
    assert_eq!(
        history.outages(),
        [("backup".to_owned(), Down, at(ALLOWANCE), None)]
    );
    // Nor did the deadline move: the heartbeat is missed again one allowance
    // after the last miss.
    until(&runner, ALLOWANCE * 2 + MS).await;
    let missed_again = ("backup".to_owned(), Down, at(ALLOWANCE * 2));
    assert_eq!(history.results(), [missed(), missed_again.clone()]);

    // Pinging again recovers it.
    let recovered = ALLOWANCE * 2 + MINUTE;
    until(&runner, recovered).await;
    assert_eq!(pings.ping("backup", up()).await, Ok(at(recovered)));
    engine.shutdown().await;
    assert_eq!(
        history.results(),
        [
            missed(),
            missed_again,
            ("backup".to_owned(), Up, at(recovered))
        ]
    );
    assert_eq!(
        changes(&events),
        [
            (None, Down, at(ALLOWANCE), true),
            (Some(Down), Up, at(recovered), true),
        ]
    );
    assert_eq!(
        history.outages(),
        [(
            "backup".to_owned(),
            Down,
            at(ALLOWANCE),
            Some(at(recovered))
        )]
    );
}

#[tokio::test(start_paused = true)]
async fn a_miss_that_cannot_be_stored_is_tried_again_with_backoff() {
    let runner = FakeRunner::always(Up);
    let history = FakeHistory::new();
    let engine = start(&backup(None), &runner, &history).await;
    engine.heartbeats().ping("backup", up()).await.unwrap();
    let pinged = ("backup".to_owned(), Up, at(Duration::ZERO));

    // The miss's write fails twice: it is tried again a second later, then
    // two seconds after that.
    history.fail_changes(2);
    until(&runner, just_before(ALLOWANCE + Duration::from_secs(3))).await;
    let state = engine.snapshot().checks[0].state.expect("a state");
    assert_eq!(
        state.health, Up,
        "a miss that was not stored did not happen"
    );
    assert_eq!(history.results(), std::slice::from_ref(&pinged));
    assert_eq!(history.outages(), []);
    until(&runner, ALLOWANCE + Duration::from_secs(3)).await;
    tokio::task::yield_now().await;
    let state = engine.snapshot().checks[0].state.expect("a state");
    assert_eq!((state.health, state.since_ms), (Down, at(ALLOWANCE)));
    engine.shutdown().await;
    assert_eq!(
        history.results(),
        [pinged, ("backup".to_owned(), Down, at(ALLOWANCE))],
        "stamped with the deadline it missed, not with the try that stored it"
    );
    assert_eq!(
        history.outages(),
        [("backup".to_owned(), Down, at(ALLOWANCE), None)]
    );
}

#[tokio::test(start_paused = true)]
async fn heartbeats_are_component_members_like_checks() {
    let runner = FakeRunner::always(Up);
    let history = FakeHistory::new();
    let events = common::Events::new();
    let toml = component("jobs", "worst")
        + &component("nightly", "worst")
        + &check("api", Some("jobs"), 60, 1)
        + &backup(Some("jobs"))
        + &heartbeat("report", Some("nightly"), "1d", "1h");
    let engine = start_with(&toml, &runner, &history, &events).await;
    let pings = engine.heartbeats();
    until(&runner, MINUTE).await;
    pings.ping("backup", up()).await.unwrap();
    pings.ping("report", up()).await.unwrap();
    let statuses = |engine: &Engine| -> Vec<(String, Option<Health>, ComponentStatus)> {
        engine
            .snapshot()
            .components
            .iter()
            .map(|c| (c.id.clone(), c.health, c.status))
            .collect()
    };
    assert_eq!(
        statuses(&engine),
        [
            ("jobs".to_owned(), Some(Up), ComponentStatus::Operational),
            // A component whose only member is a heartbeat is measured.
            ("nightly".to_owned(), Some(Up), ComponentStatus::Operational),
        ]
    );
    // The check stays up; the missed heartbeat takes its component down.
    until(&runner, MINUTE + ALLOWANCE + MS).await;
    assert_eq!(
        statuses(&engine)[0],
        ("jobs".to_owned(), Some(Down), ComponentStatus::Down)
    );
    engine.shutdown().await;
    let kinds: Vec<_> = engine_kinds(&events);
    assert!(kinds.contains(&("api".to_owned(), MemberKind::Check)));
    assert!(kinds.contains(&("backup".to_owned(), MemberKind::Heartbeat)));
    let jobs: Vec<_> = events
        .components()
        .into_iter()
        .filter(|change| change.component_id == "jobs")
        .map(|change| (change.to, change.at_ms))
        .collect();
    let api_first = at(jitter("api", MINUTE));
    assert_eq!(
        jobs,
        [
            (ComponentStatus::Operational, api_first),
            (ComponentStatus::Down, at(MINUTE + ALLOWANCE)),
        ]
    );
}

fn engine_kinds(events: &common::Events) -> Vec<(String, MemberKind)> {
    events
        .checks()
        .into_iter()
        .map(|change| (change.check_id, change.kind))
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_ping_for_an_unknown_id_or_a_stopped_engine_is_refused() {
    let runner = FakeRunner::always(Up);
    let history = FakeHistory::new();
    let engine = start(&backup(None), &runner, &history).await;
    let pings = engine.heartbeats();
    assert!(pings.knows("backup"));
    assert!(!pings.knows("nope"));
    assert_eq!(pings.ping("nope", up()).await, Err(PingError::Unknown));
    engine.shutdown().await;
    assert_eq!(pings.ping("backup", up()).await, Err(PingError::Stopped));
    assert_eq!(history.results(), []);
}
