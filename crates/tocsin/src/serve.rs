// SPDX-License-Identifier: AGPL-3.0-only

//! `tocsin serve`: run until told to stop.

use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tocsin_alert::{
    AlertMetrics, Alerter, AlertingHistory, HTTP_TIMEOUT, Notifiers, Router, WorkerSettings,
};
use tocsin_core::config::Config;
use tocsin_engine::ports::{Clock, History, ProbeRunner, SystemClock};
use tocsin_engine::{Engine, HeartbeatHandle, Parts, Plan, StatusHandle};
use tocsin_probe::ProbeContext;
use tocsin_store::{DB_FILE, Retention, Store};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::exit::Exit;
use crate::http::events::{EventBus, wake_on_incident_changes};
use crate::http::{self, AppState};
use crate::lock;
use crate::metrics::Metrics;
use crate::output::{Failure, Report};
use crate::reload::{ConfigStatus, LiveRouter, Reloader, Reloading};
use crate::web::identity::AdminAuth;

pub fn run(config: &Path, data: Option<&Path>) -> Result<Report, Failure> {
    let loaded = crate::config::load(config)?;
    // `--data` wins; a relative `server.data_dir` is relative to the config
    // folder, so the files mean the same thing wherever tocsin is started.
    let data: PathBuf = match (data, &loaded.config.server.data_dir) {
        (Some(data), _) => data.to_owned(),
        (None, Some(data_dir)) => config.join(data_dir),
        (None, None) => {
            return Err(Failure::new(
                Exit::InvalidInput,
                "no data folder: pass --data or set `data_dir` in `[server]`",
            ));
        }
    };
    let data = data.as_path();
    let retention_days = loaded.config.server.retention_days;
    let files = loaded.files.len();
    // `symlink_metadata` also catches a dangling symlink, which `exists()`
    // reports as absent and `create_dir_all` then fails on.
    if data.symlink_metadata().is_ok() && !data.is_dir() {
        return Err(Failure::new(
            Exit::InvalidInput,
            format!("data folder {} is not a directory", data.display()),
        ));
    }
    std::fs::create_dir_all(data).map_err(|error| {
        Failure::new(
            Exit::Runtime,
            format!("cannot create data folder {}: {error}", data.display()),
        )
    })?;
    // Held until this function returns, after the store has closed.
    let Some(_lock) = lock::try_take(data)? else {
        return Err(Failure::new(
            Exit::InvalidInput,
            format!("another tocsin serve is using {}", data.display()),
        ));
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| Failure::new(Exit::Runtime, format!("cannot start runtime: {error}")))?;
    let config_folder = config;
    let config = Arc::new(loaded.config);
    let signal_name =
        runtime.block_on(serve(config_folder, data, &config, files, retention_days))?;

    Ok(Report {
        json: json!({ "status": "stopped", "signal": signal_name }),
        text: format!("tocsin stopped ({signal_name})"),
        exit: Exit::Ok,
    })
}

/// Opens the database in `data`.
fn open_store(data: &Path) -> Result<Arc<Store>, Failure> {
    let path = data.join(DB_FILE);
    let store = Store::open(&path).map_err(|error| {
        Failure::new(
            Exit::Runtime,
            format!("cannot open database {}: {error}", path.display()),
        )
    })?;
    Ok(Arc::new(store))
}

/// Serves and runs the checks until a shutdown signal, then stops the
/// checks, drains requests and closes the store.
/// Resolves with the signal's name, or fails if the server ends first.
async fn serve(
    config_folder: &Path,
    data: &Path,
    config: &Arc<Config>,
    files: usize,
    retention_days: u32,
) -> Result<&'static str, Failure> {
    let shutdown = Shutdown::install()?;
    let hangup = install(SignalKind::hangup())?;
    // Before anything that holds the store starts, so failing here leaves
    // nothing to stop.
    let admin = AdminAuth::from_config(config)
        .map_err(|error| {
            Failure::new(
                Exit::Runtime,
                format!("cannot start the admin page: {error}"),
            )
        })?
        .map(Arc::new);
    let store = open_store(data)?;
    let listen = config.server.listen;
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|error| {
            Failure::new(Exit::Runtime, format!("cannot listen on {listen}: {error}"))
        })?;
    let address = listener.local_addr().map_err(|error| {
        Failure::new(Exit::Runtime, format!("cannot listen on {listen}: {error}"))
    })?;
    let events = EventBus::new();
    let metrics = Metrics::new();
    let (monitor, maintenance) = match start_checks(
        config,
        &store,
        &events,
        &metrics.alerting,
        Retention::days(retention_days),
    )
    .await
    {
        Ok(started) => started,
        Err(failure) => {
            close(store).await;
            return Err(failure);
        }
    };
    let (status, config_status) = watch::channel(ConfigStatus::loaded(http::now()));
    let state = AppState {
        store: Arc::clone(&store),
        config: Arc::clone(config),
        events: events.clone(),
        admin,
        live: Arc::new(monitor.status_handle()),
        heartbeats: monitor.heartbeats(),
        config_status,
    };
    let app = LiveRouter::new(http::router(state.clone()));
    let service = app.service();
    // Watching before the ready line, so a change made after it is seen.
    let reloader = Reloader::start(
        Reloading {
            folder: config_folder.to_owned(),
            data: data.to_owned(),
            variables: Arc::new(|name| std::env::var(name).ok()),
            state,
            app,
            status,
            metrics: metrics.config.clone(),
        },
        monitor,
        Some(hangup),
    );
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = spawn_server(listener, service, stopped, events);
    // Logged only once both handlers exist and the socket is bound, so a
    // signal sent after this line is always handled and a request sent
    // after it is always answered.
    tracing::info!(
        config = %config_folder.display(),
        data = %data.display(),
        listen = %address,
        files,
        checks = config.checks.len(),
        retention_days,
        "tocsin serving; SIGHUP or a change to the configuration reloads it; \
         waiting for SIGINT or SIGTERM"
    );
    let (outcome, _closed) = supervise(
        shutdown.wait(),
        stop,
        server,
        maintenance,
        Some(reloader),
        store,
    )
    .await;
    outcome
}

