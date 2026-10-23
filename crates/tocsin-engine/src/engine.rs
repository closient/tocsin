// SPDX-License-Identifier: AGPL-3.0-only

//! The running engine: one task per check, one per heartbeat, one task
//! watching incidents and maintenance windows, and the shared state they
//! update.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use serde::Serialize;
use tocsin_core::Health;
use tocsin_core::config::{Check, ComponentRule, Heartbeat};
use tocsin_core::probe::{CompiledCheck, Facts, ProbeError, ProbeErrorKind, ProbeOutcome};
use tocsin_core::state::{
    ComponentStatus, Confirmation, Confirmed, JITTER_DIVISOR, Transition, component_health,
    component_status, jitter,
};
use tocsin_store::{
    LastUp, LatestResult, NewCheckResult, NewOutage, Outage, OutageChange, OutageEnd, SubjectKind,
};
use tokio::sync::{Notify, Semaphore, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{error, warn};

use crate::event::{CheckChange, ComponentChange, EngineEvent, MemberKind};
use crate::notice::{Notice, in_maintenance, incident_impact, next_boundary};
use crate::plan::Plan;
use crate::ports::{Clock, History, Notices, Runner, Sink, SystemClock};

/// How long the notices task sleeps when no maintenance boundary is due
/// sooner, so incidents changed without a [`Engine::notices_changed`] call
/// still show within this time.
pub const NOTICES_REFRESH: Duration = Duration::from_secs(60);

/// How long past a check's timeout the engine waits for a probe before
/// abandoning it. Probes enforce the timeout themselves, so this only fires
/// on a bug, which it logs as an error.
pub const PROBE_BACKSTOP: Duration = Duration::from_secs(5);

/// How many agreeing results confirm a heartbeat's change: one. A ping is
/// the job's own report, and a missed deadline has already waited out the
/// grace, so neither needs corroborating.
pub const HEARTBEAT_CONFIRM: u32 = 1;

/// [`PING_MESSAGE_LIMIT`] as a literal, so `concat!` can spell it into
/// help text and API descriptions that would otherwise repeat the number.
#[macro_export]
macro_rules! ping_message_limit {
    () => {
        1024
    };
}

/// The longest message a ping may carry, in bytes of UTF-8.
pub const PING_MESSAGE_LIMIT: usize = ping_message_limit!();

/// Pings queued for one heartbeat before senders wait.
const PING_QUEUE: usize = 16;

/// How long a heartbeat waits to try storing a missed deadline again after
/// the first failure; each further failure doubles it, up to
/// [`MISS_RETRY_MAX`].
const MISS_RETRY_FIRST: Duration = Duration::from_secs(1);

/// The longest a heartbeat waits between tries at storing a missed
/// deadline.
const MISS_RETRY_MAX: Duration = Duration::from_mins(1);

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
    /// Reading when checks were last up, or heartbeats last recorded a
    /// result, failed.
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

/// The engine's view of every check, heartbeat and component at one moment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Snapshot {
    /// Every check and heartbeat, ordered by id.
    pub checks: Vec<CheckSnapshot>,
    /// Every component, ordered by id.
    pub components: Vec<ComponentSnapshot>,
}

/// One check or heartbeat in a [`Snapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckSnapshot {
    /// The check or heartbeat.
    pub id: String,
    /// Which of the two it is.
    pub kind: MemberKind,
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
    /// Its health from its checks and heartbeats alone.
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

/// Reads a running engine's [`Snapshot`], for the HTTP API; see
/// [`Engine::status_handle`]. It holds the engine weakly, so it never keeps
/// the store open; once the engine has stopped it reads an empty snapshot.
#[derive(Debug, Clone)]
pub struct StatusHandle(Weak<Shared>);

impl StatusHandle {
    /// The current state of every check and component; empty once the
    /// engine has stopped.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        self.0
            .upgrade()
            .map(|shared| shared.lock().snapshot())
            .unwrap_or_default()
    }
}

