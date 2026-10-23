// SPDX-License-Identifier: AGPL-3.0-only

//! The running engine: one task per check, one task watching incidents and
//! maintenance windows, and the shared state they update.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::Serialize;
use tocsin_core::Health;
use tocsin_core::config::Check;
use tocsin_core::config::ComponentRule;
use tocsin_core::probe::{CompiledCheck, Facts, ProbeError, ProbeErrorKind, ProbeOutcome};
use tocsin_core::state::{
    ComponentStatus, Confirmation, Confirmed, JITTER_DIVISOR, Transition, component_health,
    component_status, jitter,
};
use tocsin_store::{LastUp, NewCheckResult, NewOutage, Outage, OutageChange, OutageEnd};
use tokio::sync::{Notify, Semaphore, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{error, warn};

use crate::event::{CheckChange, ComponentChange, EngineEvent};
use crate::notice::{Notice, in_maintenance, incident_impact, next_boundary};
use crate::plan::Plan;
use crate::ports::{Clock, History, Notices, Runner, Sink};

/// How long the notices task sleeps when no maintenance boundary is due
/// sooner, so incidents changed without a [`Engine::notices_changed`] call
/// still show within this time.
pub const NOTICES_REFRESH: Duration = Duration::from_secs(60);

/// How long past a check's timeout the engine waits for a probe before
/// abandoning it. Probes enforce the timeout themselves, so this only fires
/// on a bug, which it logs as an error.
pub const PROBE_BACKSTOP: Duration = Duration::from_secs(5);

/// The engine's connections to the outside world.
#[derive(Clone)]
pub struct Parts {
    /// Runs probes.
    pub runner: Arc<dyn Runner>,
    /// Stores results and outages.
    pub history: Arc<dyn History>,
    /// Reads incidents and maintenance windows.
    pub notices: Arc<dyn Notices>,
    /// Receives every change.
    pub sink: Arc<dyn Sink>,
    /// Tells the time.
    pub clock: Arc<dyn Clock>,
}

impl fmt::Debug for Parts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Parts").finish_non_exhaustive()
    }
}

/// The engine could not start.
#[derive(Debug)]
pub enum StartError {
    /// Reading the outages left open before a restart failed.
    Outages(tocsin_store::Error),
    /// Reading when checks were last up failed.
    Results(tocsin_store::Error),
    /// Reading incidents and maintenance windows failed.
    Notices(tocsin_store::Error),
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StartError::Outages(error) => write!(f, "reading open outages: {error}"),
            StartError::Results(error) => write!(f, "reading recent results: {error}"),
            StartError::Notices(error) => {
                write!(f, "reading incidents and maintenance windows: {error}")
            }
        }
    }
}

impl std::error::Error for StartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StartError::Outages(error)
            | StartError::Results(error)
            | StartError::Notices(error) => Some(error),
        }
    }
}

/// The engine's view of every check and component at one moment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Snapshot {
    /// Every check, ordered by id.
    pub checks: Vec<CheckSnapshot>,
    /// Every component, ordered by id.
    pub components: Vec<ComponentSnapshot>,
}

/// One check in a [`Snapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckSnapshot {
    /// The check.
    pub id: String,
    /// Its component.
    pub component_id: Option<String>,
    /// Its confirmed state, once it has one.
    pub state: Option<Confirmed>,
    /// Its latest raw result, which may not be confirmed yet.
    pub last: Option<LastResult>,
}

/// A check's latest raw result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct LastResult {
    /// What it found.
    pub health: Health,
    /// When it started (unix milliseconds).
    pub started_at_ms: i64,
}

/// One component in a [`Snapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ComponentSnapshot {
    /// The component.
    pub id: String,
    /// Its health from its checks alone.
    pub health: Option<Health>,
    /// What the status page shows.
    pub status: ComponentStatus,
}

/// Tells a running engine incidents or maintenance windows changed; see
/// [`Engine::notices_handle`]. Calls after the engine stopped do nothing.
#[derive(Debug, Clone)]
pub struct NoticesHandle(Arc<Notify>);