/// Serves `app` on `listener` until `stopped` fires, then ends the event
/// streams on `events` and finishes the requests in flight.
fn spawn_server(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    stopped: oneshot::Receiver<()>,
    events: EventBus,
) -> JoinHandle<io::Result<()>> {
    tokio::spawn(async move {
        // Peer addresses let `trusted_header` auth check where a request
        // came from.
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = stopped.await;
            // An event stream never finishes by itself; without this the
            // drain waits out every open status page.
            events.close();
        })
        .await
    })
}

/// The running checks: the engine, the task that wakes it when an
/// incident or maintenance window changes, and the alert delivery worker.
pub(crate) struct Monitor {
    engine: Engine,
    wake: JoinHandle<()>,
    alerter: Alerter,
    history: Arc<AlertingHistory>,
    /// What alerting is configured with now, to go back to when a reload
    /// cannot finish.
    notifiers: Notifiers,
    router: Arc<Router>,
    client: reqwest::Client,
    store: Arc<Store>,
}

/// Why [`Monitor::reload`] failed.
#[derive(Debug)]
pub(crate) enum ReloadError {
    /// Nothing changed: the old configuration is still running.
    Rejected(Failure),
    /// The new configuration is running, but a step after it failed.
    Unfinished(Failure),
}

impl Monitor {
    /// Starts the alert delivery worker and the engine on `store`,
    /// publishing on `events`. The engine records outages through
    /// [`AlertingHistory`], which queues each alert in the same transaction.
    pub(crate) async fn start(
        config: &Config,
        store: &Arc<Store>,
        events: &EventBus,
        metrics: &AlertMetrics,
    ) -> Result<Self, Failure> {
        Self::start_with(config, store, events, metrics, |history| history).await
    }

    /// [`Monitor::start`], with the engine's history passed through `wrap`
    /// first: the seam a test uses to make the engine's reads fail.
    async fn start_with(
        config: &Config,
        store: &Arc<Store>,
        events: &EventBus,
        metrics: &AlertMetrics,
        wrap: impl FnOnce(Arc<dyn History>) -> Arc<dyn History>,
    ) -> Result<Self, Failure> {
        let plan = Plan::from_config(config).map_err(|error| {
            Failure::new(
                Exit::Runtime,
                format!("cannot schedule the checks: {error}"),
            )
        })?;
        // Like the probes, alerting runs without system root certificates:
        // each HTTPS delivery then fails and is recorded as failed.
        let client = match crate::net::http_client(HTTP_TIMEOUT).build() {
            Ok(client) => client,
            Err(error) => {
                tracing::error!(
                    "alerting cannot verify TLS ({error}); every HTTPS notifier will fail \
                     until the system trusts a certificate authority"
                );
                crate::net::http_client(HTTP_TIMEOUT)
                    .tls_certs_only(std::iter::empty())
                    .build()
                    .map_err(|error| {
                        Failure::new(Exit::Runtime, format!("cannot set up HTTP: {error}"))
                    })?
            }
        };
        let notifiers = tocsin_alert::build_all(config, &client).map_err(|error| {
            Failure::new(Exit::Runtime, format!("cannot set up alerting: {error}"))
        })?;
        // Alerts left open on a notifier removed while tocsin was stopped
        // would otherwise stay open for good: nothing can resolve them.
        end_alerts_without_notifier(store, &notifiers).await?;
        let router = Arc::new(Router::new(config));
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let alerter = Alerter::start(
            Arc::clone(store),
            notifiers.clone(),
            Arc::clone(&router),
            Arc::clone(&clock),
            metrics.clone(),
            WorkerSettings::default(),
        );
        let history = Arc::new(AlertingHistory::new(
            Arc::clone(store),
            Arc::clone(&router),
            alerter.waker(),
            clock,
        ));
        let parts = Parts {
            runner: Arc::new(ProbeRunner::new(probe_context(config))),
            history: wrap(Arc::clone(&history) as _),
            notices: Arc::clone(store) as _,
            sink: Arc::new(events.clone()),
            clock: Arc::new(SystemClock),
        };
        // Subscribed before the engine starts, so no incident write can fall
        // between its first read and the subscription.
        let incident_changes = events.subscribe();
        let engine = match Engine::start(plan, parts).await {
            Ok(engine) => engine,
            Err(error) => {
                alerter.stop().await;
                return Err(Failure::new(
                    Exit::Runtime,
                    format!("cannot start the checks: {error}"),
                ));
            }
        };
        let wake = tokio::spawn(wake_on_incident_changes(
            incident_changes,
            engine.notices_handle(),
        ));
        Ok(Monitor {
            engine,
            wake,
            alerter,
            history,
            notifiers,
            router,
            client,
            store: Arc::clone(store),
        })
    }

