// SPDX-License-Identifier: AGPL-3.0-only

//! Fakes for the engine's ports, a configuration builder and helpers to
//! read what the engine emitted. Every fake is synchronous inside, so a
//! paused tokio clock only advances when every task is waiting on a timer.

#![allow(dead_code, reason = "each test file uses a different subset")]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tocsin_core::Health;
use tocsin_core::config::{Config, Environment, Source, load};
use tocsin_core::probe::{CompiledCheck, ProbeError, ProbeErrorKind, ProbeOutcome};
use tocsin_engine::ports::{BoxFuture, Clock, History, Notices, Runner, Sink, TokioClock};
use tocsin_engine::{CheckChange, ComponentChange, EngineEvent, Notice, Parts, Plan};
use tocsin_store::{Error, LastUp, NewCheckResult, Outage, OutageChange, Result};
use tokio::time::Instant;

/// Wall-clock time when a test's engine starts.
pub const T0: i64 = 1_800_000_000_000;

/// Loads `toml` as the only configuration file, on node `node-a`.
pub fn config(toml: &str) -> Config {
    let sources = [Source {
        name: "test.toml".to_owned(),
        text: Ok(toml.to_owned()),
    }];
    let environment = Environment {
        hostname: "node-a".to_owned(),
        variable: &|_| None,
    };
    load(&sources, &environment).unwrap_or_else(|errors| panic!("bad config: {errors:?}"))
}

pub fn plan(toml: &str) -> Plan {
    Plan::from_config(&config(toml)).expect("a runnable plan")
}

/// A TCP check `id` on `component`, every `interval_s` seconds, with a
/// 10-second timeout.
pub fn check(id: &str, component: Option<&str>, interval_s: u64, confirm: u32) -> String {
    let component = component.map_or(String::new(), |c| format!("component = \"{c}\"\n"));
    format!(
        "[[check]]\nid = \"{id}\"\ntype = \"tcp\"\ntarget = \"{id}.example.com:443\"\n\
         interval = \"{interval_s}s\"\ntimeout = \"10s\"\nconfirm = {confirm}\n{component}\n"
    )
}

pub fn component(id: &str, rule: &str) -> String {
    format!("[[component]]\nid = \"{id}\"\nname = \"{id}\"\nrule = \"{rule}\"\n\n")
}

/// What a scripted probe does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Finish after the runner's delay with this health.
    Report(Health),
    /// Never finish.
    Hang,
}

type Script = dyn Fn(&str, usize, Duration) -> Step + Send + Sync;

/// A runner that answers from a script given the check, how many times it
/// has run before and the time since the runner was made. It stamps each
/// outcome from its clock, which [`parts`] gives the engine too, so engines
/// restarted on the same runner share one timeline.
pub struct FakeRunner {
    origin: Instant,
    pub clock: Arc<dyn Clock>,
    delay: Duration,
    script: Box<Script>,
    running: AtomicUsize,
    peak: AtomicUsize,
    runs: Mutex<HashMap<String, Vec<Duration>>>,
}

impl FakeRunner {
    /// A runner whose clock reads [`T0`] now.
    pub fn new(
        delay: Duration,
        script: impl Fn(&str, usize, Duration) -> Step + Send + Sync + 'static,
    ) -> Arc<Self> {
        FakeRunner::with_clock(Arc::new(TokioClock::starting_at(T0)), delay, script)
    }

    pub fn with_clock(
        clock: Arc<dyn Clock>,
        delay: Duration,
        script: impl Fn(&str, usize, Duration) -> Step + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(FakeRunner {
            origin: Instant::now(),
            clock,
            delay,
            script: Box::new(script),
            running: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            runs: Mutex::new(HashMap::new()),
        })
    }

    /// Every check reports `health` at once.
    pub fn always(health: Health) -> Arc<Self> {
        FakeRunner::new(Duration::ZERO, move |_, _, _| Step::Report(health))
    }

    /// When each run of `check` started, since the runner was made.
    pub fn runs(&self, check: &str) -> Vec<Duration> {
        self.runs
            .lock()
            .unwrap()
            .get(check)
            .cloned()
            .unwrap_or_default()
    }

    /// The most probes that were ever running at once.
    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
}