impl NoticesHandle {
    /// Asks the engine to re-read incidents and maintenance windows now.
    pub fn notices_changed(&self) {
        self.0.notify_one();
    }
}

/// A running engine. Call [`Engine::shutdown`] to stop it and wait for its
/// tasks; dropping it also stops them, without waiting.
#[derive(Debug)]
pub struct Engine {
    shared: Arc<Shared>,
    stop: watch::Sender<bool>,
    tasks: Vec<(String, JoinHandle<()>)>,
}

impl Engine {
    /// Starts running `plan`.
    ///
    /// Before any check runs it seeds every check's state from what this
    /// node recorded before a restart (see [`seed_checks`]) and reads the
    /// current incidents and maintenance windows, so the first statuses are
    /// right. Seeding emits nothing.
    ///
    /// # Errors
    ///
    /// A read fails.
    pub async fn start(plan: Plan, parts: Parts) -> Result<Engine, StartError> {
        let mut seeds = seed_checks(&plan, parts.history.as_ref(), parts.clock.now_ms()).await?;
        let notices = parts.notices.current().await.map_err(StartError::Notices)?;

        let mut runs = Vec::with_capacity(plan.checks.len());
        let mut checks = BTreeMap::new();
        for compiled in &plan.checks {
            let check = compiled.check();
            let id = check.id.to_string();
            let component = check.component.as_ref().map(ToString::to_string);
            let Seed {
                confirmation,
                outage,
            } = seeds
                .remove(&id)
                .unwrap_or_else(|| Seed::unknown(check.confirm));
            checks.insert(
                id,
                CheckEntry {
                    component: component.clone(),
                    state: confirmation.state(),
                    last: None,
                },
            );
            runs.push(CheckRun {
                compiled: Arc::clone(compiled),
                component,
                confirmation,
                outage,
            });
        }
        let mut components = BTreeMap::new();
        for (id, rule) in &plan.components {
            let members = checks
                .iter()
                .filter(|(_, entry)| entry.component.as_deref() == Some(id))
                .map(|(check, _)| check.clone())
                .collect();
            components.insert(
                id.clone(),
                ComponentEntry {
                    rule: *rule,
                    checks: members,
                    health: None,
                    status: ComponentStatus::Unknown,
                },
            );
        }
        let mut state = State {
            checks,
            components,
            notices,
        };
        // The starting statuses are not changes, so nothing is emitted.
        let _ = state.refresh_all(parts.clock.now_ms());

        let shared = Arc::new(Shared {
            node_id: plan.node_id,
            limit: Semaphore::new(plan.max_concurrent),
            state: Mutex::new(state),
            notices_changed: Arc::new(Notify::new()),
            parts,
        });
        let (stop, stopped) = watch::channel(false);
        let start = Instant::now();
        let mut tasks = Vec::with_capacity(runs.len() + 1);
        for run in runs {
            let name = run.compiled.check().id.to_string();
            let handle = tokio::spawn(check_loop(Arc::clone(&shared), run, start, stopped.clone()));
            tasks.push((name, handle));
        }
        tasks.push((
            "notices".to_owned(),
            tokio::spawn(notices_loop(Arc::clone(&shared), stopped)),
        ));
        Ok(Engine {
            shared,
            stop,
            tasks,
        })
    }

    /// The current state of every check and component.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        self.shared.lock().snapshot()
    }

    /// Tells the engine incidents or maintenance windows changed, so it
    /// re-reads them now instead of at the next refresh.
    pub fn notices_changed(&self) {
        self.shared.notices_changed.notify_one();
    }

    /// A handle that does what [`Engine::notices_changed`] does, for a task
    /// that outlives its borrow of the engine (a bus subscriber). It holds
    /// nothing else of the engine's, so it never keeps the store open.
    #[must_use]
    pub fn notices_handle(&self) -> NoticesHandle {
        NoticesHandle(Arc::clone(&self.shared.notices_changed))
    }

    /// Stops every task and waits for them. A check whose probe is in
    /// flight abandons it; one already recording its result finishes
    /// recording first.
    pub async fn shutdown(self) {
        // Every task holds a receiver until it ends, so this only fails once
        // they have all ended already.
        let _ = self.stop.send(true);
        for (name, handle) in self.tasks {
            if let Err(join) = handle.await {
                error!(task = %name, error = %join, "an engine task ended abnormally");
            }
        }
    }
}