    /// Runs `config` in place of the running configuration: its checks,
    /// heartbeats and components, notifiers and routes. `config.server` is
    /// not read; the engine keeps the node and concurrency it started with.
    ///
    /// Alerting changes first, so an outage the new checks open is routed
    /// by the new routes to notifiers the worker has. Alerts already open
    /// keep the notifiers they fired on; those on a notifier no longer
    /// configured are ended, since nothing can resolve them.
    ///
    /// # Errors
    ///
    /// [`ReloadError::Rejected`], with the old configuration still
    /// running, if the checks cannot be scheduled, a notifier cannot be
    /// built, or the engine cannot start the new checks.
    /// [`ReloadError::Unfinished`] if the open alerts on removed notifiers
    /// cannot be ended; the next reload or start tries again.
    pub(crate) async fn reload(&mut self, config: &Config) -> Result<(), ReloadError> {
        let plan = Plan::from_config(config).map_err(|error| {
            ReloadError::Rejected(Failure::new(
                Exit::InvalidInput,
                format!("cannot schedule the checks: {error}"),
            ))
        })?;
        let notifiers = tocsin_alert::build_all(config, &self.client).map_err(|error| {
            ReloadError::Rejected(Failure::new(
                Exit::InvalidInput,
                format!("cannot set up alerting: {error}"),
            ))
        })?;
        let router = Arc::new(Router::new(config));
        self.alerter
            .reconfigure(notifiers.clone(), Arc::clone(&router));
        self.history.set_router(Arc::clone(&router));
        if let Err(error) = self.engine.reconcile(&plan).await {
            self.alerter
                .reconfigure(self.notifiers.clone(), Arc::clone(&self.router));
            self.history.set_router(Arc::clone(&self.router));
            return Err(ReloadError::Rejected(Failure::new(
                Exit::Runtime,
                format!("cannot start the new checks: {error}"),
            )));
        }
        self.notifiers = notifiers;
        self.router = router;
        end_alerts_without_notifier(&self.store, &self.notifiers)
            .await
            .map_err(ReloadError::Unfinished)
    }

    /// Takes heartbeat pings to the engine.
    pub(crate) fn heartbeats(&self) -> HeartbeatHandle {
        self.engine.heartbeats()
    }

    /// Reads the engine's current state for the read API. A reload keeps
    /// the same engine, so the handle stays good across reloads.
    pub(crate) fn status_handle(&self) -> StatusHandle {
        self.engine.status_handle()
    }

    /// The checks and heartbeats running now.
    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> tocsin_engine::Snapshot {
        self.engine.snapshot()
    }

    /// Stops the checks, then the alert delivery worker, and waits for
    /// both, so nothing still holds the store afterwards. The engine goes
    /// first so no alert is queued after the worker has gone; a send still
    /// in flight past the worker's grace is abandoned and stays queued.
    pub(crate) async fn stop(self) {
        self.wake.abort();
        let _ = self.wake.await;
        self.engine.shutdown().await;
        self.alerter.stop().await;
    }
}

/// Ends the open alerts that fired on a notifier not in `notifiers`.
async fn end_alerts_without_notifier(store: &Store, notifiers: &Notifiers) -> Result<(), Failure> {
    let configured = notifiers.keys().cloned().collect();
    let ended = store
        .end_alerts_without_notifier(configured, tocsin_store::now_ms())
        .await
        .map_err(|error| {
            Failure::new(
                Exit::Runtime,
                format!("cannot end the alerts of removed notifiers: {error}"),
            )
        })?;
    if ended > 0 {
        tracing::warn!(
            alerts = ended,
            "ended open alerts whose notifiers are no longer configured; they will not be resolved"
        );
    }
    Ok(())
}