impl Runner for FakeRunner {
    fn run<'a>(&'a self, check: &'a CompiledCheck) -> BoxFuture<'a, ProbeOutcome> {
        Box::pin(async move {
            let id = check.check().id.to_string();
            let started_at_ms = self.clock.now_ms();
            let elapsed = self.origin.elapsed();
            let index = {
                let mut runs = self.runs.lock().unwrap();
                let runs = runs.entry(id.clone()).or_default();
                runs.push(elapsed);
                runs.len() - 1
            };
            let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            let step = (self.script)(&id, index, elapsed);
            if step == Step::Hang {
                std::future::pending::<()>().await;
            }
            tokio::time::sleep(self.delay).await;
            self.running.fetch_sub(1, Ordering::SeqCst);
            let Step::Report(health) = step else {
                unreachable!()
            };
            let mut outcome = ProbeOutcome::failed(
                check.check(),
                started_at_ms,
                u64::try_from(self.delay.as_millis()).unwrap(),
                ProbeError::new(ProbeErrorKind::Connect, "scripted failure"),
            );
            outcome.health = health;
            if health != Health::Down {
                outcome.error = None;
            }
            outcome
        })
    }
}

/// An in-memory history enforcing the store's outage rules: one open
/// outage per check and node, ended once, never before it started.
#[derive(Default)]
pub struct FakeHistory {
    pub results: Mutex<Vec<NewCheckResult>>,
    pub outages: Mutex<Vec<Outage>>,
    /// How many of the next outage changes fail, writing nothing.
    pub failing_changes: AtomicUsize,
}

impl FakeHistory {
    pub fn new() -> Arc<Self> {
        Arc::new(FakeHistory::default())
    }

    /// `(check, health, started_at_ms)` for every stored result.
    pub fn results(&self) -> Vec<(String, Health, i64)> {
        self.results
            .lock()
            .unwrap()
            .iter()
            .map(|result| (result.check_id.clone(), result.health, result.started_at_ms))
            .collect()
    }

    /// Makes the next `count` outage changes fail.
    pub fn fail_changes(&self, count: usize) {
        self.failing_changes.store(count, Ordering::SeqCst);
    }

    /// `(check, health, alerted)` for every outage, in the order they
    /// opened.
    pub fn alerted(&self) -> Vec<(String, Health, bool)> {
        self.outages
            .lock()
            .unwrap()
            .iter()
            .map(|outage| (outage.check_id.clone(), outage.health, outage.alerted))
            .collect()
    }

    /// `(check, health, started_at, ended_at)` for every outage, in the
    /// order they opened.
    pub fn outages(&self) -> Vec<(String, Health, i64, Option<i64>)> {
        self.outages
            .lock()
            .unwrap()
            .iter()
            .map(|outage| {
                (
                    outage.check_id.clone(),
                    outage.health,
                    outage.started_at_ms,
                    outage.ended_at_ms,
                )
            })
            .collect()
    }
}

impl History for FakeHistory {
    fn append(&self, result: NewCheckResult) -> BoxFuture<'_, Result<i64>> {
        let mut results = self.results.lock().unwrap();
        results.push(result);
        let id = i64::try_from(results.len()).unwrap();
        Box::pin(async move { Ok(id) })
    }

    fn change_outage(&self, change: OutageChange) -> BoxFuture<'_, Result<Option<i64>>> {
        let result = if self
            .failing_changes
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            Err(Error::Batch("scripted write failure".to_owned()))
        } else {
            // Checked on a copy and swapped in, so a failure writes nothing.
            let mut outages = self.outages.lock().unwrap();
            let mut next = outages.clone();
            apply_change(&mut next, change).inspect(|_| *outages = next)
        };
        Box::pin(async move { result })
    }

    fn open_outages<'a>(&'a self, node_id: &'a str) -> BoxFuture<'a, Result<Vec<Outage>>> {
        let open = self
            .outages
            .lock()
            .unwrap()
            .iter()
            .filter(|outage| outage.node_id == node_id && outage.ended_at_ms.is_none())
            .cloned()
            .collect();
        Box::pin(async move { Ok(open) })
    }

    fn last_up<'a>(
        &'a self,
        node_id: &'a str,
        check_ids: Vec<String>,
    ) -> BoxFuture<'a, Result<Vec<LastUp>>> {
        let results = self.results.lock().unwrap();
        let outages = self.outages.lock().unwrap();
        let found = check_ids
            .into_iter()
            .filter_map(|check_id| {
                let mine = || {
                    results
                        .iter()
                        .filter(|result| result.check_id == check_id && result.node_id == node_id)
                };
                let last_up_ms = mine()
                    .filter(|result| result.health == Health::Up)
                    .map(|result| result.started_at_ms)
                    .max()?;
                let since_ms = outages
                    .iter()
                    .filter(|outage| outage.check_id == check_id && outage.node_id == node_id)
                    .filter_map(|outage| outage.ended_at_ms)
                    .max()
                    .or_else(|| mine().map(|result| result.started_at_ms).min())?;
                Some(LastUp {
                    check_id,
                    last_up_ms,
                    since_ms,
                })
            })
            .collect();
        Box::pin(async move { Ok(found) })
    }
}