struct Shared {
    node_id: String,
    limit: Semaphore,
    state: Mutex<State>,
    notices_changed: Arc<Notify>,
    parts: Parts,
}

impl fmt::Debug for Shared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shared")
            .field("node_id", &self.node_id)
            .finish_non_exhaustive()
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // Nothing done under the lock panics by design; if something does,
        // the panic has been reported and the state is still the last
        // consistent update, so carry on rather than stop monitoring.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn emit(&self, event: EngineEvent) {
        self.parts.sink.emit(event);
    }
}

/// A check's private state, owned by its task.
struct CheckRun {
    compiled: Arc<CompiledCheck>,
    component: Option<String>,
    confirmation: Confirmation,
    outage: Option<OpenOutage>,
}

/// A check's open outage, as its task tracks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OpenOutage {
    id: i64,
    /// Whether opening it alerted, so whether ending it resolves.
    alerted: bool,
}

/// Where a check's state starts.
#[derive(Debug)]
struct Seed {
    confirmation: Confirmation,
    outage: Option<OpenOutage>,
}

impl Seed {
    fn unknown(confirm: u32) -> Seed {
        Seed {
            confirmation: Confirmation::new(confirm),
            outage: None,
        }
    }
}

/// Seeds every check in `plan` from what `history` holds for this node, so
/// a restart (or a reload) neither forgets a state nor repeats a change:
///
/// - A check with an open outage resumes in that outage's state, since it
///   began, and the outage stays open.
/// - Otherwise a check whose latest `up` result is recent, within
///   [`up_window_ms`] of `now_ms`, resumes as up since its last outage ended
///   (or its oldest stored result). Fewer than `confirm` results can have
///   followed that one, so a confirmed state would still be up.
/// - Any other check starts with no state.
///
/// Outages open for checks no longer configured are left open, with a
/// warning.
async fn seed_checks(
    plan: &Plan,
    history: &dyn History,
    now_ms: i64,
) -> Result<HashMap<String, Seed>, StartError> {
    let open = history
        .open_outages(&plan.node_id)
        .await
        .map_err(StartError::Outages)?;
    let mut open: HashMap<String, Outage> = open
        .into_iter()
        .map(|outage| (outage.check_id.clone(), outage))
        .collect();
    let without_outage = plan
        .checks
        .iter()
        .map(|compiled| compiled.check().id.to_string())
        .filter(|id| !open.contains_key(id))
        .collect();
    let last_up: HashMap<String, LastUp> = history
        .last_up(&plan.node_id, without_outage)
        .await
        .map_err(StartError::Results)?
        .into_iter()
        .map(|last| (last.check_id.clone(), last))
        .collect();

    let mut seeds = HashMap::with_capacity(plan.checks.len());
    for compiled in &plan.checks {
        let check = compiled.check();
        let id = check.id.to_string();
        let seed = if let Some(outage) = open.remove(&id) {
            Seed {
                confirmation: Confirmation::resume(
                    check.confirm,
                    outage.health,
                    outage.started_at_ms,
                ),
                outage: Some(OpenOutage {
                    id: outage.id,
                    alerted: outage.alerted,
                }),
            }
        } else if let Some(last) = last_up
            .get(&id)
            .filter(|last| now_ms.saturating_sub(last.last_up_ms) <= up_window_ms(check))
        {
            Seed {
                confirmation: Confirmation::resume(check.confirm, Health::Up, last.since_ms),
                outage: None,
            }
        } else {
            Seed::unknown(check.confirm)
        };
        seeds.insert(id, seed);
    }
    for outage in open.values() {
        warn!(
            check = %outage.check_id,
            outage = outage.id,
            "an outage is open for a check that is no longer configured; leaving it open"
        );
    }
    Ok(seeds)
}