/// The root certificates HTTP and TLS checks verify against. Without a
/// system root store those checks cannot run: serve keeps running, each of
/// them is recorded down with a setup error, and this says so once, loudly,
/// at startup.
fn probe_context(config: &Config) -> ProbeContext {
    let context = ProbeContext::system_or_without_roots();
    let blocked: Vec<&str> = config
        .checks
        .iter()
        .filter(|check| context.cannot_run(check).is_some())
        .map(|check| check.id.as_str())
        .collect();
    if let Some(error) = config
        .checks
        .iter()
        .find_map(|check| context.cannot_run(check))
    {
        tracing::error!(
            checks = ?blocked,
            "{}; these checks cannot run and will be recorded down until root \
             certificates are installed and tocsin is restarted",
            error.message
        );
    }
    context
}

/// Waits for `shutdown` or for the server to end on its own, then stops
/// everything that holds the store and closes it: the checks first, then
/// the requests in flight. Every way out runs the same cleanup; the flag
/// says whether the store closed.
async fn supervise(
    shutdown: impl Future<Output = &'static str>,
    stop: oneshot::Sender<()>,
    mut server: JoinHandle<io::Result<()>>,
    maintenance: JoinHandle<()>,
    reloader: Option<Reloader>,
    store: Arc<Store>,
) -> (Result<&'static str, Failure>, bool) {
    let ended = tokio::select! {
        signal_name = shutdown => Ok(signal_name),
        result = &mut server => Err(result),
    };
    let outcome = match ended {
        Ok(signal_name) => {
            tracing::info!(signal = signal_name, "shutting down");
            Ok(signal_name)
        }
        Err(result) => {
            let message = match result {
                Ok(Ok(())) => "the HTTP server stopped on its own".to_owned(),
                Ok(Err(error)) => format!("the HTTP server failed: {error}"),
                Err(error) => format!("the HTTP server panicked: {error}"),
            };
            tracing::error!("{message}; shutting down");
            Err(Failure::new(Exit::Runtime, message))
        }
    };
    if let Some(reloader) = reloader {
        reloader.stop().await;
    }
    if outcome.is_ok() {
        drain(stop, server).await;
    }
    maintenance.abort();
    // An aborted task drops its handle on the store at its next await,
    // which `await` on the join handle waits for.
    let _ = maintenance.await;
    (outcome, close(store).await)
}

/// Stops accepting, then lets requests in flight finish so their writes
/// commit before the store closes.
async fn drain(stop: oneshot::Sender<()>, mut server: JoinHandle<io::Result<()>>) {
    let _ = stop.send(());
    match tokio::time::timeout(DRAIN_FOR, &mut server).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(error))) => tracing::warn!(%error, "HTTP server stopped with an error"),
        Ok(Err(error)) => tracing::warn!(%error, "HTTP server panicked while draining"),
        Err(_) => {
            // axum owns the connection tasks: aborting the server stops
            // the accept loop, not a request still running, which keeps
            // its handle on the store. `close` reports it if it outlives
            // the grace period.
            tracing::warn!(
                seconds = DRAIN_FOR.as_secs(),
                "requests still running; dropping them"
            );
            server.abort();
            let _ = server.await;
        }
    }
}

/// Closes the store once this is the last handle on it. A connection
/// task drops its handle a moment after the server sees it finish, so
/// this waits up to [`RELEASE_FOR`] before giving up. Says whether the
/// store closed; logs an error when it did not.
async fn close(store: Arc<Store>) -> bool {
    let deadline = tokio::time::Instant::now() + RELEASE_FOR;
    while Arc::strong_count(&store) > 1 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    match Arc::try_unwrap(store) {
        Ok(store) => {
            if let Err(error) = store.close().await {
                tracing::error!(%error, "database did not close cleanly");
                return false;
            }
            true
        }
        Err(store) => {
            tracing::error!(
                holders = Arc::strong_count(&store) - 1,
                "database still in use at shutdown; not closed"
            );
            false
        }
    }
}

/// How long requests in flight may run after a shutdown signal.
const DRAIN_FOR: Duration = Duration::from_secs(10);

/// How long [`close`] waits for the last other handle on the store to go.
const RELEASE_FOR: Duration = Duration::from_secs(1);

/// How often rollups are brought up to date and retention runs.
const MAINTENANCE_EVERY: Duration = Duration::from_hours(1);

/// Starts the checks, and the hourly maintenance behind them.
///
/// One maintenance pass runs first, so the engine reads history retention
/// has finished with. A heartbeat's deadline runs from its latest stored
/// result, and a missed one is stamped at that deadline. Were the result
/// due for deletion (behind a watermark a part-way pass left, or moved by a
/// pass racing the engine's first reads), the miss could fall behind the
/// watermark too: the store would refuse it on every retry, and every ping
/// after it, which records the overdue miss first.
pub(crate) async fn start_checks(
    config: &Config,
    store: &Arc<Store>,
    events: &EventBus,
    metrics: &AlertMetrics,
    retention: Retention,
) -> Result<(Monitor, JoinHandle<()>), Failure> {
    maintenance_pass(store, retention).await;
    let monitor = Monitor::start(config, store, events, metrics).await?;
    let maintenance = tokio::spawn(maintain(Arc::clone(store), retention));
    Ok((monitor, maintenance))
}