/// Applies `change` to `outages` under the store's rules: the ended outage
/// is open and ends no earlier than it started, at most one outage per check
/// and node is open, and none is up.
fn apply_change(outages: &mut Vec<Outage>, change: OutageChange) -> Result<Option<i64>> {
    if let Some(end) = change.end {
        match outages.iter_mut().find(|outage| outage.id == end.id) {
            Some(outage)
                if outage.ended_at_ms.is_none() && end.ended_at_ms >= outage.started_at_ms =>
            {
                outage.ended_at_ms = Some(end.ended_at_ms);
            }
            _ => return Err(Error::Batch(format!("cannot end outage {}", end.id))),
        }
    }
    let Some(outage) = change.open else {
        return Ok(None);
    };
    if outages.iter().any(|open| {
        open.ended_at_ms.is_none()
            && open.check_id == outage.check_id
            && open.node_id == outage.node_id
    }) {
        return Err(Error::Batch(format!(
            "{} already has an open outage",
            outage.check_id
        )));
    }
    if outage.health == Health::Up {
        return Err(Error::Batch("an outage cannot be up".to_owned()));
    }
    let id = i64::try_from(outages.len()).unwrap() + 1;
    outages.push(Outage {
        id,
        node_id: outage.node_id,
        subject_kind: outage.subject_kind,
        check_id: outage.check_id,
        component_id: outage.component_id,
        health: outage.health,
        started_at_ms: outage.started_at_ms,
        ended_at_ms: None,
        alerted: outage.alerted,
    });
    Ok(Some(id))
}

/// Incidents and maintenance windows a test can change.
#[derive(Default)]
pub struct FakeNotices(pub Mutex<Vec<Notice>>);

impl FakeNotices {
    pub fn new(notices: Vec<Notice>) -> Arc<Self> {
        Arc::new(FakeNotices(Mutex::new(notices)))
    }

    pub fn set(&self, notices: Vec<Notice>) {
        *self.0.lock().unwrap() = notices;
    }
}

impl Notices for FakeNotices {
    fn current(&self) -> BoxFuture<'_, Result<Vec<Notice>>> {
        let notices = self.0.lock().unwrap().clone();
        Box::pin(async move { Ok(notices) })
    }
}

/// Collects every event.
#[derive(Default)]
pub struct Events(pub Mutex<Vec<EngineEvent>>);

impl Events {
    pub fn new() -> Arc<Self> {
        Arc::new(Events::default())
    }

    pub fn checks(&self) -> Vec<CheckChange> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                EngineEvent::CheckChanged(change) => Some(change.clone()),
                EngineEvent::ComponentChanged(_) => None,
            })
            .collect()
    }

    pub fn components(&self) -> Vec<ComponentChange> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                EngineEvent::ComponentChanged(change) => Some(change.clone()),
                EngineEvent::CheckChanged(_) => None,
            })
            .collect()
    }
}

impl Sink for Events {
    fn emit(&self, event: EngineEvent) {
        self.0.lock().unwrap().push(event);
    }
}

/// Parts wired to the given fakes and the runner's clock.
pub fn parts(
    runner: &Arc<FakeRunner>,
    history: &Arc<FakeHistory>,
    notices: &Arc<FakeNotices>,
    events: &Arc<Events>,
) -> Parts {
    Parts {
        runner: Arc::clone(runner) as Arc<dyn Runner>,
        history: Arc::clone(history) as Arc<dyn History>,
        notices: Arc::clone(notices) as Arc<dyn Notices>,
        sink: Arc::clone(events) as Arc<dyn Sink>,
        clock: Arc::clone(&runner.clock),
    }
}

/// Milliseconds after [`T0`].
pub fn at(offset: Duration) -> i64 {
    T0 + i64::try_from(offset.as_millis()).unwrap()
}