/// How long after a check's latest `up` result it can still be confirmed
/// up: the time `confirm` more results take, plus the jitter.
fn up_window_ms(check: &Check) -> i64 {
    let interval = check.interval.get();
    let window = interval * check.confirm.max(1) + interval / JITTER_DIVISOR;
    i64::try_from(window.as_millis()).unwrap_or(i64::MAX)
}

struct State {
    checks: BTreeMap<String, CheckEntry>,
    components: BTreeMap<String, ComponentEntry>,
    notices: Vec<Notice>,
}

struct CheckEntry {
    component: Option<String>,
    state: Option<Confirmed>,
    last: Option<LastResult>,
}

struct ComponentEntry {
    rule: ComponentRule,
    checks: Vec<String>,
    health: Option<Health>,
    status: ComponentStatus,
}

impl State {
    fn maintenance(&self, component: Option<&str>, now_ms: i64) -> bool {
        component.is_some_and(|id| in_maintenance(&self.notices, id, now_ms))
    }

    /// Recomputes one component; returns the change if its status moved.
    fn refresh(&mut self, id: &str, now_ms: i64) -> Option<ComponentChange> {
        let entry = self.components.get(id)?;
        let healths: Vec<Option<Health>> = entry
            .checks
            .iter()
            .map(|check| {
                self.checks
                    .get(check)
                    .and_then(|check| check.state)
                    .map(|state| state.health)
            })
            .collect();
        let health = component_health(entry.rule, &healths);
        let status = component_status(
            health,
            incident_impact(&self.notices, id),
            in_maintenance(&self.notices, id, now_ms),
        );
        let entry = self.components.get_mut(id)?;
        let from = entry.status;
        entry.health = health;
        entry.status = status;
        (from != status).then(|| ComponentChange {
            component_id: id.to_owned(),
            from,
            to: status,
            health,
            at_ms: now_ms,
        })
    }

    fn refresh_all(&mut self, now_ms: i64) -> Vec<ComponentChange> {
        let ids: Vec<String> = self.components.keys().cloned().collect();
        ids.iter()
            .filter_map(|id| self.refresh(id, now_ms))
            .collect()
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            checks: self
                .checks
                .iter()
                .map(|(id, entry)| CheckSnapshot {
                    id: id.clone(),
                    component_id: entry.component.clone(),
                    state: entry.state,
                    last: entry.last,
                })
                .collect(),
            components: self
                .components
                .iter()
                .map(|(id, entry)| ComponentSnapshot {
                    id: id.clone(),
                    health: entry.health,
                    status: entry.status,
                })
                .collect(),
        }
    }
}

/// Runs one check forever: first at the start plus its jitter, then every
/// interval. A run that cannot start on time because every slot is taken
/// starts late, and the ticks it overran are skipped rather than run in a
/// burst.
async fn check_loop(
    shared: Arc<Shared>,
    mut run: CheckRun,
    start: Instant,
    mut stopped: watch::Receiver<bool>,
) {
    let check = run.compiled.check();
    let interval = check.interval.get();
    let mut ticks = tokio::time::interval_at(start + jitter(check.id.as_str(), interval), interval);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = ticks.tick() => {}
            _ = stopped.changed() => return,
        }
        let outcome = tokio::select! {
            outcome = probe(&shared, &run.compiled) => outcome,
            _ = stopped.changed() => return,
        };
        let Some(outcome) = outcome else {
            return;
        };
        record(&shared, &mut run, &outcome).await;
    }
}

