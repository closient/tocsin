// SPDX-License-Identifier: AGPL-3.0-only

//! The engine on a paused clock with fake ports: when checks run, when
//! their state changes, what gets emitted and recorded, and how incidents
//! and maintenance windows change what a component shows.

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use common::{FakeHistory, FakeNotices, FakeRunner, Step, T0, at, check, component, parts, plan};
use tocsin_core::Health::{self, Degraded, Down, Up};
use tocsin_core::state::{ComponentStatus, JITTER_DIVISOR, jitter};
use tocsin_engine::{ComponentChange, Engine, Notice};
use tocsin_store::Outage;

const MINUTE: Duration = Duration::from_secs(60);

/// Runs `toml` with a runner reporting `script[n]` on a check's `n`th run
/// (the last entry after that) until `for_`, then stops the engine.
async fn run_script(
    toml: &str,
    script: &'static [Health],
    for_: Duration,
) -> (Arc<FakeRunner>, Arc<FakeHistory>, Arc<common::Events>) {
    let runner = FakeRunner::new(Duration::ZERO, move |_, index, _| {
        Step::Report(script[index.min(script.len() - 1)])
    });
    let history = FakeHistory::new();
    let events = common::Events::new();
    let engine = Engine::start(
        plan(toml),
        parts(&runner, &history, &FakeNotices::new(Vec::new()), &events),
    )
    .await
    .expect("the engine starts");
    tokio::time::sleep(for_).await;
    engine.shutdown().await;
    (runner, history, events)
}