/// A heartbeat ping, as the write API received it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ping {
    /// `Up`, or `Down` when the job reports its own failure.
    pub health: Health,
    /// A short note from the job, stored with the result; at most
    /// [`PING_MESSAGE_LIMIT`] bytes, which the caller checks.
    pub message: Option<String>,
}

/// Why a ping was not recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PingError {
    /// No heartbeat has that id.
    Unknown,
    /// The engine has stopped.
    Stopped,
    /// Storing the ping failed, so nothing changed: the heartbeat's state
    /// and deadline are as they were, and the sender should ping again.
    NotStored,
}

impl fmt::Display for PingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            PingError::Unknown => "no heartbeat has that id",
            PingError::Stopped => "the engine has stopped",
            PingError::NotStored => "storing the ping failed, so it was not recorded",
        })
    }
}

impl std::error::Error for PingError {}

/// Hands pings to a running engine's heartbeats; see [`Engine::heartbeats`].
/// It holds the engine's clock, to stamp each ping as it arrives, and
/// nothing else of the engine's, so it never keeps the store open. The
/// default handle knows no heartbeats.
#[derive(Clone)]
pub struct HeartbeatHandle {
    clock: Arc<dyn Clock>,
    senders: Arc<HashMap<String, mpsc::Sender<PingRequest>>>,
}

impl Default for HeartbeatHandle {
    fn default() -> Self {
        HeartbeatHandle {
            clock: Arc::new(SystemClock),
            senders: Arc::default(),
        }
    }
}