/// Waits for a free slot, then runs the probe, which enforces the check's
/// timeout and stamps its own start and duration. Resolves with the
/// outcome, or `None` if the slots have been closed.
///
/// A probe still running [`PROBE_BACKSTOP`] past its timeout is abandoned
/// and logged as a bug, and the run counts as down.
async fn probe(shared: &Shared, compiled: &CompiledCheck) -> Option<ProbeOutcome> {
    let _permit = shared.limit.acquire().await.ok()?;
    let check = compiled.check();
    let started_at_ms = shared.parts.clock.now_ms();
    let backstop = check.timeout.get() + PROBE_BACKSTOP;
    let outcome = tokio::time::timeout(backstop, shared.parts.runner.run(compiled))
        .await
        .unwrap_or_else(|_| {
            error!(
                check = %check.id,
                timeout = %check.timeout,
                "bug: a probe ran {}s past its own timeout and was abandoned",
                PROBE_BACKSTOP.as_secs()
            );
            ProbeOutcome::failed(
                check,
                started_at_ms,
                u64::try_from(backstop.as_millis()).unwrap_or(u64::MAX),
                ProbeError::new(
                    ProbeErrorKind::Timeout,
                    format!(
                        "no result within the check's timeout of {}; abandoned {}s later",
                        check.timeout,
                        PROBE_BACKSTOP.as_secs()
                    ),
                ),
            )
        });
    Some(outcome)
}

/// Stores a result and feeds it to the check's confirmation. On a confirmed
/// change it writes the outage records, and only once they are written
/// moves the check's state, announces the change and updates the shared
/// state. If the write fails the state stays put and the next result that
/// continues the run tries the change again.
async fn record(shared: &Shared, run: &mut CheckRun, outcome: &ProbeOutcome) {
    let check_id = run.compiled.check().id.to_string();
    let health = outcome.health;
    let started_at_ms = outcome.started_at_ms;
    if let Err(error) = shared
        .parts
        .history
        .append(new_result(&shared.node_id, &check_id, outcome))
        .await
    {
        error!(check = %check_id, %error, "storing a check result failed");
    }

    let change = match run.confirmation.observe(health, started_at_ms) {
        Some(transition) => {
            let change = write_change(shared, run, &check_id, &transition).await;
            if change.is_some() {
                run.confirmation.commit(&transition);
            }
            change
        }
        None => None,
    };

    let now_ms = shared.parts.clock.now_ms();
    let mut state = shared.lock();
    if let Some(entry) = state.checks.get_mut(&check_id) {
        entry.last = Some(LastResult {
            health,
            started_at_ms,
        });
        entry.state = run.confirmation.state();
    }
    let Some(change) = change else {
        return;
    };
    shared.emit(EngineEvent::CheckChanged(change));
    if let Some(component) = &run.component
        && let Some(change) = state.refresh(component, now_ms)
    {
        shared.emit(EngineEvent::ComponentChanged(change));
    }
}

/// Writes the outage records for `transition` in one store transaction:
/// ends the check's open outage, and opens one for the new state if it is
/// degraded or down, both at the time the new state began. A move between
/// degraded and down ends one outage and opens another, because an
/// outage's health never changes.
///
/// Resolves with the change to announce, or `None` if the write failed, in
/// which case nothing was written and the check still tracks its old
/// outage.
async fn write_change(
    shared: &Shared,
    run: &mut CheckRun,
    check_id: &str,
    transition: &Transition,
) -> Option<CheckChange> {
    let maintenance = {
        let now_ms = shared.parts.clock.now_ms();
        shared.lock().maintenance(run.component.as_deref(), now_ms)
    };
    let alert = alerts(transition, run.outage, maintenance);
    let change = OutageChange {
        end: run.outage.map(|outage| OutageEnd {
            id: outage.id,
            ended_at_ms: transition.since_ms,
        }),
        open: (transition.to != Health::Up).then(|| NewOutage {
            node_id: shared.node_id.clone(),
            check_id: check_id.to_owned(),
            component_id: run.component.clone(),
            health: transition.to,
            started_at_ms: transition.since_ms,
            alerted: alert,
        }),
    };
    // A first state of up has no outage to end or open.
    let written = if change.end.is_none() && change.open.is_none() {
        Ok(None)
    } else {
        shared.parts.history.change_outage(change).await
    };
    match written {
        Ok(opened) => {
            let ended = run.outage.map(|outage| outage.id);
            run.outage = opened.map(|id| OpenOutage { id, alerted: alert });
            Some(CheckChange {
                check_id: check_id.to_owned(),
                component_id: run.component.clone(),
                from: transition.from,
                to: transition.to,
                since_ms: transition.since_ms,
                confirmed_at_ms: transition.confirmed_at_ms,
                ended_outage_id: ended,
                opened_outage_id: opened,
                maintenance,
                alert,
            })
        }
        Err(error) => {
            error!(
                check = %check_id,
                from = ?transition.from,
                to = %transition.to,
                %error,
                "recording a state change failed; the check keeps its old state and the next \
                 result that confirms the change retries it"
            );
            None
        }
    }
}

