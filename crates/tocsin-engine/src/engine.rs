// SPDX-License-Identifier: AGPL-3.0-only

//! The running engine: one task per check, one per heartbeat, one task
//! watching incidents and maintenance windows, and the shared state they
//! update. [`Engine::reconcile`] moves a running engine to a new plan.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, Weak};
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
use tracing::{error, info};

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

/// The engine could not start, or could not move to a new plan.
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
    /// Replaced in place as [`Engine::reconcile`] adds and removes
    /// heartbeats, so every clone sees the running set.
    senders: Arc<RwLock<HashMap<String, mpsc::Sender<PingRequest>>>>,
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
            .field("heartbeats", &self.read().keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl HeartbeatHandle {
    /// Whether a heartbeat has id `id`.
    #[must_use]
    pub fn knows(&self, id: &str) -> bool {
        self.read().contains_key(id)
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, mpsc::Sender<PingRequest>>> {
        // Nothing done under the lock panics; carry on as `Shared::lock` does.
        self.senders.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, mpsc::Sender<PingRequest>>> {
        self.senders.write().unwrap_or_else(PoisonError::into_inner)
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
        let sender = self.read().get(id).cloned().ok_or(PingError::Unknown)?;
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
    /// Stops the notices task.
    stop: watch::Sender<bool>,
    notices: JoinHandle<()>,
    /// Every running check and heartbeat, by id.
    members: BTreeMap<String, Member>,
}

impl Engine {
    /// Starts running `plan`.
    ///
    /// Before any check runs it seeds every check's and heartbeat's state
    /// from what this node recorded before a restart (see `seed`), resumes
    /// each heartbeat's deadline from its latest stored result (see
    /// `heartbeat_anchors`) and reads the current incidents and
    /// maintenance windows, so the first statuses are right. Seeding emits
    /// nothing. Outages left open for checks and heartbeats `plan` no
    /// longer has are withdrawn, as [`Engine::reconcile`] does.
    ///
    /// # Errors
    ///
    /// A read fails.
    pub async fn start(plan: Plan, parts: Parts) -> Result<Engine, StartError> {
        let notices = parts.notices.current().await.map_err(StartError::Notices)?;
        let heartbeats = HeartbeatHandle {
            clock: Arc::clone(&parts.clock),
            senders: Arc::default(),
        };
        let shared = Arc::new(Shared {
            node_id: plan.node_id.clone(),
            limit: Semaphore::new(plan.max_concurrent),
            state: Mutex::new(State {
                checks: BTreeMap::new(),
                components: BTreeMap::new(),
                notices,
            }),
            notices_changed: Arc::new(Notify::new()),
            parts,
        });
        let mut members = BTreeMap::new();
        reconcile(&shared, &heartbeats, &mut members, &plan).await?;
        let (stop, stopped) = watch::channel(false);
        let notices = tokio::spawn(notices_loop(Arc::clone(&shared), stopped));
        Ok(Engine {
            shared,
            heartbeats,
            stop,
            notices,
            members,
        })
    }

    /// Moves the running engine to `plan`, keyed by id:
    ///
    /// - A check or heartbeat `plan` adds is seeded from history as
    ///   [`Engine::start`] seeds it, then started.
    /// - One `plan` changes is stopped and started again with the state its
    ///   task held: its confirmed state, its open outage and, for a
    ///   heartbeat, its queued pings and the time of its latest result, from
    ///   which its deadline is measured again with the new interval and
    ///   grace. A new `confirm` keeps the confirmed state but starts the
    ///   count towards the next change over. A check that becomes a
    ///   heartbeat, or the reverse, is removed and added.
    /// - One `plan` removes is stopped, and its open outage, if any, is
    ///   withdrawn: ended now, its alerts ended without a resolve. Its
    ///   results and past outages stay in history.
    /// - Unchanged ones keep running untouched.
    ///
    /// Components are rebuilt from `plan`, and a status that moves because
    /// of it is emitted; a new component's first status is not. The node id
    /// and the concurrency limit are fixed when the engine starts, and
    /// `plan`'s are ignored.
    ///
    /// Every read happens before anything is stopped, so an error leaves
    /// the engine running its old plan. A task found to have panicked is
    /// logged and left out; the next reconcile adds it again.
    ///
    /// # Errors
    ///
    /// Seeding an added check or heartbeat fails.
    pub async fn reconcile(&mut self, plan: &Plan) -> Result<(), StartError> {
        reconcile(&self.shared, &self.heartbeats, &mut self.members, plan).await
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
    /// [`PingError::Stopped`]. It follows [`Engine::reconcile`]: a heartbeat
    /// added later accepts pings through it, and one removed refuses them
    /// as unknown.
    #[must_use]
    pub fn heartbeats(&self) -> HeartbeatHandle {
        self.heartbeats.clone()
    }

    /// Stops every task and waits for them. A check whose probe is in
    /// flight abandons it; one already recording its result finishes
    /// recording first.
    pub async fn shutdown(self) {
        // Every task holds a receiver until it ends, so these only fail once
        // they have ended already.
        let _ = self.stop.send(true);
        for member in self.members.values() {
            let _ = member.stop.send(true);
        }
        for (name, member) in self.members {
            if let Err(join) = member.task.await {
                error!(task = %name, error = %join, "an engine task ended abnormally");
            }
        }
        if let Err(join) = self.notices.await {
            error!(task = "notices", error = %join, "an engine task ended abnormally");
        }
    }
}

/// A running check or heartbeat: what it runs, and its task, which hands its
/// state back when stopped.
struct Member {
    spec: Spec,
    stop: watch::Sender<bool>,
    task: JoinHandle<Run>,
}

impl fmt::Debug for Member {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Member")
            .field("id", &self.spec.id())
            .finish_non_exhaustive()
    }
}

/// A check or heartbeat as configured.
#[derive(Clone)]
enum Spec {
    Check(Arc<CompiledCheck>),
    Heartbeat(Heartbeat),
}

impl Spec {
    fn all(plan: &Plan) -> Vec<Spec> {
        plan.checks
            .iter()
            .map(|compiled| Spec::Check(Arc::clone(compiled)))
            .chain(plan.heartbeats.iter().cloned().map(Spec::Heartbeat))
            .collect()
    }

    fn id(&self) -> String {
        match self {
            Spec::Check(compiled) => compiled.check().id.to_string(),
            Spec::Heartbeat(heartbeat) => heartbeat.id.to_string(),
        }
    }

    fn kind(&self) -> MemberKind {
        match self {
            Spec::Check(_) => MemberKind::Check,
            Spec::Heartbeat(_) => MemberKind::Heartbeat,
        }
    }

    fn component(&self) -> Option<String> {
        match self {
            Spec::Check(compiled) => compiled.check().component.as_ref(),
            Spec::Heartbeat(heartbeat) => heartbeat.component.as_ref(),
        }
        .map(ToString::to_string)
    }

    /// How many agreeing results confirm a change.
    fn confirm(&self) -> u32 {
        match self {
            Spec::Check(compiled) => compiled.check().confirm,
            Spec::Heartbeat(_) => HEARTBEAT_CONFIRM,
        }
    }

    /// How long after its latest `up` result it is still up; see `seed`.
    fn up_window_ms(&self) -> i64 {
        match self {
            Spec::Check(compiled) => up_window_ms(compiled.check()),
            Spec::Heartbeat(heartbeat) => allowance_ms(heartbeat),
        }
    }

    /// Whether `other` is configured exactly as this one is.
    fn same(&self, other: &Spec) -> bool {
        match (self, other) {
            (Spec::Check(this), Spec::Check(other)) => this.check() == other.check(),
            (Spec::Heartbeat(this), Spec::Heartbeat(other)) => this == other,
            _ => false,
        }
    }
}

/// A member's private state, which its task owns while it runs.
enum Run {
    Check(CheckRun),
    Heartbeat(HeartbeatRun),
}

impl Run {
    fn tracker(&self) -> &Tracker {
        match self {
            Run::Check(run) => &run.tracker,
            Run::Heartbeat(run) => &run.tracker,
        }
    }

    /// The same member's state, carried over to `spec`, a changed
    /// configuration of the same kind.
    fn carry_over(self, spec: &Spec) -> Run {
        match (self, spec) {
            (Run::Check(mut run), Spec::Check(compiled)) => {
                let confirm = compiled.check().confirm;
                if confirm != run.compiled.check().confirm {
                    run.tracker.confirmation = match run.tracker.confirmation.state() {
                        Some(state) => Confirmation::resume(confirm, state.health, state.since_ms),
                        None => Confirmation::new(confirm),
                    };
                }
                run.tracker.component = spec.component();
                run.compiled = Arc::clone(compiled);
                Run::Check(run)
            }
            (Run::Heartbeat(mut run), Spec::Heartbeat(heartbeat)) => {
                // The deadline was measured from the latest result (or, after
                // a miss the engine slept through, from when it woke); measure
                // the new allowance from the same point. A miss waiting to be
                // stored again is due again only if the new deadline passed.
                let anchor_ms = run.deadline_ms.saturating_sub(run.allowance_ms);
                run.allowance_ms = allowance_ms(heartbeat);
                run.deadline_ms = anchor_ms.saturating_add(run.allowance_ms);
                run.retry = None;
                run.tracker.component = spec.component();
                run.heartbeat = heartbeat.clone();
                Run::Heartbeat(run)
            }
            (run, _) => run,
        }
    }
}

/// Moves `members` to `plan`; see [`Engine::reconcile`].
async fn reconcile(
    shared: &Arc<Shared>,
    heartbeats: &HeartbeatHandle,
    members: &mut BTreeMap<String, Member>,
    plan: &Plan,
) -> Result<(), StartError> {
    let specs = Spec::all(plan);
    let mut added = Vec::new();
    let mut changed = Vec::new();
    for spec in &specs {
        match members.get(&spec.id()) {
            Some(member) if member.spec.same(spec) => {}
            Some(member) if member.spec.kind() == spec.kind() => changed.push(spec.clone()),
            _ => added.push(spec.clone()),
        }
    }
    let wanted: HashMap<String, MemberKind> =
        specs.iter().map(|spec| (spec.id(), spec.kind())).collect();
    let removed: Vec<String> = members
        .iter()
        .filter(|(id, member)| wanted.get(*id) != Some(&member.spec.kind()))
        .map(|(id, _)| id.clone())
        .collect();

    // Every read, before anything stops.
    let now_ms = shared.parts.clock.now_ms();
    let history = shared.parts.history.as_ref();
    let mut seeds = seed(history, &shared.node_id, &added, now_ms).await?;
    let anchors = heartbeat_anchors(history, &shared.node_id, &added).await?;

    // New pings for a removed heartbeat are refused as unknown from here;
    // a changed one's keep queueing for its next task.
    {
        let mut senders = heartbeats.write();
        for id in &removed {
            senders.remove(id);
        }
    }
    let changed_ids: Vec<String> = changed.iter().map(Spec::id).collect();
    let mut handed_back = stop(members, removed.iter().chain(&changed_ids)).await;

    let running: HashSet<(SubjectKind, String)> = specs
        .iter()
        .map(|spec| (subject_kind(spec.kind()), spec.id()))
        .collect();
    withdraw_orphans(shared, &running).await;

    let start_ms = shared.parts.clock.now_ms();
    let mut runs = Vec::with_capacity(added.len() + changed.len());
    let mut panicked = Vec::new();
    for spec in &changed {
        match handed_back.remove(&spec.id()) {
            Some(run) => runs.push((spec.clone(), run.carry_over(spec))),
            None => panicked.push(spec.id()),
        }
    }
    let mut new_senders = Vec::new();
    for spec in &added {
        let seed = seeds
            .remove(&spec.id())
            .unwrap_or_else(|| Seed::unknown(spec.confirm()));
        let (run, sender) = new_run(spec, seed, &anchors, start_ms);
        if let Some(sender) = sender {
            new_senders.push((spec.id(), sender));
        }
        runs.push((spec.clone(), run));
    }

    let gone: Vec<&String> = removed.iter().chain(&panicked).collect();
    publish(
        shared,
        &gone,
        runs.iter().map(|(_, run)| run.tracker()),
        plan,
    );
    heartbeats.write().extend(new_senders);
    let start = Instant::now();
    for (spec, run) in runs {
        let (stop, stopped) = watch::channel(false);
        let shared = Arc::clone(shared);
        let task =
            match run {
                Run::Check(run) => tokio::spawn(async move {
                    Run::Check(check_loop(shared, run, start, stopped).await)
                }),
                Run::Heartbeat(run) => tokio::spawn(async move {
                    Run::Heartbeat(heartbeat_loop(shared, run, stopped).await)
                }),
            };
        members.insert(spec.id(), Member { spec, stop, task });
    }
    Ok(())
}

/// Stops the members with `ids` and waits for them, returning the state
/// each handed back. A task that panicked is logged and left out.
async fn stop<'a>(
    members: &mut BTreeMap<String, Member>,
    ids: impl Iterator<Item = &'a String>,
) -> HashMap<String, Run> {
    let stopping: Vec<(String, Member)> = ids.filter_map(|id| members.remove_entry(id)).collect();
    for (_, member) in &stopping {
        // The task holds the receiver until it ends.
        let _ = member.stop.send(true);
    }
    let mut handed_back = HashMap::with_capacity(stopping.len());
    for (id, member) in stopping {
        match member.task.await {
            Ok(run) => {
                handed_back.insert(id, run);
            }
            Err(join) => error!(
                task = %id,
                error = %join,
                "an engine task ended abnormally; it is left out until the next reload or restart"
            ),
        }
    }
    handed_back
}

/// Shows the reconciled members in the shared state: drops those `gone`,
/// enters each of `trackers` (a changed one keeping its latest raw result),
/// rebuilds the components from `plan` and emits the statuses that moved.
fn publish<'a>(
    shared: &Shared,
    gone: &[&String],
    trackers: impl Iterator<Item = &'a Tracker>,
    plan: &Plan,
) {
    let mut state = shared.lock();
    for id in gone {
        state.checks.remove(*id);
    }
    for tracker in trackers {
        let last = state.checks.get(&tracker.id).and_then(|entry| entry.last);
        state.checks.insert(
            tracker.id.clone(),
            CheckEntry {
                kind: tracker.kind,
                component: tracker.component.clone(),
                state: tracker.confirmation.state(),
                last,
            },
        );
    }
    for change in state.rebuild_components(&plan.components, shared.parts.clock.now_ms()) {
        shared.emit(EngineEvent::ComponentChanged(change));
    }
}

/// Withdraws every outage this node has open for a check or heartbeat not
/// in `running`, so it neither stays open forever nor ends with a resolve
/// claiming a recovery nobody saw. A failure is logged, and the outage is
/// tried again at the next reconcile or start.
async fn withdraw_orphans(shared: &Shared, running: &HashSet<(SubjectKind, String)>) {
    let history = shared.parts.history.as_ref();
    let open = match history.open_outages(&shared.node_id).await {
        Ok(open) => open,
        Err(error) => {
            error!(
                %error,
                "reading open outages failed, so those of removed checks and heartbeats stay \
                 open until the next reload or restart"
            );
            return;
        }
    };
    let now_ms = shared.parts.clock.now_ms();
    for outage in open
        .into_iter()
        .filter(|outage| !running.contains(&(outage.subject_kind, outage.check_id.clone())))
    {
        match history.withdraw(outage.id, now_ms).await {
            Ok(()) => info!(
                kind = %outage.subject_kind,
                check = %outage.check_id,
                outage = outage.id,
                "withdrew the open outage of a check or heartbeat that is no longer configured; \
                 its alerts end without a resolve"
            ),
            Err(error) => error!(
                kind = %outage.subject_kind,
                check = %outage.check_id,
                outage = outage.id,
                %error,
                "withdrawing the open outage of a check or heartbeat that is no longer \
                 configured failed; it stays open until the next reload or restart"
            ),
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

/// A new member's run, from its seed and, for a heartbeat, its anchor (see
/// `heartbeat_anchors`), with the sender a heartbeat's pings arrive on.
fn new_run(
    spec: &Spec,
    seed: Seed,
    anchors: &HashMap<String, i64>,
    start_ms: i64,
) -> (Run, Option<mpsc::Sender<PingRequest>>) {
    let Seed {
        confirmation,
        outage,
    } = seed;
    let tracker = Tracker {
        id: spec.id(),
        kind: spec.kind(),
        component: spec.component(),
        confirmation,
        outage,
    };
    match spec {
        Spec::Check(compiled) => (
            Run::Check(CheckRun {
                compiled: Arc::clone(compiled),
                tracker,
            }),
            None,
        ),
        Spec::Heartbeat(heartbeat) => {
            let (sender, pings) = mpsc::channel(PING_QUEUE);
            let allowance_ms = allowance_ms(heartbeat);
            // Measured from the latest stored result, so a restart does not
            // reset the clock; a heartbeat that never reported is measured
            // from now.
            let anchor = anchors.get(&tracker.id).copied();
            let anchor_ms = anchor.unwrap_or(start_ms);
            let run = HeartbeatRun {
                heartbeat: heartbeat.clone(),
                tracker,
                allowance_ms,
                deadline_ms: anchor_ms.saturating_add(allowance_ms),
                last_ms: anchor.unwrap_or(i64::MIN),
                retry: None,
                pings,
            };
            (Run::Heartbeat(run), Some(sender))
        }
    }
}

/// Seeds every one of `specs` from what `history` holds for `node_id`, so
/// a restart (or a reload) neither forgets a state nor repeats a change:
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
/// An outage resumes only for the kind it was opened for. Those left over
/// are withdrawn by `withdraw_orphans` once nothing else can claim them.
async fn seed(
    history: &dyn History,
    node_id: &str,
    specs: &[Spec],
    now_ms: i64,
) -> Result<HashMap<String, Seed>, StartError> {
    if specs.is_empty() {
        return Ok(HashMap::new());
    }
    let open = history
        .open_outages(node_id)
        .await
        .map_err(StartError::Outages)?;
    // An outage resumes only the kind it was opened for: a check that
    // replaced a heartbeat of the same id does not inherit its outage.
    let mut open: HashMap<(SubjectKind, String), Outage> = open
        .into_iter()
        .map(|outage| ((outage.subject_kind, outage.check_id.clone()), outage))
        .collect();
    let without_outage = specs
        .iter()
        .filter(|spec| !open.contains_key(&(subject_kind(spec.kind()), spec.id())))
        .map(Spec::id)
        .collect();
    let last_up: HashMap<String, LastUp> = history
        .last_up(node_id, without_outage)
        .await
        .map_err(StartError::Results)?
        .into_iter()
        .map(|last| (last.check_id.clone(), last))
        .collect();

    let mut seeds = HashMap::with_capacity(specs.len());
    for spec in specs {
        let id = spec.id();
        let confirm = spec.confirm();
        let seed = if let Some(outage) = open.remove(&(subject_kind(spec.kind()), id.clone())) {
            Seed {
                confirmation: Confirmation::resume(confirm, outage.health, outage.started_at_ms),
                outage: Some(OpenOutage {
                    id: outage.id,
                    alerted: outage.alerted,
                }),
            }
        } else if let Some(last) = last_up
            .get(&id)
            .filter(|last| now_ms.saturating_sub(last.last_up_ms) <= spec.up_window_ms())
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
    Ok(seeds)
}

/// When each heartbeat among `specs` last recorded a result on `node_id`,
/// ping or miss (unix milliseconds). Its deadline resumes from there.
async fn heartbeat_anchors(
    history: &dyn History,
    node_id: &str,
    specs: &[Spec],
) -> Result<HashMap<String, i64>, StartError> {
    let ids: Vec<String> = specs
        .iter()
        .filter(|spec| spec.kind() == MemberKind::Heartbeat)
        .map(Spec::id)
        .collect();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    Ok(history
        .latest_results(node_id, ids)
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

    /// Rebuilds the components from `rules` and the checks' components,
    /// and returns the changes in status of those that existed before; a
    /// new component's first status is not a change.
    fn rebuild_components(
        &mut self,
        rules: &[(String, ComponentRule)],
        now_ms: i64,
    ) -> Vec<ComponentChange> {
        let mut components = BTreeMap::new();
        let mut new = HashSet::new();
        for (id, rule) in rules {
            let members = self
                .checks
                .iter()
                .filter(|(_, entry)| entry.component.as_deref() == Some(id))
                .map(|(check, _)| check.clone())
                .collect();
            let (health, status) = if let Some(entry) = self.components.get(id) {
                (entry.health, entry.status)
            } else {
                new.insert(id.clone());
                (None, ComponentStatus::Unknown)
            };
            components.insert(
                id.clone(),
                ComponentEntry {
                    rule: *rule,
                    checks: members,
                    health,
                    status,
                },
            );
        }
        self.components = components;
        self.refresh_all(now_ms)
            .into_iter()
            .filter(|change| !new.contains(&change.component_id))
            .collect()
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

/// Runs one check until it is stopped, then hands its state back: first at
/// the start plus its jitter, then every interval. A run that cannot start on time because every slot is taken
/// starts late, and the ticks it overran are skipped rather than run in a
/// burst.
async fn check_loop(
    shared: Arc<Shared>,
    mut run: CheckRun,
    start: Instant,
    mut stopped: watch::Receiver<bool>,
) -> CheckRun {
    let compiled = Arc::clone(&run.compiled);
    let check = compiled.check();
    let interval = check.interval.get();
    let mut ticks = tokio::time::interval_at(start + jitter(check.id.as_str(), interval), interval);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = ticks.tick() => {}
            _ = stopped.changed() => return run,
        }
        let outcome = tokio::select! {
            outcome = probe(&shared, &compiled) => outcome,
            _ = stopped.changed() => return run,
        };
        let Some(outcome) = outcome else {
            return run;
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

/// Runs one heartbeat until it is stopped, then hands its state back, queued
/// pings included: records each ping it is
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
) -> HeartbeatRun {
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
            _ = stopped.changed() => return run,
            request = run.pings.recv() => {
                let Some(PingRequest { ping, arrived_ms, ack }) = request else {
                    return run;
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
/// it again. A check's result that confirms no change is acknowledged
/// once queued, before it commits (DESIGN.md, "Durability"), so if the
/// database refuses it later the store logs and counts it, and its place in
/// the confirmation stays.
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