/// Hourly after the pass [`start_checks`] runs: see [`maintenance_pass`].
async fn maintain(store: Arc<Store>, retention: Retention) {
    let mut ticks = tokio::time::interval_at(
        tokio::time::Instant::now() + MAINTENANCE_EVERY,
        MAINTENANCE_EVERY,
    );
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        maintenance_pass(&store, retention).await;
    }
}

/// Rolls up the days since the last pass, then deletes results older than
/// the retention window. Failures are logged and retried on the next pass;
/// a pass never stops the monitor.
async fn maintenance_pass(store: &Store, retention: Retention) {
    match store.rollup_recent().await {
        Ok(report) => tracing::debug!(rows = report.rows, "daily rollups updated"),
        Err(error) => tracing::error!(%error, "daily rollup failed"),
    }
    match store
        .apply_retention(retention, tocsin_store::now_ms())
        .await
    {
        Ok(report) if report.deleted > 0 => tracing::info!(
            deleted = report.deleted,
            chunks = report.chunks,
            rolled_up = report.rolled_up,
            "retention purged old check results"
        ),
        Ok(_) => {}
        Err(error) => tracing::error!(%error, "retention failed"),
    }
}

/// Handlers for the signals that stop `tocsin serve`.
struct Shutdown {
    interrupt: Signal,
    terminate: Signal,
}

/// A handler for `kind`, which then no longer has its default effect.
fn install(kind: SignalKind) -> Result<Signal, Failure> {
    signal(kind).map_err(|error| {
        Failure::new(
            Exit::Runtime,
            format!("cannot install signal handler: {error}"),
        )
    })
}

impl Shutdown {
    fn install() -> Result<Self, Failure> {
        Ok(Shutdown {
            interrupt: install(SignalKind::interrupt())?,
            terminate: install(SignalKind::terminate())?,
        })
    }

    /// Resolves with the name of the first shutdown signal received.
    async fn wait(mut self) -> &'static str {
        tokio::select! {
            _ = self.interrupt.recv() => "SIGINT",
            _ = self.terminate.recv() => "SIGTERM",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::{pending, ready};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, Weak};
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use serde_json::json;
    use tocsin_core::Health;
    use tocsin_core::config::{Environment, Source};
    use tocsin_core::state::ComponentStatus;
    use tocsin_engine::Ping;
    use tocsin_engine::ports::{BoxFuture, History};
    use tocsin_store::{
        DB_FILE, LastUp, LatestResult, NewCheckResult, Outage, OutageChange, Retention, Store,
        SubjectKind,
    };
    use tokio::sync::oneshot;
    use tower::ServiceExt;

    use super::{
        AlertMetrics, Monitor, ReloadError, close, io, spawn_server, start_checks, supervise,
    };
    use crate::exit::Exit;
    use crate::http::events::{Event, EventBus};
    use crate::http::{self, AppState};