/// Whether alerting notifies about `transition`, given the outage it ends,
/// so that every alert gets exactly one resolve:
///
/// - Leaving up, or a first state that is not up, alerts unless a
///   maintenance window covers the check.
/// - Moving between degraded and down alerts if the outage it ends alerted,
///   so that alert is updated rather than left behind, or else unless a
///   window covers the check.
/// - Returning to up resolves exactly when the outage it ends alerted,
///   whatever window is active now.
/// - A first state of up alerts nothing.
fn alerts(transition: &Transition, ending: Option<OpenOutage>, maintenance: bool) -> bool {
    let ending_alerted = ending.is_some_and(|outage| outage.alerted);
    match transition.to {
        Health::Up => ending_alerted,
        Health::Degraded | Health::Down => ending_alerted || !maintenance,
    }
}

/// The stored form of a result: assertion verdicts as the detail, the
/// probe error's message as the error, and the HTTP status if there was
/// one.
fn new_result(node_id: &str, check_id: &str, outcome: &ProbeOutcome) -> NewCheckResult {
    let detail = if outcome.assertions.is_empty() {
        None
    } else {
        match serde_json::to_value(&outcome.assertions) {
            Ok(detail) => Some(detail),
            Err(error) => {
                error!(check = %check_id, %error, "serializing assertion verdicts failed");
                None
            }
        }
    };
    let status_code = match &outcome.facts {
        Some(Facts::Http(facts)) => Some(facts.status),
        _ => None,
    };
    NewCheckResult {
        node_id: node_id.to_owned(),
        check_id: check_id.to_owned(),
        started_at_ms: outcome.started_at_ms,
        duration_ms: i64::try_from(outcome.duration_ms).unwrap_or(i64::MAX),
        health: outcome.health,
        detail,
        error: outcome.error.as_ref().map(|error| error.message.clone()),
        status_code,
    }
}

/// Re-reads incidents and maintenance windows at every window boundary,
/// whenever [`Engine::notices_changed`] is called, and at least every
/// [`NOTICES_REFRESH`], and emits the component changes they cause.
async fn notices_loop(shared: Arc<Shared>, mut stopped: watch::Receiver<bool>) {
    loop {
        let now_ms = shared.parts.clock.now_ms();
        let next = {
            let state = shared.lock();
            next_boundary(&state.notices, now_ms)
        };
        let wait = next.map_or(NOTICES_REFRESH, |at| {
            Duration::from_millis(u64::try_from(at - now_ms).unwrap_or(0)).min(NOTICES_REFRESH)
        });
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = shared.notices_changed.notified() => {}
            _ = stopped.changed() => return,
        }
        let read = tokio::select! {
            read = shared.parts.notices.current() => read,
            _ = stopped.changed() => return,
        };
        let now_ms = shared.parts.clock.now_ms();
        let mut state = shared.lock();
        match read {
            Ok(notices) => state.notices = notices,
            Err(error) => error!(
                %error,
                "reading incidents and maintenance windows failed; keeping the last ones read"
            ),
        }
        for change in state.refresh_all(now_ms) {
            shared.emit(EngineEvent::ComponentChanged(change));
        }
    }
}