#[tokio::test(start_paused = true)]
async fn first_runs_follow_the_jitter_then_every_interval() {
    let ids: Vec<String> = (0..40).map(|index| format!("check-{index}")).collect();
    let toml: String = ids.iter().map(|id| check(id, None, 60, 1)).collect();
    let (runner, _, _) = run_script(&toml, &[Up], Duration::from_secs(200)).await;

    let mut firsts = BTreeSet::new();
    for id in &ids {
        let runs = runner.runs(id);
        let first = runs[0];
        assert!(
            first * JITTER_DIVISOR <= MINUTE,
            "{id} first ran at {first:?}, past a tenth of its interval"
        );
        assert_eq!(first, jitter(id, MINUTE), "{id}");
        let expected: Vec<Duration> = (0..4).map(|n| first + MINUTE * n).collect();
        assert_eq!(runs, expected, "{id} ran off its interval");
        firsts.insert(first);
    }
    // Spread: the starts are nearly all distinct and cover most of the
    // six-second window rather than bunching at one end.
    assert!(firsts.len() >= 38, "only {} distinct starts", firsts.len());
    let spread = firsts
        .last()
        .unwrap()
        .saturating_sub(*firsts.first().unwrap());
    assert!(
        spread >= Duration::from_secs(4),
        "starts span only {spread:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn state_changes_only_after_confirm_agreeing_results() {
    static SCRIPT: [Health; 12] = [Up, Up, Up, Down, Down, Up, Down, Down, Down, Up, Up, Up];
    let (_, history, events) =
        run_script(&check("api", None, 60, 3), &SCRIPT, Duration::from_mins(12)).await;
    let run = |n: u32| at(jitter("api", MINUTE) + MINUTE * n);

    assert_eq!(history.results().len(), 12, "every raw result is stored");
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
            (None, Up, run(0), run(2)),
            (Some(Up), Down, run(6), run(8)),
            (Some(Down), Up, run(9), run(11)),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn flapping_never_changes_the_state() {
    static SCRIPT: [Health; 41] = {
        let mut script = [Up; 41];
        let mut index = 2;
        while index < 41 {
            if index % 2 == 0 {
                script[index] = Down;
            }
            index += 1;
        }
        script
    };
    let (_, history, events) =
        run_script(&check("api", None, 60, 2), &SCRIPT, Duration::from_mins(41)).await;

    let changes: Vec<_> = events.checks().iter().map(|c| (c.from, c.to)).collect();
    assert_eq!(changes, [(None, Up)]);
    assert_eq!(history.outages(), []);
    let downs = history.results().iter().filter(|r| r.1 == Down).count();
    assert_eq!(downs, 20, "the flapping results are still stored");
}

#[tokio::test(start_paused = true)]
async fn one_event_per_transition_not_per_result() {
    static SCRIPT: [Health; 7] = [Up, Up, Down, Down, Down, Up, Up];
    let toml = component("api", "worst") + &check("api", Some("api"), 60, 1);
    let (_, history, events) = run_script(&toml, &SCRIPT, Duration::from_mins(7)).await;

    assert_eq!(history.results().len(), 7);
    let changes: Vec<_> = events.checks().iter().map(|c| (c.from, c.to)).collect();
    assert_eq!(changes, [(None, Up), (Some(Up), Down), (Some(Down), Up)]);
    let statuses: Vec<_> = events.components().iter().map(|c| (c.from, c.to)).collect();
    assert_eq!(
        statuses,
        [
            (ComponentStatus::Unknown, ComponentStatus::Operational),
            (ComponentStatus::Operational, ComponentStatus::Down),
            (ComponentStatus::Down, ComponentStatus::Operational),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn outages_open_on_leaving_up_and_end_on_return() {
    static SCRIPT: [Health; 5] = [Up, Degraded, Down, Down, Up];
    let toml = component("api", "worst") + &check("api", Some("api"), 60, 1);
    let (_, history, events) = run_script(&toml, &SCRIPT, Duration::from_mins(5)).await;
    let run = |n: u32| at(jitter("api", MINUTE) + MINUTE * n);

    // Degraded to down ends one outage and opens the next at the same time,
    // because an outage's health never changes.
    assert_eq!(
        history.outages(),
        [
            ("api".to_owned(), Degraded, run(1), Some(run(2))),
            ("api".to_owned(), Down, run(2), Some(run(4))),
        ]
    );
    let ids: Vec<_> = events
        .checks()
        .iter()
        .map(|c| (c.to, c.ended_outage_id, c.opened_outage_id))
        .collect();
    assert_eq!(
        ids,
        [
            (Up, None, None),
            (Degraded, None, Some(1)),
            (Down, Some(1), Some(2)),
            (Up, Some(2), None),
        ]
    );
    let outages = history.outages.lock().unwrap();
    assert!(
        outages
            .iter()
            .all(|outage| outage.component_id.as_deref() == Some("api")
                && outage.node_id == "node-a")
    );
}

#[tokio::test(start_paused = true)]
async fn maintenance_records_changes_but_suppresses_alerts() {
    let window_start = Duration::from_secs(150);
    let window_end = Duration::from_secs(400);
    let runner = FakeRunner::new(Duration::ZERO, move |_, _, elapsed| {
        Step::Report(if elapsed >= window_start { Down } else { Up })
    });
    let history = FakeHistory::new();
    let notices = FakeNotices::new(vec![Notice::Maintenance {
        components: vec!["api".into()],
        starts_at_ms: at(window_start),
        ends_at_ms: Some(at(window_end)),
    }]);
    let events = common::Events::new();
    let toml = component("api", "worst") + &check("api", Some("api"), 60, 1);
    let engine = Engine::start(plan(&toml), parts(&runner, &history, &notices, &events))
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_secs(390)).await;
    let api = |engine: &Engine| engine.snapshot().components[0].status;
    assert_eq!(api(&engine), ComponentStatus::Maintenance);
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(api(&engine), ComponentStatus::Down);
    engine.shutdown().await;

    let jitter = jitter("api", MINUTE);
    let first_down = jitter + MINUTE * 3;
    assert!(first_down >= window_start && first_down < window_end);
    let changes: Vec<_> = events
        .checks()
        .iter()
        .map(|c| (c.from, c.to, c.confirmed_at_ms, c.maintenance, c.alerts()))
        .collect();
    assert_eq!(
        changes,
        [
            (None, Up, at(jitter), false, true),
            (Some(Up), Down, at(first_down), true, false),
        ],
        "the change inside the window is emitted, marked as maintenance"
    );
    assert_eq!(
        history.outages(),
        [("api".to_owned(), Down, at(first_down), None)],
        "the outage inside the window is still recorded"
    );
    assert_eq!(
        events.components(),
        [
            status_change(
                ComponentStatus::Unknown,
                ComponentStatus::Operational,
                Some(Up),
                jitter
            ),
            status_change(
                ComponentStatus::Operational,
                ComponentStatus::Maintenance,
                Some(Up),
                window_start
            ),
            status_change(
                ComponentStatus::Maintenance,
                ComponentStatus::Down,
                Some(Down),
                window_end
            ),
        ],
        "the page shows maintenance, not down, until the window ends"
    );
}

fn status_change(
    from: ComponentStatus,
    to: ComponentStatus,
    health: Option<Health>,
    at_: Duration,
) -> ComponentChange {
    ComponentChange {
        component_id: "api".into(),
        from,
        to,
        health,
        at_ms: at(at_),
    }
}

#[tokio::test(start_paused = true)]
async fn a_worse_incident_overrides_component_health() {
    let runner = FakeRunner::always(Up);
    let history = FakeHistory::new();
    let notices = FakeNotices::new(vec![Notice::Incident {
        components: vec!["api".into()],
        impact: Down,
    }]);
    let events = common::Events::new();
    let toml = component("api", "worst") + &check("api", Some("api"), 60, 1);
    let engine = Engine::start(plan(&toml), parts(&runner, &history, &notices, &events))
        .await
        .unwrap();
    let api = |engine: &Engine| engine.snapshot().components[0].clone();

    assert_eq!(api(&engine).status, ComponentStatus::Down);
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(api(&engine).health, Some(Up));
    assert_eq!(
        api(&engine).status,
        ComponentStatus::Down,
        "the incident is worse"
    );
    assert_eq!(events.components(), []);

    notices.set(vec![Notice::Incident {
        components: vec!["api".into()],
        impact: Up,
    }]);
    engine.notices_changed();
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(api(&engine).status, ComponentStatus::Operational);
    let statuses: Vec<_> = events.components().iter().map(|c| (c.from, c.to)).collect();
    assert_eq!(
        statuses,
        [(ComponentStatus::Down, ComponentStatus::Operational)]
    );
    engine.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn worst_and_majority_rules_combine_checks() {
    let toml = component("strict", "worst")
        + &component("quorum", "majority")
        + &["a", "b", "c"]
            .iter()
            .map(|id| check(&format!("strict-{id}"), Some("strict"), 60, 1))
            .collect::<String>()
        + &["a", "b", "c"]
            .iter()
            .map(|id| check(&format!("quorum-{id}"), Some("quorum"), 60, 1))
            .collect::<String>();
    let runner = FakeRunner::new(Duration::ZERO, |id, _, _| {
        Step::Report(if id.ends_with("-a") { Down } else { Up })
    });
    let history = FakeHistory::new();
    let events = common::Events::new();
    let engine = Engine::start(
        plan(&toml),
        parts(&runner, &history, &FakeNotices::new(Vec::new()), &events),
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_secs(30)).await;
    let snapshot = engine.snapshot();
    engine.shutdown().await;

    let statuses: Vec<_> = snapshot
        .components
        .iter()
        .map(|c| (c.id.as_str(), c.health, c.status))
        .collect();
    assert_eq!(
        statuses,
        [
            ("quorum", Some(Up), ComponentStatus::Operational),
            ("strict", Some(Down), ComponentStatus::Down),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn no_more_than_max_concurrent_checks_run_at_once() {
    let toml = "[server]\nmax_concurrent_checks = 3\n\n".to_owned()
        + &(0..20)
            .map(|index| check(&format!("check-{index}"), None, 60, 1))
            .collect::<String>();
    let runner = FakeRunner::new(Duration::from_secs(5), |_, _, _| Step::Report(Up));
    let history = FakeHistory::new();
    let events = common::Events::new();
    let engine = Engine::start(
        plan(&toml),
        parts(&runner, &history, &FakeNotices::new(Vec::new()), &events),
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_secs(55)).await;
    engine.shutdown().await;

    assert_eq!(runner.peak(), 3);
    assert_eq!(history.results().len(), 20, "every check still ran");
}

#[tokio::test(start_paused = true)]
async fn a_probe_past_its_timeout_is_down() {
    let runner = FakeRunner::new(Duration::ZERO, |_, index, _| {
        if index == 0 {
            Step::Hang
        } else {
            Step::Report(Up)
        }
    });
    let history = FakeHistory::new();
    let events = common::Events::new();
    let engine = Engine::start(
        plan(&check("api", None, 60, 1)),
        parts(&runner, &history, &FakeNotices::new(Vec::new()), &events),
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_secs(90)).await;
    engine.shutdown().await;

    let results = history.results.lock().unwrap();
    assert_eq!(results.len(), 2, "the hung run ends and the next one runs");
    assert_eq!(results[0].health, Down);
    assert_eq!(results[0].duration_ms, 10_000);
    let error = results[0].error.as_deref().unwrap_or_default();
    assert!(error.contains("timeout"), "{error}");
    assert_eq!(results[1].health, Up);
}

#[tokio::test(start_paused = true)]
async fn an_open_outage_resumes_after_a_restart() {
    let history = FakeHistory::new();
    let open = |id, check: &str| Outage {
        id,
        node_id: "node-a".into(),
        check_id: check.into(),
        component_id: None,
        health: Down,
        started_at_ms: T0 - 60_000,
        ended_at_ms: None,
    };
    history
        .outages
        .lock()
        .unwrap()
        .extend([open(1, "api"), open(2, "removed")]);
    let runner = FakeRunner::always(Up);
    let events = common::Events::new();
    let engine = Engine::start(
        plan(&check("api", None, 60, 1)),
        parts(&runner, &history, &FakeNotices::new(Vec::new()), &events),
    )
    .await
    .unwrap();
    let state = engine.snapshot().checks[0].state.expect("resumed");
    assert_eq!((state.health, state.since_ms), (Down, T0 - 60_000));
    tokio::time::sleep(Duration::from_secs(30)).await;
    engine.shutdown().await;

    let changes: Vec<_> = events
        .checks()
        .iter()
        .map(|c| (c.from, c.to, c.ended_outage_id))
        .collect();
    assert_eq!(changes, [(Some(Down), Up, Some(1))]);
    let ended = at(jitter("api", MINUTE));
    assert_eq!(
        history.outages(),
        [
            ("api".to_owned(), Down, T0 - 60_000, Some(ended)),
            ("removed".to_owned(), Down, T0 - 60_000, None),
        ]
    );
}