    fn open() -> (tempfile::TempDir, Arc<Store>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join(DB_FILE)).unwrap();
        (dir, Arc::new(store))
    }

    /// A task that holds a handle on the store until it is aborted.
    fn holding(store: &Arc<Store>) -> tokio::task::JoinHandle<()> {
        let store = Arc::clone(store);
        tokio::spawn(async move {
            let _store = store;
            pending::<()>().await;
        })
    }

    #[tokio::test]
    async fn a_failed_server_still_stops_maintenance_and_closes_the_store() {
        let (_dir, store) = open();
        let weak: Weak<Store> = Arc::downgrade(&store);
        let held = Arc::clone(&store);
        let server = tokio::spawn(async move {
            let _store = held;
            Err(io::Error::other("accept failed"))
        });
        let maintenance = holding(&store);
        let (stop, _stopped) = oneshot::channel();

        let (outcome, closed) = supervise(pending(), stop, server, maintenance, None, store).await;

        let failure = outcome.unwrap_err();
        assert_eq!(failure.exit, Exit::Runtime);
        assert_eq!(failure.message, "the HTTP server failed: accept failed");
        assert!(closed, "the store was not closed");
        assert!(weak.upgrade().is_none(), "something still holds the store");
    }

    #[tokio::test]
    async fn a_signal_drains_the_server_and_closes_the_store() {
        let (_dir, store) = open();
        let weak = Arc::downgrade(&store);
        let held = Arc::clone(&store);
        let (stop, stopped) = oneshot::channel();
        let server = tokio::spawn(async move {
            let _store = held;
            let _ = stopped.await;
            Ok(())
        });
        let maintenance = holding(&store);

        let (outcome, closed) =
            supervise(ready("SIGTERM"), stop, server, maintenance, None, store).await;

        assert_eq!(outcome.unwrap(), "SIGTERM");
        assert!(closed, "the store was not closed");
        assert!(weak.upgrade().is_none(), "something still holds the store");
    }

    /// A status page holds `/api/v1/events` open for as long as it is
    /// shown. Shutting down ends the stream, so the drain finishes at once
    /// rather than waiting out its 10 seconds and dropping the connection.
    #[tokio::test]
    async fn a_signal_ends_open_event_streams_so_the_drain_is_quick() {
        let (_dir, store) = open();
        let weak = Arc::downgrade(&store);
        let config = tocsin_core::config::load(
            &[Source {
                name: "tocsin.toml".into(),
                text: Ok("[[component]]\nid = \"api\"\nname = \"API\"\n".into()),
            }],
            &Environment {
                hostname: "test".into(),
                variable: &|_| None,
            },
        )
        .unwrap();
        let events = EventBus::new();
        let app = http::router(AppState {
            store: Arc::clone(&store),
            config: Arc::new(config),
            events: events.clone(),
            admin: None,
            live: Arc::new(tocsin_engine::Snapshot::default()),
            heartbeats: tocsin_engine::HeartbeatHandle::default(),
            config_status: crate::reload::steady_status(),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel();
        let server = spawn_server(listener, app, stopped, events);
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut stream = reqwest::Client::new()
            .get(format!("http://{address}/api/v1/events"))
            .send()
            .await
            .unwrap();
        // The opening `resync` is on the wire: the stream is open.
        let first = stream.chunk().await.unwrap().unwrap();
        assert!(String::from_utf8_lossy(&first).contains("event: resync"));
        let maintenance = holding(&store);

        let started = std::time::Instant::now();
        let (outcome, closed) =
            supervise(ready("SIGTERM"), stop, server, maintenance, None, store).await;

        assert_eq!(outcome.unwrap(), "SIGTERM");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the drain waited {:?} on an open event stream",
            started.elapsed()
        );
        assert!(closed, "the store was not closed");
        assert!(weak.upgrade().is_none(), "something still holds the store");
        // The client sees the stream end, not a reset.
        assert!(stream.chunk().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_store_still_held_at_shutdown_is_reported_not_closed() {
        let (_dir, store) = open();
        let straggler = Arc::clone(&store);
        let (stop, stopped) = oneshot::channel();
        let server = tokio::spawn(async move {
            let _ = stopped.await;
            Ok(())
        });
        let maintenance = holding(&store);

        let (outcome, closed) =
            supervise(ready("SIGINT"), stop, server, maintenance, None, store).await;

        assert_eq!(outcome.unwrap(), "SIGINT");
        assert!(!closed, "closed a store something else still holds");
        assert_eq!(Arc::strong_count(&straggler), 1);
    }

    /// The engine re-reads incidents on its own only once a minute; a write
    /// through the API must move the component's status straight away.
    #[tokio::test]
    async fn an_incident_written_through_the_api_moves_a_component_at_once() {
        const SECRET: &str = "oncall-secret-0123456789";
        let (_dir, store) = open();
        let toml = "[[component]]\nid = \"api\"\nname = \"API\"\n\n\
                    [[token]]\nid = \"oncall\"\nsecret = \"${ENV:ONCALL_TOKEN}\"\n\
                    scopes = [\"incident\"]\n";
        let config = tocsin_core::config::load(
            &[Source {
                name: "tocsin.toml".into(),
                text: Ok(toml.into()),
            }],
            &Environment {
                hostname: "test".into(),
                variable: &|name| (name == "ONCALL_TOKEN").then(|| SECRET.to_owned()),
            },
        )
        .unwrap();
        let config = Arc::new(config);
        let events = EventBus::new();
        let mut seen = events.subscribe();
        let monitor = Monitor::start(&config, &store, &events, &AlertMetrics::default())
            .await
            .unwrap();
        let app = http::router(AppState {
            store: Arc::clone(&store),
            config,
            events,
            admin: None,
            live: Arc::new(monitor.engine.status_handle()),
            heartbeats: monitor.heartbeats(),
            config_status: crate::reload::steady_status(),
        });

        let body = json!({
            "title": "API errors",
            "impact": "major",
            "components": ["api"],
            "message": "Looking into it."
        });
        let request = Request::post("/api/v1/incidents")
            .header(header::AUTHORIZATION, format!("Bearer {SECRET}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);

        let change = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Event::ComponentChanged(change) = seen.recv().await.unwrap() {
                    return change;
                }
            }
        })
        .await
        .expect("the write woke the engine");
        assert_eq!(change.component_id, "api");
        // A component with no checks has no health of its own, so it starts
        // unknown and the incident alone sets it.
        assert_eq!(
            (change.from, change.to),
            (ComponentStatus::Unknown, ComponentStatus::Down)
        );

        monitor.stop().await;
        assert!(close(store).await, "the stopped engine released the store");
    }

    /// A pass that moved the retention watermark and stopped before its
    /// delete leaves a heartbeat's last result behind the watermark. The
    /// checks start only once retention has finished, so the heartbeat
    /// starts fresh rather than owing a miss the store must refuse, which
    /// would refuse every ping after it.
    #[tokio::test]
    async fn retention_finishes_before_heartbeats_read_their_deadlines() {
        const DAY: i64 = 86_400_000;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DB_FILE);
        let toml = "[[heartbeat]]\nid = \"backup\"\ninterval = \"1h\"\n";
        let config = tocsin_core::config::load(
            &[Source {
                name: "tocsin.toml".into(),
                text: Ok(toml.into()),
            }],
            &Environment {
                hostname: "test".into(),
                variable: &|_| None,
            },
        )
        .unwrap();
        let node = config.server.node_id.clone().unwrap();
        let today = tocsin_store::now_ms() / DAY * DAY;
        let store = Arc::new(Store::open(&path).unwrap());
        store
            .record(
                NewCheckResult {
                    node_id: node,
                    check_id: "backup".to_owned(),
                    started_at_ms: today - 100 * DAY,
                    duration_ms: 0,
                    health: Health::Up,
                    detail: None,
                    error: None,
                    status_code: None,
                },
                SubjectKind::Heartbeat,
                None,
            )
            .await
            .unwrap();
        assert!(close(store).await);
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute(
                "INSERT INTO retention (id, purged_before) VALUES (1, ?1)
                 ON CONFLICT (id) DO UPDATE SET purged_before = excluded.purged_before",
                [today - 95 * DAY],
            )
            .unwrap();

        let store = Arc::new(Store::open(&path).unwrap());
        let (monitor, maintenance) = start_checks(
            &config,
            &store,
            &EventBus::new(),
            &AlertMetrics::default(),
            Retention::days(90),
        )
        .await
        .unwrap();
        let ping = Ping {
            health: Health::Up,
            message: None,
        };
        let pinged = monitor.heartbeats().ping("backup", ping).await;

        maintenance.abort();
        let _ = maintenance.await;
        monitor.stop().await;
        assert!(close(store).await);
        pinged.expect("the ping is stored");
    }

    fn parse(toml: &str) -> tocsin_core::config::Config {
        tocsin_core::config::load(
            &[Source {
                name: "tocsin.toml".into(),
                text: Ok(toml.into()),
            }],
            &Environment {
                hostname: "test".into(),
                variable: &|_| None,
            },
        )
        .unwrap()
    }

    /// An alert sent to a notifier the configuration no longer has would
    /// never be resolved or repeated: it ends, at startup and at reload,
    /// without a message, and the others keep firing.
    #[tokio::test]
    async fn the_alerts_of_a_removed_notifier_end_at_startup_and_at_reload() {
        let (_dir, store) = open();
        // Nothing listens there once the listener is dropped.
        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let check = format!(
            "[[check]]\nid = \"db\"\ntype = \"tcp\"\ntarget = \"{closed}\"\n\
             interval = \"1s\"\ntimeout = \"500ms\"\nconfirm = 1\n"
        );
        let notifier = |id: &str| {
            format!(
                "[[notifier]]\nid = \"{id}\"\ntype = \"ntfy\"\n\
                 [notifier.ntfy]\nurl = \"http://127.0.0.1:9/{id}\"\n"
            )
        };
        let route =
            |notifiers: &str| format!("[[route]]\nid = \"all\"\nnotifiers = [{notifiers}]\n");
        let open_alerts = async || {
            let mut open: Vec<String> = store
                .alerts()
                .await
                .unwrap()
                .into_iter()
                .filter(|alert| alert.ended_at_ms.is_none())
                .map(|alert| alert.notifier_id)
                .collect();
            open.sort();
            open
        };

        let both = parse(&format!(
            "{check}{}{}{}",
            notifier("kept"),
            notifier("gone"),
            route("\"kept\", \"gone\"")
        ));
        let events = EventBus::new();
        let mut seen = events.subscribe();
        let monitor = Monitor::start(&both, &store, &events, &AlertMetrics::default())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Event::CheckChanged(change) = seen.recv().await.unwrap()
                    && change.to == Health::Down
                {
                    return;
                }
            }
        })
        .await
        .expect("the check goes down");
        monitor.stop().await;
        assert_eq!(open_alerts().await, ["gone", "kept"]);

        let one = parse(&format!("{check}{}{}", notifier("kept"), route("\"kept\"")));
        let mut monitor = Monitor::start(&one, &store, &events, &AlertMetrics::default())
            .await
            .unwrap();
        assert_eq!(open_alerts().await, ["kept"], "ended at startup");

        monitor.reload(&parse(&check)).await.unwrap();
        assert!(open_alerts().await.is_empty(), "ended at reload");
        let outages = store.open_outages("test").await.unwrap();
        assert_eq!(outages.len(), 1, "the outage itself stays open");
        monitor.stop().await;
        assert!(close(store).await);
    }

    /// The engine's history, whose next `failing` reads of open outages
    /// fail.
    struct FailingReads {
        history: Arc<dyn History>,
        failing: AtomicUsize,
    }

    impl History for FailingReads {
        fn record(
            &self,
            result: NewCheckResult,
            kind: SubjectKind,
            change: Option<OutageChange>,
        ) -> BoxFuture<'_, tocsin_store::Result<Option<i64>>> {
            self.history.record(result, kind, change)
        }

        fn open_outages<'a>(
            &'a self,
            node_id: &'a str,
        ) -> BoxFuture<'a, tocsin_store::Result<Vec<Outage>>> {
            let fail = self
                .failing
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok();
            if fail {
                return Box::pin(ready(Err(tocsin_store::Error::Batch(
                    "scripted read failure".to_owned(),
                ))));
            }
            self.history.open_outages(node_id)
        }

        fn last_up<'a>(
            &'a self,
            node_id: &'a str,
            check_ids: Vec<String>,
        ) -> BoxFuture<'a, tocsin_store::Result<Vec<LastUp>>> {
            self.history.last_up(node_id, check_ids)
        }

        fn latest_results<'a>(
            &'a self,
            node_id: &'a str,
            check_ids: Vec<String>,
        ) -> BoxFuture<'a, tocsin_store::Result<Vec<LatestResult>>> {
            self.history.latest_results(node_id, check_ids)
        }

        fn withdraw(&self, outage: i64, at_ms: i64) -> BoxFuture<'_, tocsin_store::Result<()>> {
            self.history.withdraw(outage, at_ms)
        }
    }

    /// A server that takes every notifier's posts, at `/{id}`, and yields
    /// the id of each.
    async fn notifier_server() -> (
        std::net::SocketAddr,
        tokio::sync::mpsc::UnboundedReceiver<String>,
    ) {
        let (sent, received) = tokio::sync::mpsc::unbounded_channel();
        let app = axum::Router::new().route(
            "/{id}",
            axum::routing::post(
                async move |axum::extract::Path(id): axum::extract::Path<String>| {
                    let _ = sent.send(id);
                    StatusCode::OK
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        (address, received)
    }

    /// A reload the engine cannot apply puts alerting back as it was: the
    /// old routes still queue alerts, and the worker still has the
    /// notifiers to send them, rather than routing to notifiers it never
    /// started.
    #[tokio::test]
    async fn a_reload_the_engine_refuses_leaves_the_old_alerting() {
        let (_dir, store) = open();
        let (notify, mut received) = notifier_server().await;
        // The check is up while this listens, and down once it is dropped.
        let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = target.local_addr().unwrap();
        let check = |id: &str| {
            format!(
                "[[check]]\nid = \"{id}\"\ntype = \"tcp\"\ntarget = \"{address}\"\n\
                 interval = \"1s\"\ntimeout = \"500ms\"\nconfirm = 1\n"
            )
        };
        let alerting = |id: &str| {
            format!(
                "[[notifier]]\nid = \"{id}\"\ntype = \"ntfy\"\n\
                 [notifier.ntfy]\nurl = \"http://{notify}/{id}\"\n\
                 [[route]]\nid = \"all\"\nnotifiers = [\"{id}\"]\n"
            )
        };
        let old = parse(&format!("{}{}", check("db"), alerting("old")));
        let new = parse(&format!(
            "{}{}{}",
            check("db"),
            check("api"),
            alerting("new")
        ));

        let events = EventBus::new();
        let mut seen = events.subscribe();
        let history = Arc::new(Mutex::new(None::<Arc<FailingReads>>));
        let mut monitor =
            Monitor::start_with(&old, &store, &events, &AlertMetrics::default(), |inner| {
                let failing = Arc::new(FailingReads {
                    history: inner,
                    failing: AtomicUsize::new(0),
                });
                *history.lock().unwrap() = Some(Arc::clone(&failing));
                failing
            })
            .await
            .unwrap();
        let mut reached = async |health: Health| {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if let Event::CheckChanged(change) = seen.recv().await.unwrap()
                        && change.check_id == "db"
                        && change.to == health
                    {
                        return;
                    }
                }
            })
            .await
            .unwrap_or_else(|_| panic!("db never went {health:?}"));
        };
        reached(Health::Up).await;

        history
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .failing
            .store(1, Ordering::SeqCst);
        let error = monitor
            .reload(&new)
            .await
            .expect_err("the engine's read fails");
        assert!(matches!(error, ReloadError::Rejected(_)), "{error:?}");
        let running: Vec<String> = monitor
            .snapshot()
            .checks
            .iter()
            .map(|check| check.id.clone())
            .collect();
        assert_eq!(running, ["db"], "the old checks still run");

        drop(target);
        reached(Health::Down).await;
        let delivered = tokio::time::timeout(Duration::from_secs(10), received.recv())
            .await
            .expect("the old notifier is sent the alert");
        assert_eq!(delivered.as_deref(), Some("old"));
        let notifiers: Vec<String> = store
            .alerts()
            .await
            .unwrap()
            .into_iter()
            .map(|alert| alert.notifier_id)
            .collect();
        assert_eq!(notifiers, ["old"], "routed by the old routes");
        monitor.stop().await;
        assert!(
            received.try_recv().is_err(),
            "nothing reached the new notifier"
        );
        // It holds the engine's history, and through it the store.
        drop(history);
        assert!(close(store).await);
    }
}