impl fmt::Debug for HeartbeatHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HeartbeatHandle")
            .field("heartbeats", &self.senders.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl HeartbeatHandle {
    /// Whether a heartbeat has id `id`.
    #[must_use]
    pub fn knows(&self, id: &str) -> bool {
        self.senders.contains_key(id)
    }

    /// Records `ping` for heartbeat `id`, stamped with the engine's clock
    /// as it arrives here, and resolves with the time it was recorded at
    /// (unix milliseconds) once the result, and any change of state it
    /// makes, is stored and the heartbeat's state updated. That is the
    /// arrival time, unless the heartbeat already has a later result.
    ///
    /// # Errors
    ///
    /// No heartbeat has that id, the engine has stopped, or storing the
    /// result failed, in which case nothing changed; see [`PingError`].
    pub async fn ping(&self, id: &str, ping: Ping) -> Result<i64, PingError> {
        let sender = self.senders.get(id).ok_or(PingError::Unknown)?;
        let arrived_ms = self.clock.now_ms();
        let (ack, acked) = oneshot::channel();
        sender
            .send(PingRequest {
                ping,
                arrived_ms,
                ack,
            })
            .await
            .map_err(|_| PingError::Stopped)?;
        acked.await.map_err(|_| PingError::Stopped)?
    }
}

/// A ping on its way to a heartbeat's task, with when it arrived and where
/// to report back.
#[derive(Debug)]
struct PingRequest {
    ping: Ping,
    /// Unix milliseconds.
    arrived_ms: i64,
    ack: oneshot::Sender<Result<i64, PingError>>,
}

/// A running engine. Call [`Engine::shutdown`] to stop it and wait for its
/// tasks; dropping it also stops them, without waiting.
#[derive(Debug)]
pub struct Engine {
    shared: Arc<Shared>,
    heartbeats: HeartbeatHandle,
    stop: watch::Sender<bool>,
    tasks: Vec<(String, JoinHandle<()>)>,
}

impl Engine {
    /// Starts running `plan`.
    ///
    /// Before any check runs it seeds every check's and heartbeat's state
    /// from what this node recorded before a restart (see `seed_checks`),
    /// resumes each heartbeat's deadline from its latest stored result (see
    /// `heartbeat_anchors`) and reads the current incidents and
    /// maintenance windows, so the first statuses are right. Seeding emits
    /// nothing.
    ///
    /// # Errors
    ///
    /// A read fails.
    pub async fn start(plan: Plan, parts: Parts) -> Result<Engine, StartError> {
        let start_ms = parts.clock.now_ms();
        let mut seeds = seed_checks(&plan, parts.history.as_ref(), start_ms).await?;
        let anchors = heartbeat_anchors(&plan, parts.history.as_ref()).await?;
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
                id.clone(),
                CheckEntry {
                    kind: MemberKind::Check,
                    component: component.clone(),
                    state: confirmation.state(),
                    last: None,
                },
            );
            runs.push(CheckRun {
                compiled: Arc::clone(compiled),
                tracker: Tracker {
                    id,
                    kind: MemberKind::Check,
                    component,
                    confirmation,
                    outage,
                },
            });
        }
        let (beats, senders) = heartbeat_runs(&plan, &mut seeds, &anchors, start_ms, &mut checks);
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

        let heartbeats = HeartbeatHandle {
            clock: Arc::clone(&parts.clock),
            senders: Arc::new(senders),
        };
        let shared = Arc::new(Shared {
            node_id: plan.node_id,
            limit: Semaphore::new(plan.max_concurrent),
            state: Mutex::new(state),
            notices_changed: Arc::new(Notify::new()),
            parts,
        });
        let (stop, stopped) = watch::channel(false);
        let start = Instant::now();
        let mut tasks = Vec::with_capacity(runs.len() + beats.len() + 1);
        for run in runs {
            let name = run.tracker.id.clone();
            let handle = tokio::spawn(check_loop(Arc::clone(&shared), run, start, stopped.clone()));
            tasks.push((name, handle));
        }
        for run in beats {
            let name = run.tracker.id.clone();
            let handle = tokio::spawn(heartbeat_loop(Arc::clone(&shared), run, stopped.clone()));
            tasks.push((name, handle));
        }
        tasks.push((
            "notices".to_owned(),
            tokio::spawn(notices_loop(Arc::clone(&shared), stopped)),
        ));
        Ok(Engine {
            shared,
            heartbeats,
            stop,
            tasks,
        })
    }

    /// The current state of every check, heartbeat and component.
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

    /// A handle that reads what [`Engine::snapshot`] does, for the HTTP
    /// API, which outlives the engine during shutdown.
    #[must_use]
    pub fn status_handle(&self) -> StatusHandle {
        StatusHandle(Arc::downgrade(&self.shared))
    }

    /// A handle that records heartbeat pings, for the write API. Like
    /// [`Engine::notices_handle`], it holds nothing that keeps the store
    /// open; once the engine stops, its pings fail with
    /// [`PingError::Stopped`].
    #[must_use]
    pub fn heartbeats(&self) -> HeartbeatHandle {
        self.heartbeats.clone()
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

/// What a check's or heartbeat's task tracks between results: its
/// confirmation and its open outage. Results from either go through
/// [`record`], so both reach the same outages and events.
struct Tracker {
    id: String,
    kind: MemberKind,
    component: Option<String>,
    confirmation: Confirmation,
    outage: Option<OpenOutage>,
}

/// A check's private state, owned by its task.
struct CheckRun {
    compiled: Arc<CompiledCheck>,
    tracker: Tracker,
}

/// A heartbeat's private state, owned by its task.
struct HeartbeatRun {
    heartbeat: Heartbeat,
    tracker: Tracker,
    /// `interval + grace`, in milliseconds.
    allowance_ms: i64,
    /// When the heartbeat is missed if no ping arrives first (unix
    /// milliseconds): the latest result's time plus the allowance.
    deadline_ms: i64,
    /// When the latest result was stamped (unix milliseconds), or
    /// `i64::MIN` before the first. A ping is never stamped earlier, so
    /// results stay in the order they were recorded.
    last_ms: i64,
    /// After storing a miss failed: when to try again, and how long that
    /// wait was.
    retry: Option<(Instant, Duration)>,
    pings: mpsc::Receiver<PingRequest>,
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

/// Each heartbeat's run and the sender its pings arrive on, entering each
/// in `checks` as a member of its component.
fn heartbeat_runs(
    plan: &Plan,
    seeds: &mut HashMap<String, Seed>,
    anchors: &HashMap<String, i64>,
    start_ms: i64,
    checks: &mut BTreeMap<String, CheckEntry>,
) -> (
    Vec<HeartbeatRun>,
    HashMap<String, mpsc::Sender<PingRequest>>,
) {
    let mut beats = Vec::with_capacity(plan.heartbeats.len());
    let mut senders = HashMap::with_capacity(plan.heartbeats.len());
    for heartbeat in &plan.heartbeats {
        let id = heartbeat.id.to_string();
        let component = heartbeat.component.as_ref().map(ToString::to_string);
        let Seed {
            confirmation,
            outage,
        } = seeds
            .remove(&id)
            .unwrap_or_else(|| Seed::unknown(HEARTBEAT_CONFIRM));
        checks.insert(
            id.clone(),
            CheckEntry {
                kind: MemberKind::Heartbeat,
                component: component.clone(),
                state: confirmation.state(),
                last: None,
            },
        );
        let (sender, pings) = mpsc::channel(PING_QUEUE);
        senders.insert(id.clone(), sender);
        let allowance_ms = allowance_ms(heartbeat);
        // Measured from the latest stored result, so a restart does not
        // reset the clock; a heartbeat that never reported is measured
        // from now.
        let anchor = anchors.get(&id).copied();
        let anchor_ms = anchor.unwrap_or(start_ms);
        beats.push(HeartbeatRun {
            heartbeat: heartbeat.clone(),
            tracker: Tracker {
                id,
                kind: MemberKind::Heartbeat,
                component,
                confirmation,
                outage,
            },
            allowance_ms,
            deadline_ms: anchor_ms.saturating_add(allowance_ms),
            last_ms: anchor.unwrap_or(i64::MIN),
            retry: None,
            pings,
        });
    }
    (beats, senders)
}

/// Seeds every check and heartbeat in `plan` from what `history` holds for
/// this node, so a restart (or a reload) neither forgets a state nor
/// repeats a change:
///
/// - One with an open outage resumes in that outage's state, since it
///   began, and the outage stays open.
/// - Otherwise one whose latest `up` result is recent resumes as up since
///   its last outage ended (or its oldest stored result). Recent means
///   within [`up_window_ms`] of `now_ms` for a check, since fewer than
///   `confirm` results can have followed that one, so a confirmed state
///   would still be up; and within `interval + grace` for a heartbeat,
///   since no deadline has passed since that ping.
/// - Any other starts with no state.
///
/// Outages open for checks or heartbeats no longer configured are left
/// open, with a warning.
async fn seed_checks(
    plan: &Plan,
    history: &dyn History,
    now_ms: i64,
) -> Result<HashMap<String, Seed>, StartError> {
    // (kind, id, confirm, how long an up result keeps it up)
    let members: Vec<(SubjectKind, String, u32, i64)> = plan
        .checks
        .iter()
        .map(|compiled| {
            let check = compiled.check();
            (
                SubjectKind::Check,
                check.id.to_string(),
                check.confirm,
                up_window_ms(check),
            )
        })
        .chain(plan.heartbeats.iter().map(|heartbeat| {
            (
                SubjectKind::Heartbeat,
                heartbeat.id.to_string(),
                HEARTBEAT_CONFIRM,
                allowance_ms(heartbeat),
            )
        }))
        .collect();
    let open = history
        .open_outages(&plan.node_id)
        .await
        .map_err(StartError::Outages)?;
    // An outage resumes only the kind it was opened for: a check that
    // replaced a heartbeat of the same id does not inherit its outage.
    let mut open: HashMap<(SubjectKind, String), Outage> = open
        .into_iter()
        .map(|outage| ((outage.subject_kind, outage.check_id.clone()), outage))
        .collect();
    let without_outage = members
        .iter()
        .filter(|(kind, id, _, _)| !open.contains_key(&(*kind, id.clone())))
        .map(|(_, id, _, _)| id.clone())
        .collect();
    let last_up: HashMap<String, LastUp> = history
        .last_up(&plan.node_id, without_outage)
        .await
        .map_err(StartError::Results)?
        .into_iter()
        .map(|last| (last.check_id.clone(), last))
        .collect();

    let mut seeds = HashMap::with_capacity(members.len());
    for (kind, id, confirm, window_ms) in members {
        let seed = if let Some(outage) = open.remove(&(kind, id.clone())) {
            Seed {
                confirmation: Confirmation::resume(confirm, outage.health, outage.started_at_ms),
                outage: Some(OpenOutage {
                    id: outage.id,
                    alerted: outage.alerted,
                }),
            }
        } else if let Some(last) = last_up
            .get(&id)
            .filter(|last| now_ms.saturating_sub(last.last_up_ms) <= window_ms)
        {
            Seed {
                confirmation: Confirmation::resume(confirm, Health::Up, last.since_ms),
                outage: None,
            }
        } else {
            Seed::unknown(confirm)
        };
        seeds.insert(id, seed);
    }
    for outage in open.values() {
        warn!(
            kind = %outage.subject_kind,
            check = %outage.check_id,
            outage = outage.id,
            "an outage is open for a check or heartbeat that is no longer configured; leaving it \
             open"
        );
    }
    Ok(seeds)
}

/// When each heartbeat in `plan` last recorded a result on this node, ping
/// or miss (unix milliseconds). Its deadline resumes from there.
async fn heartbeat_anchors(
    plan: &Plan,
    history: &dyn History,
) -> Result<HashMap<String, i64>, StartError> {
    let ids = plan
        .heartbeats
        .iter()
        .map(|heartbeat| heartbeat.id.to_string())
        .collect();
    Ok(history
        .latest_results(&plan.node_id, ids)
        .await
        .map_err(StartError::Results)?
        .into_iter()
        .map(
            |LatestResult {
                 check_id,
                 started_at_ms,
             }| (check_id, started_at_ms),
        )
        .collect())
}

/// How long a heartbeat may go without a result before it is missed:
/// `interval + grace`, in milliseconds.
fn allowance_ms(heartbeat: &Heartbeat) -> i64 {
    let allowance = heartbeat
        .interval
        .get()
        .saturating_add(heartbeat.grace.get());
    i64::try_from(allowance.as_millis()).unwrap_or(i64::MAX)
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
    kind: MemberKind,
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
                    kind: entry.kind,
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
        let result = new_result(&shared.node_id, &run.tracker.id, &outcome);
        if let Err(error) = record(&shared, &mut run.tracker, result).await {
            error!(
                check = %run.tracker.id,
                %error,
                "storing a check result failed; it is lost, the check keeps its state, and the \
                 next result that confirms any change it made makes that change again"
            );
        }
    }
}

/// Runs one heartbeat until the engine stops: records each ping it is
/// handed, stamped with when it arrived, and a down result whenever its
/// deadline passes first, stamped with the deadline. A result moves the
/// deadline only once it is stored: a ping's to its own time plus
/// `interval + grace`, so a heartbeat that stays silent is missed again each
/// allowance; a miss's to the later of its deadline and now, plus the
/// allowance, so one whose deadline passed while the engine was stopped is
/// missed once rather than once per allowance it slept through.
///
/// The deadline is exclusive for the ping: one arriving a millisecond before
/// it is on time, and one arriving at or after it, even if the task wakes
/// late and takes the ping before the miss, records the miss first.
///
/// A miss that cannot be stored is tried again after [`MISS_RETRY_FIRST`],
/// doubling each time up to [`MISS_RETRY_MAX`], with the deadline left where
/// it was.
async fn heartbeat_loop(
    shared: Arc<Shared>,
    mut run: HeartbeatRun,
    mut stopped: watch::Receiver<bool>,
) {
    loop {
        let wake = if let Some((at, _)) = run.retry {
            at
        } else {
            let now_ms = shared.parts.clock.now_ms();
            let wait = u64::try_from(run.deadline_ms - now_ms).unwrap_or(0);
            Instant::now() + Duration::from_millis(wait)
        };
        tokio::select! {
            biased;
            _ = stopped.changed() => return,
            request = run.pings.recv() => {
                let Some(PingRequest { ping, arrived_ms, ack }) = request else {
                    return;
                };
                let taken = take_ping(&shared, &mut run, &ping, arrived_ms).await;
                // The sender may have given up waiting; the ping stands.
                let _ = ack.send(taken);
            }
            () = tokio::time::sleep_until(wake) => {
                miss(&shared, &mut run).await;
            }
        }
    }
}

/// Records `ping`, which arrived at `arrived_ms`: first the miss, if the
/// deadline had passed by then, then the ping, never stamped before the
/// heartbeat's latest result. Resolves with the ping's stamp once both are
/// stored; if either fails nothing more is recorded and the deadline stays
/// where it was.
async fn take_ping(
    shared: &Shared,
    run: &mut HeartbeatRun,
    ping: &Ping,
    arrived_ms: i64,
) -> Result<i64, PingError> {
    if arrived_ms >= run.deadline_ms && !miss(shared, run).await {
        return Err(PingError::NotStored);
    }
    let at_ms = arrived_ms.max(run.last_ms);
    let result = ping_result(&shared.node_id, &run.tracker.id, at_ms, ping);
    if let Err(error) = record(shared, &mut run.tracker, result).await {
        error!(
            heartbeat = %run.tracker.id,
            %error,
            "storing a ping failed; it was refused, and the heartbeat keeps its state and deadline"
        );
        return Err(PingError::NotStored);
    }
    run.last_ms = at_ms;
    run.deadline_ms = at_ms.saturating_add(run.allowance_ms);
    Ok(at_ms)
}

/// Records the miss of the current deadline, stamped with the deadline.
/// Resolves with whether it was stored; if not, schedules another try.
async fn miss(shared: &Shared, run: &mut HeartbeatRun) -> bool {
    let at_ms = run.deadline_ms;
    let result = missed_result(&shared.node_id, &run.heartbeat, at_ms);
    match record(shared, &mut run.tracker, result).await {
        Ok(()) => {
            let now_ms = shared.parts.clock.now_ms();
            run.last_ms = at_ms;
            run.deadline_ms = at_ms.max(now_ms).saturating_add(run.allowance_ms);
            run.retry = None;
            true
        }
        Err(error) => {
            let wait = run.retry.map_or(MISS_RETRY_FIRST, |(_, wait)| {
                wait.saturating_mul(2).min(MISS_RETRY_MAX)
            });
            error!(
                heartbeat = %run.tracker.id,
                deadline_ms = at_ms,
                %error,
                "storing a missed deadline failed; the heartbeat keeps its state and tries again \
                 in {}s",
                wait.as_secs()
            );
            run.retry = Some((Instant::now() + wait, wait));
            false
        }
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

/// Feeds a check's or heartbeat's result to its confirmation, and stores the
/// result with the outage records for any change it confirms in one store
/// transaction. Only once that is written does it move the state, announce
/// the change and update the shared state.
///
/// If the write fails nothing is stored and nothing moves: the confirmation
/// is put back as it was, so as far as the engine is concerned the result
/// never happened, and the next result that confirms the same change makes
/// it again.
async fn record(
    shared: &Shared,
    tracker: &mut Tracker,
    result: NewCheckResult,
) -> Result<(), tocsin_store::Error> {
    let health = result.health;
    let started_at_ms = result.started_at_ms;
    let before = tracker.confirmation.clone();
    let mut pending = tracker
        .confirmation
        .observe(health, started_at_ms)
        .map(|transition| plan_change(shared, tracker, transition));
    let outage_change = pending.as_mut().and_then(|pending| pending.write.take());
    let opened = match shared
        .parts
        .history
        .record(result, subject_kind(tracker.kind), outage_change)
        .await
    {
        Ok(opened) => opened,
        Err(error) => {
            tracker.confirmation = before;
            return Err(error);
        }
    };

    let change = pending.map(|pending| {
        tracker.confirmation.commit(&pending.transition);
        let ended = tracker.outage.map(|outage| outage.id);
        tracker.outage = opened.map(|id| OpenOutage {
            id,
            alerted: pending.alert,
        });
        CheckChange {
            check_id: tracker.id.clone(),
            kind: tracker.kind,
            component_id: tracker.component.clone(),
            from: pending.transition.from,
            to: pending.transition.to,
            since_ms: pending.transition.since_ms,
            confirmed_at_ms: pending.transition.confirmed_at_ms,
            ended_outage_id: ended,
            opened_outage_id: opened,
            maintenance: pending.maintenance,
            alert: pending.alert,
        }
    });

    let now_ms = shared.parts.clock.now_ms();
    let mut state = shared.lock();
    if let Some(entry) = state.checks.get_mut(&tracker.id) {
        entry.last = Some(LastResult {
            health,
            started_at_ms,
        });
        entry.state = tracker.confirmation.state();
    }
    let Some(change) = change else {
        return Ok(());
    };
    shared.emit(EngineEvent::CheckChanged(change));
    if let Some(component) = &tracker.component
        && let Some(change) = state.refresh(component, now_ms)
    {
        shared.emit(EngineEvent::ComponentChanged(change));
    }
    Ok(())
}

/// A confirmed change, and the outage records that make it.
struct PendingChange {
    transition: Transition,
    /// Ends the open outage and opens one for the new state if it is
    /// degraded or down, both at the time the new state began; `None` for a
    /// first state of up, which has no outage to end or open.
    write: Option<OutageChange>,
    /// Whether alerting notifies about the change; see [`alerts`].
    alert: bool,
    /// Whether a maintenance window covers the check.
    maintenance: bool,
}

/// The outage records for `transition`. A move between degraded and down
/// ends one outage and opens another, because an outage's health never
/// changes.
fn plan_change(shared: &Shared, tracker: &Tracker, transition: Transition) -> PendingChange {
    let maintenance = {
        let now_ms = shared.parts.clock.now_ms();
        shared
            .lock()
            .maintenance(tracker.component.as_deref(), now_ms)
    };
    let alert = alerts(&transition, tracker.outage, maintenance);
    let change = OutageChange {
        end: tracker.outage.map(|outage| OutageEnd {
            id: outage.id,
            ended_at_ms: transition.since_ms,
        }),
        open: (transition.to != Health::Up).then(|| NewOutage {
            node_id: shared.node_id.clone(),
            subject_kind: subject_kind(tracker.kind),
            check_id: tracker.id.clone(),
            component_id: tracker.component.clone(),
            health: transition.to,
            started_at_ms: transition.since_ms,
            alerted: alert,
        }),
    };
    PendingChange {
        transition,
        write: (change.end.is_some() || change.open.is_some()).then_some(change),
        alert,
        maintenance,
    }
}

/// How the store files a member's results.
fn subject_kind(kind: MemberKind) -> SubjectKind {
    match kind {
        MemberKind::Check => SubjectKind::Check,
        MemberKind::Heartbeat => SubjectKind::Heartbeat,
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

/// The stored form of a ping: its health, its message as the detail, and
/// for a ping reporting down, the message (or a stock one) as the error.
fn ping_result(node_id: &str, heartbeat_id: &str, at_ms: i64, ping: &Ping) -> NewCheckResult {
    NewCheckResult {
        node_id: node_id.to_owned(),
        check_id: heartbeat_id.to_owned(),
        started_at_ms: at_ms,
        duration_ms: 0,
        health: ping.health,
        detail: ping
            .message
            .as_ref()
            .map(|message| serde_json::json!({ "message": message })),
        error: (ping.health != Health::Up).then(|| {
            ping.message
                .clone()
                .unwrap_or_else(|| "the job reported itself down".to_owned())
        }),
        status_code: None,
    }
}

/// The stored form of a missed deadline: down, saying how long it waited.
fn missed_result(node_id: &str, heartbeat: &Heartbeat, at_ms: i64) -> NewCheckResult {
    NewCheckResult {
        node_id: node_id.to_owned(),
        check_id: heartbeat.id.to_string(),
        started_at_ms: at_ms,
        duration_ms: 0,
        health: Health::Down,
        detail: None,
        error: Some(format!(
            "no ping within the interval of {} plus {} grace",
            heartbeat.interval, heartbeat.grace
        )),
        status_code: None,
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
