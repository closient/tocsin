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
use tocsin_engine::ports::{BoxFuture, History, Notices, Runner, Sink, TokioClock};
use tocsin_engine::{CheckChange, ComponentChange, EngineEvent, Notice, Parts, Plan};
use tocsin_store::{Error, NewCheckResult, NewOutage, Outage, Result};
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
/// has run before and the time since the runner was made.
pub struct FakeRunner {
    origin: Instant,
    delay: Duration,
    script: Box<Script>,
    running: AtomicUsize,
    peak: AtomicUsize,
    runs: Mutex<HashMap<String, Vec<Duration>>>,
}

impl FakeRunner {
    pub fn new(
        delay: Duration,
        script: impl Fn(&str, usize, Duration) -> Step + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(FakeRunner {
            origin: Instant::now(),
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
                0,
                0,
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

    fn open_outage(&self, outage: NewOutage) -> BoxFuture<'_, Result<i64>> {
        let mut outages = self.outages.lock().unwrap();
        let already = outages.iter().any(|open| {
            open.ended_at_ms.is_none()
                && open.check_id == outage.check_id
                && open.node_id == outage.node_id
        });
        let result = if already {
            Err(Error::Batch(format!(
                "{} already has an open outage",
                outage.check_id
            )))
        } else if outage.health == Health::Up {
            Err(Error::Batch("an outage cannot be up".to_owned()))
        } else {
            let id = i64::try_from(outages.len()).unwrap() + 1;
            outages.push(Outage {
                id,
                node_id: outage.node_id,
                check_id: outage.check_id,
                component_id: outage.component_id,
                health: outage.health,
                started_at_ms: outage.started_at_ms,
                ended_at_ms: None,
            });
            Ok(id)
        };
        Box::pin(async move { result })
    }

    fn end_outage(&self, id: i64, ended_at_ms: i64) -> BoxFuture<'_, Result<()>> {
        let mut outages = self.outages.lock().unwrap();
        let result = match outages.iter_mut().find(|outage| outage.id == id) {
            Some(outage) if outage.ended_at_ms.is_none() && ended_at_ms >= outage.started_at_ms => {
                outage.ended_at_ms = Some(ended_at_ms);
                Ok(())
            }
            _ => Err(Error::Batch(format!("cannot end outage {id}"))),
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

/// Parts wired to the given fakes and a clock reading [`T0`] now.
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
        clock: Arc::new(TokioClock::starting_at(T0)),
    }
}

/// Milliseconds after [`T0`].
pub fn at(offset: Duration) -> i64 {
    T0 + i64::try_from(offset.as_millis()).unwrap()
}
