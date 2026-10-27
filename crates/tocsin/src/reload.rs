// SPDX-License-Identifier: AGPL-3.0-only

//! Reloading the configuration while `tocsin serve` runs.
//!
//! A change under the configuration folder, once nothing has changed for
//! [`DEBOUNCE`], or a SIGHUP reads and validates the whole folder again,
//! exactly as startup does. A configuration that validates goes in at
//! once: the engine reconciles its checks (see
//! [`Engine::reconcile`](tocsin_engine::Engine::reconcile)), alerting takes
//! the new notifiers and routes, and the HTTP service answers every new
//! request from the new configuration, tokens, components and projects
//! included. One that does not changes nothing: the last good
//! configuration keeps running, and the failure shows in the log, in the
//! `tocsin_config_reload_success` and `tocsin_config_last_error_timestamp`
//! metrics, at `GET /api/v1/config/status` and in `tocsin config status`.
//!
//! `[server]` is read only at startup: it names the socket, the database
//! and the node the history belongs to, none of which can move under a
//! running server. A reload keeps the running `[server]` and warns when the
//! files say something else. `[subscribers]` is the same: its rate limits
//! and mailer start with the server. So is `[telemetry]`: its exporter
//! starts with the server.

use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use axum::Router;
use axum::extract::Request;
use notify::event::{AccessKind, AccessMode};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use tocsin_core::config::Diagnostic;
use tocsin_core::incident::Timestamp;
use tokio::signal::unix::Signal;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tower::ServiceExt;
use utoipa::ToSchema;

use crate::exit::Exit;
use crate::http::{self, AppState};
use crate::metrics::ConfigMetrics;
use crate::output::Failure;
use crate::serve::{Monitor, ReloadError};

/// How long the configuration folder must be quiet before a change to it
/// is read: an editor's save or a deploy tool's copy is several writes.
pub const DEBOUNCE: Duration = Duration::from_millis(500);

/// Looks up the environment variables `${ENV:NAME}` secrets name.
pub type Variables = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// How the configuration last reloaded, as `GET /api/v1/config/status`
/// returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ConfigStatus {
    /// When the running configuration was read: at startup, or by the
    /// last reload that put its configuration in place.
    pub loaded_at: Timestamp,
    /// When a reload was last tried, whether or not it took; the startup
    /// read until the first.
    pub last_attempt_at: Timestamp,
    /// Whether the last attempt put its configuration in place. False
    /// means the configuration from `loaded_at` is still running.
    pub ok: bool,
    /// Why the last attempt failed, each with its file, line and column
    /// when it has one; empty when it did not fail.
    pub errors: Vec<Diagnostic>,
}

impl ConfigStatus {
    /// A configuration read at `at` and running.
    #[must_use]
    pub fn loaded(at: Timestamp) -> Self {
        ConfigStatus {
            loaded_at: at,
            last_attempt_at: at,
            ok: true,
            errors: Vec::new(),
        }
    }
}

/// The HTTP service, built from the configuration last loaded.
///
/// Each request goes to the router current when it arrives and runs on
/// its state to the end, so it never sees half of one configuration and
/// half of the next.
#[derive(Clone)]
pub struct LiveRouter(Arc<RwLock<Router>>);

impl LiveRouter {
    /// Serves `router` until [`replace`](Self::replace)d.
    #[must_use]
    pub fn new(router: Router) -> Self {
        LiveRouter(Arc::new(RwLock::new(router)))
    }

    /// The router to serve: it hands each request to the current one.
    pub fn service(&self) -> Router {
        let live = self.clone();
        Router::new().fallback_service(tower::service_fn(move |request: Request| {
            let router = live.current();
            async move { router.oneshot(request).await }
        }))
    }

    fn replace(&self, router: Router) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = router;
    }

    fn current(&self) -> Router {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Everything a reload replaces, besides the running checks.
pub struct Reloading {
    /// The configuration folder, as serve was given it.
    pub folder: PathBuf,
    /// The data folder. Writes in it are not configuration changes, unless
    /// they are to a `*.toml` file the folder's scan would read.
    pub data: PathBuf,
    /// Where `${ENV:NAME}` secrets are read from.
    pub variables: Variables,
    /// The state requests are served from; its `config` is the running
    /// configuration.
    pub state: AppState,
    /// The service built from `state`.
    pub app: LiveRouter,
    /// Where each attempt's outcome is published.
    pub status: watch::Sender<ConfigStatus>,
    /// Each attempt's outcome, as metrics.
    pub metrics: ConfigMetrics,
}

/// The task that reloads the configuration. It owns the running checks
/// and hands them back to be stopped.
pub struct Reloader {
    stop: oneshot::Sender<()>,
    task: JoinHandle<Monitor>,
}

impl Reloader {
    /// Watches `reloading.folder` and reloads into `monitor` and
    /// `reloading.app` on each change, and on each `hangup` signal when one
    /// is given. Without a watcher, which some file systems refuse, only
    /// the signal reloads; that is logged as an error.
    pub fn start(reloading: Reloading, monitor: Monitor, hangup: Option<Signal>) -> Self {
        // One pending change is enough: the reload it causes reads them all.
        let (sender, changes) = mpsc::channel(1);
        let watcher = watch(&reloading.folder, &reloading.data, sender);
        let (stop, stopped) = oneshot::channel();
        let running = Running {
            reloading,
            monitor,
            _watcher: watcher,
        };
        let task = tokio::spawn(running.run(stopped, changes, hangup));
        Reloader { stop, task }
    }

    /// Stops reloading, letting one under way finish, then stops the
    /// checks.
    pub async fn stop(self) {
        let _ = self.stop.send(());
        match self.task.await {
            Ok(monitor) => monitor.stop().await,
            Err(error) => {
                tracing::error!(%error, "the configuration reloader failed; the checks it held were dropped");
            }
        }
    }
}

#[cfg(test)]
impl Reloader {
    /// Stops reloading and hands the checks back still running.
    async fn into_monitor(self) -> Monitor {
        let _ = self.stop.send(());
        self.task.await.expect("the reloader does not panic")
    }
}

struct Running {
    reloading: Reloading,
    monitor: Monitor,
    /// Held so it keeps watching; it stops when dropped.
    _watcher: Option<RecommendedWatcher>,
}

impl Running {
    async fn run(
        mut self,
        mut stopped: oneshot::Receiver<()>,
        mut changes: mpsc::Receiver<()>,
        mut hangup: Option<Signal>,
    ) -> Monitor {
        let mut debounce = Debounce::new(DEBOUNCE);
        loop {
            tokio::select! {
                biased;
                _ = &mut stopped => break,
                Some(()) = hung_up(&mut hangup) => {
                    debounce.cancel();
                    self.reload("SIGHUP").await;
                }
                Some(()) = changes.recv() => debounce.restart(),
                () = debounce.elapsed() => self.reload("file change").await,
            }
        }
        self.monitor
    }

    /// Reads the folder and puts the configuration in place if it
    /// validates; otherwise keeps the running one and reports why.
    async fn reload(&mut self, cause: &'static str) {
        let attempt = http::now();
        let folder = self.reloading.folder.clone();
        let variables = Arc::clone(&self.reloading.variables);
        let loaded =
            tokio::task::spawn_blocking(move || crate::config::load_with(&folder, &*variables))
                .await
                .unwrap_or_else(|error| {
                    Err(Failure::new(
                        Exit::Runtime,
                        format!("reading the configuration failed: {error}"),
                    ))
                });
        let loaded = match loaded {
            Ok(loaded) => loaded,
            Err(failure) => return self.rejected(cause, attempt, &failure),
        };
        let mut config = loaded.config;
        let running = &self.reloading.state.config;
        if config.server != running.server {
            tracing::warn!(
                "`[server]` changed; it is read only at startup, so the running one is kept \
                 until tocsin restarts"
            );
        }
        config.server = running.server.clone();
        if config.subscribers != running.subscribers {
            tracing::warn!(
                "`[subscribers]` changed; it is read only at startup, so the running one is kept \
                 until tocsin restarts"
            );
        }
        config.subscribers.clone_from(&running.subscribers);
        if config.slack != running.slack {
            tracing::warn!(
                "`[slack]` changed; it is read only at startup, so the running one is kept \
                 until tocsin restarts"
            );
        }
        config.slack.clone_from(&running.slack);
        if config.telemetry != running.telemetry {
            tracing::warn!(
                "`[telemetry]` changed; it is read only at startup, so the running exporter is \
                 kept until tocsin restarts"
            );
        }
        config.telemetry.clone_from(&running.telemetry);

        let unfinished = match self.monitor.reload(&config).await {
            Ok(()) => None,
            Err(ReloadError::Rejected(failure)) => return self.rejected(cause, attempt, &failure),
            Err(ReloadError::Unfinished(failure)) => Some(failure),
        };
        let checks = config.checks.len();
        self.reloading.state.config = Arc::new(config);
        self.reloading
            .state
            .metrics
            .configured(&self.reloading.state.config);
        self.reloading
            .app
            .replace(http::router(self.reloading.state.clone()));
        let Some(failure) = unfinished else {
            tracing::info!(
                cause,
                files = loaded.files.len(),
                checks,
                "configuration reloaded"
            );
            self.reloading.metrics.succeeded();
            self.reloading
                .status
                .send_replace(ConfigStatus::loaded(attempt));
            return;
        };
        let errors = diagnostics(&failure);
        tracing::error!(
            cause,
            message = %failure.message,
            errors = %serde_json::to_string(&errors).unwrap_or_default(),
            "configuration reloaded, but a step after it failed"
        );
        self.reloading.metrics.failed(attempt);
        self.reloading.status.send_replace(ConfigStatus {
            loaded_at: attempt,
            last_attempt_at: attempt,
            ok: false,
            errors,
        });
    }

    fn rejected(&self, cause: &'static str, attempt: Timestamp, failure: &Failure) {
        let errors = diagnostics(failure);
        tracing::error!(
            cause,
            message = %failure.message,
            errors = %serde_json::to_string(&errors).unwrap_or_default(),
            "configuration reload failed; the last good configuration is still running"
        );
        self.reloading.metrics.failed(attempt);
        self.reloading.status.send_modify(|status| {
            status.last_attempt_at = attempt;
            status.ok = false;
            status.errors = errors;
        });
    }
}

/// Every problem `failure` names, or its message when it names none.
fn diagnostics(failure: &Failure) -> Vec<Diagnostic> {
    if failure.errors.is_empty() {
        vec![Diagnostic {
            file: None,
            line: None,
            column: None,
            path: None,
            message: failure.message.clone(),
            hint: None,
        }]
    } else {
        failure.errors.clone()
    }
}

/// The next SIGHUP; never, without a handler.
async fn hung_up(hangup: &mut Option<Signal>) -> Option<()> {
    match hangup {
        Some(signal) => signal.recv().await,
        None => std::future::pending().await,
    }
}

/// A deadline that moves back with each change, so a burst of changes
/// is read once, [`DEBOUNCE`] after the last.
#[derive(Debug)]
struct Debounce {
    quiet: Duration,
    due: Option<Instant>,
}

impl Debounce {
    fn new(quiet: Duration) -> Self {
        Debounce { quiet, due: None }
    }

    /// Something changed: wait until `quiet` has passed with no change.
    fn restart(&mut self) {
        self.due = Some(Instant::now() + self.quiet);
    }

    /// Nothing is waiting any more: a reload has just read the folder.
    fn cancel(&mut self) {
        self.due = None;
    }

    /// Resolves once the quiet period after the last change has passed,
    /// and then not again until the next change.
    async fn elapsed(&mut self) {
        match self.due {
            Some(due) => {
                tokio::time::sleep_until(due).await;
                self.due = None;
            }
            None => std::future::pending().await,
        }
    }
}

/// Watches `folder`, sending on `changed` for each event that could change
/// the configuration. `None`, with the error logged, if it cannot.
fn watch(folder: &Path, data: &Path, changed: mpsc::Sender<()>) -> Option<RecommendedWatcher> {
    // Event paths start with the path watched; the data folder must be
    // spelled the same way to be recognised under it.
    let folder = folder.canonicalize().unwrap_or_else(|_| folder.to_owned());
    let data = data.canonicalize().ok();
    let root = folder.clone();
    let started = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        match event {
            Ok(event) if !may_change_config(&event, &root, data.as_deref()) => return,
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%error, "watching the configuration folder failed; reloading in case");
            }
        }
        // Runs on the watcher's own thread, so it must not block: a full
        // channel already has a reload coming, and a closed one means
        // serve is stopping.
        let _ = changed.try_send(());
    })
    .and_then(|mut watcher| {
        watcher.watch(&folder, RecursiveMode::Recursive)?;
        Ok(watcher)
    });
    match started {
        Ok(watcher) => Some(watcher),
        Err(error) => {
            tracing::error!(
                %error,
                folder = %folder.display(),
                "cannot watch the configuration folder; only SIGHUP reloads it"
            );
            None
        }
    }
}

/// Whether `event`, under the configuration folder `folder`, could change
/// what a reload reads.
fn may_change_config(event: &notify::Event, folder: &Path, data: Option<&Path>) -> bool {
    if event.need_rescan() {
        return true;
    }
    match event.kind {
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => {}
        // Opening and reading the files, as every reload does, changes
        // nothing; counting it would reload forever.
        EventKind::Access(_) => return false,
        _ => {}
    }
    event
        .paths
        .iter()
        .any(|path| is_config_path(path, folder, data))
}

/// Whether a change at `path` could change what a reload reads: a
/// `*.toml` file or a folder, outside hidden entries (editors' swap files)
/// and the data folder. Kubernetes swaps a mounted `ConfigMap` in by
/// renaming its `..data` link, which counts.
fn is_config_path(path: &Path, folder: &Path, data: Option<&Path>) -> bool {
    let toml = path
        .extension()
        .is_some_and(|extension| extension == "toml");
    if data.is_some_and(|data| path.starts_with(data)) {
        return toml;
    }
    let relative = path.strip_prefix(folder).unwrap_or(path);
    for component in relative.components() {
        let name = component.as_os_str().to_string_lossy();
        if name.starts_with("..") {
            return true;
        }
        if name.starts_with('.') {
            return false;
        }
    }
    toml || path.extension().is_none()
}

/// A status that never changes, for tests that serve no reloads.
#[cfg(test)]
pub fn steady_status() -> watch::Receiver<ConfigStatus> {
    watch::channel(ConfigStatus::loaded(http::now())).1
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    use notify::event::{AccessKind, AccessMode, CreateKind, Flag, ModifyKind};
    use notify::{Event, EventKind};
    use reqwest::{StatusCode, header};
    use tocsin_store::{DB_FILE, Store};
    use tokio::sync::watch;
    use tokio::time::{Instant, timeout};

    use super::{
        ConfigStatus, DEBOUNCE, Debounce, LiveRouter, Reloader, Reloading, Variables,
        is_config_path, may_change_config,
    };
    use crate::cli::ServerArgs;
    use crate::exit::Exit;
    use crate::http::events::{Event as BusEvent, EventBus};
    use crate::http::{self, AppState};
    use crate::metrics::Metrics;
    use crate::serve::Monitor;
    use crate::testing::Captured;

    /// How long any one step may take before the test gives up on it.
    const PATIENCE: Duration = Duration::from_secs(20);

    const SECRET: &str = "reader-secret-0123456789";
    const LATE: &str = "late-secret-0123456789";

    const BASE: &str = r#"
[[check]]
id = "kept"
type = "tcp"
target = "TARGET"
interval = "1s"
timeout = "500ms"
confirm = 1

[[token]]
id = "reader"
secret = "${ENV:READER_TOKEN}"
scopes = ["status:read", "heartbeat"]
"#;

    const EXTRA: &str = r#"
[[check]]
id = "added"
type = "tcp"
target = "TARGET"
interval = "1s"
timeout = "500ms"
confirm = 1

[[heartbeat]]
id = "nightly"
interval = "1h"

[[token]]
id = "late"
secret = "${ENV:LATE_TOKEN}"
scopes = ["status:read"]
"#;

    /// A component with a check, a heartbeat and a notifier, all of which
    /// have series in `/metrics` while the file is there.
    const EDGE: &str = r#"
[[component]]
id = "edge"
name = "Edge"

[[check]]
id = "edge-tcp"
component = "edge"
type = "tcp"
target = "TARGET"
interval = "1s"
timeout = "500ms"
confirm = 1

[[heartbeat]]
id = "edge-sync"
component = "edge"
interval = "1h"

[[notifier]]
id = "edge-pager"
type = "webhook"

[notifier.webhook]
url = "https://pager.example.com/hook"
signing_secret = "${ENV:EDGE_SIGNING_SECRET}"
"#;

    /// Valid but for a secret from a variable nobody set; it also drops
    /// `added` and `nightly`, which must therefore keep running.
    const BROKEN: &str = r#"
[[token]]
id = "deploys"
secret = "${ENV:NEVER_SET}"
scopes = ["release"]
"#;

    /// A running reloader with the HTTP API in front of it.
    struct Served {
        folder: PathBuf,
        url: String,
        status: watch::Receiver<ConfigStatus>,
        metrics: Arc<Metrics>,
        events: tokio::sync::broadcast::Receiver<BusEvent>,
        reloader: Reloader,
        store: Arc<Store>,
        node: String,
        http: reqwest::Client,
        _dirs: (tempfile::TempDir, tempfile::TempDir),
    }

    impl Served {
        async fn start(target: &str) -> Self {
            let config_dir = tempfile::tempdir().unwrap();
            let data_dir = tempfile::tempdir().unwrap();
            let folder = config_dir.path().to_owned();
            std::fs::write(folder.join("base.toml"), BASE.replace("TARGET", target)).unwrap();
            let variables: Variables = Arc::new(|name| match name {
                "READER_TOKEN" => Some(SECRET.to_owned()),
                "LATE_TOKEN" => Some(LATE.to_owned()),
                "EDGE_SIGNING_SECRET" => Some("0123456789abcdef0123456789abcdef".to_owned()),
                _ => None,
            });
            let config = crate::config::load_with(&folder, &*variables)
                .unwrap()
                .config;
            let node = config.server.node_id.clone().unwrap();
            let store = Arc::new(Store::open(&data_dir.path().join(DB_FILE)).unwrap());
            let bus = EventBus::new();
            let events = bus.subscribe();
            let metrics = Arc::new(Metrics::new());
            let monitor = Monitor::start(&config, &store, &bus, &metrics)
                .await
                .unwrap();
            let (sender, status) = watch::channel(ConfigStatus::loaded(http::now()));
            let state = AppState {
                store: Arc::clone(&store),
                config: Arc::new(config),
                events: bus,
                admin: None,
                live: Arc::new(monitor.status_handle()),
                heartbeats: monitor.heartbeats(),
                alert_wake: monitor.alert_waker(),
                config_status: status.clone(),
                metrics: Arc::clone(&metrics),
                subscribers: None,
                slack: None,
            };
            let app = LiveRouter::new(http::router(state.clone()));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let service = app.service();
            tokio::spawn(async move { axum::serve(listener, service).await });
            let reloader = Reloader::start(
                Reloading {
                    folder: folder.clone(),
                    data: data_dir.path().to_owned(),
                    variables,
                    state,
                    app,
                    status: sender,
                    metrics: metrics.config.clone(),
                },
                monitor,
                None,
            );
            Served {
                folder,
                url,
                status,
                metrics,
                events,
                reloader,
                store,
                node,
                http: crate::net::http_client(PATIENCE).build().unwrap(),
                _dirs: (config_dir, data_dir),
            }
        }

        /// Writes `contents` to `name`, or removes it when `None`, and waits
        /// for the reload that follows.
        async fn change(&mut self, name: &str, contents: Option<&str>) -> ConfigStatus {
            let before = self.status.borrow().last_attempt_at;
            let path = self.folder.join(name);
            match contents {
                Some(contents) => std::fs::write(&path, contents).unwrap(),
                None => std::fs::remove_file(&path).unwrap(),
            }
            timeout(
                PATIENCE,
                self.status
                    .wait_for(|status| status.last_attempt_at > before),
            )
            .await
            .expect("a change to the folder reloads it")
            .unwrap()
            .clone()
        }

        /// Waits for `id`'s first state.
        async fn first_state(&mut self, id: &str) {
            timeout(PATIENCE, async {
                loop {
                    if let BusEvent::CheckChanged(change) = self.events.recv().await.unwrap()
                        && change.check_id == id
                    {
                        return;
                    }
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{id} runs"));
        }

        async fn ping(&self, id: &str) -> StatusCode {
            self.http
                .post(format!("{}/api/v1/heartbeat/{id}", self.url))
                .bearer_auth(SECRET)
                .send()
                .await
                .unwrap()
                .status()
        }

        /// A status page at `path`: its tag and its body.
        async fn page(&self, path: &str) -> (String, String) {
            let response = self
                .http
                .get(format!("{}{path}", self.url))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            let tag = response.headers()[header::ETAG]
                .to_str()
                .unwrap()
                .to_owned();
            (tag, response.text().await.unwrap())
        }

        /// `GET /metrics`, read with a `status:read` token.
        async fn scrape(&self) -> String {
            let response = self
                .http
                .get(format!("{}/metrics", self.url))
                .bearer_auth(SECRET)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            response.text().await.unwrap()
        }

        async fn status_code_for(&self, token: &str) -> StatusCode {
            self.http
                .get(format!("{}/api/v1/config/status", self.url))
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
                .status()
        }

        async fn status_over_http(&self) -> ConfigStatus {
            let response = self
                .http
                .get(format!("{}/api/v1/config/status", self.url))
                .bearer_auth(SECRET)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            serde_json::from_slice(&response.bytes().await.unwrap()).unwrap()
        }

        /// `tocsin config status`, run as the CLI runs it.
        async fn status_from_cli(&self) -> (Exit, serde_json::Value) {
            let server = ServerArgs {
                url: self.url.clone(),
                token: Some(SECRET.to_owned()),
            };
            let report = tokio::task::spawn_blocking(move || crate::config::status(&server))
                .await
                .unwrap()
                .unwrap_or_else(|failure| panic!("{}", failure.message));
            (report.exit, report.json)
        }
    }

    /// Accepts connections for the tcp checks until dropped.
    async fn target() -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let accepting = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        (address, accepting)
    }

    fn running(monitor: &Monitor) -> Vec<String> {
        monitor
            .snapshot()
            .checks
            .into_iter()
            .map(|check| check.id)
            .collect()
    }

    #[tokio::test]
    async fn a_bad_reload_keeps_the_last_good_configuration_and_says_why_everywhere() {
        let log = Captured::default();
        let _log = log.capture();
        let (target, _accepting) = target().await;
        let mut served = Served::start(&target).await;
        assert_eq!(served.ping("nightly").await, StatusCode::NOT_FOUND);
        assert_eq!(served.status_code_for(LATE).await, StatusCode::UNAUTHORIZED);

        // A valid edit: the new check and heartbeat run.
        let extra = EXTRA.replace("TARGET", &target);
        let status = served.change("extra.toml", Some(&extra)).await;
        assert!(status.ok, "{status:?}");
        assert!(status.errors.is_empty(), "{status:?}");
        assert_eq!(status.loaded_at, status.last_attempt_at);
        served.first_state("added").await;
        assert_eq!(served.ping("nightly").await, StatusCode::ACCEPTED);
        assert_eq!(
            served.status_code_for(LATE).await,
            StatusCode::OK,
            "requests are served from the new configuration"
        );
        let loaded_at = status.loaded_at;

        // An invalid one: nothing changes, and every surface says why.
        let status = served.change("extra.toml", Some(BROKEN)).await;
        assert!(!status.ok, "{status:?}");
        assert_eq!(status.loaded_at, loaded_at, "the old configuration runs");
        assert!(status.last_attempt_at > loaded_at);
        assert!(
            status
                .errors
                .iter()
                .any(|error| error.message.contains("NEVER_SET")),
            "{status:?}"
        );
        assert_eq!(
            served.ping("nightly").await,
            StatusCode::ACCEPTED,
            "the heartbeat the broken file drops still runs"
        );
        // 1. The log.
        let logged = log.text();
        assert!(
            logged.contains("configuration reload failed") && logged.contains("NEVER_SET"),
            "{logged}"
        );
        // 2. The metrics.
        let metrics = served.metrics.encode();
        assert!(
            metrics.contains("tocsin_config_reload_success 0\n"),
            "{metrics}"
        );
        let seconds = status.last_attempt_at.as_millis().div_euclid(1000);
        assert!(
            metrics.contains(&format!("tocsin_config_last_error_timestamp {seconds}\n")),
            "{metrics}"
        );
        // 3. The API, behind a token from the configuration still running.
        assert_eq!(served.status_over_http().await, status);
        // 4. The CLI.
        let (exit, json) = served.status_from_cli().await;
        assert!(matches!(exit, Exit::CheckFailed), "{json}");
        assert_eq!(json, serde_json::json!(status));

        // Fixing the file puts it in place.
        let status = served.change("extra.toml", Some(&extra)).await;
        assert!(status.ok, "{status:?}");
        assert!(status.errors.is_empty(), "{status:?}");
        assert_eq!(status.loaded_at, status.last_attempt_at);
        let metrics = served.metrics.encode();
        assert!(
            metrics.contains("tocsin_config_reload_success 1\n"),
            "{metrics}"
        );
        assert_eq!(served.status_over_http().await, status);
        let (exit, _) = served.status_from_cli().await;
        assert!(matches!(exit, Exit::Ok));

        // Removing the file stops what it held, and keeps its history.
        let status = served.change("extra.toml", None).await;
        assert!(status.ok, "{status:?}");
        assert_eq!(served.ping("nightly").await, StatusCode::NOT_FOUND);
        assert_eq!(served.status_code_for(LATE).await, StatusCode::UNAUTHORIZED);
        let monitor = served.reloader.into_monitor().await;
        assert_eq!(running(&monitor), ["kept"]);
        monitor.stop().await;
        let history = served
            .store
            .latest_results(&served.node, vec!["added".to_owned()])
            .await
            .unwrap();
        assert_eq!(history.len(), 1, "{history:?}");
    }

    /// The label contract the alert sweep routes on.
    const CONTRACT: &str = r#"
[alerting.labels.needs_human]
required = true
values = ["false", "true"]
"#;

    #[tokio::test]
    async fn a_reload_that_breaks_the_label_contract_keeps_the_last_good_configuration() {
        let (target, _accepting) = target().await;
        let mut served = Served::start(&target).await;
        let labelled = |value: &str| {
            BASE.replace("TARGET", &target).replace(
                "confirm = 1\n",
                &format!("confirm = 1\nlabels = {{ needs_human = \"{value}\" }}\n"),
            )
        };
        let names = |status: &ConfigStatus, path: &str, message: &str| {
            assert!(
                status.errors.iter().any(|error| {
                    error
                        .file
                        .as_deref()
                        .is_some_and(|file| file.ends_with("/base.toml"))
                        && error.path.as_deref() == Some(path)
                        && error.message.contains(message)
                }),
                "{status:?}"
            );
        };

        // A contract the running check does not meet is refused.
        let status = served.change("alerting.toml", Some(CONTRACT)).await;
        assert!(!status.ok, "{status:?}");
        names(
            &status,
            "check[0].labels.needs_human",
            "check `kept` is missing label `needs_human`",
        );

        // Meeting it loads both.
        let status = served.change("base.toml", Some(&labelled("false"))).await;
        assert!(status.ok, "{status:?}");
        let loaded_at = status.loaded_at;

        // A value outside it is refused, and the last good configuration
        // keeps running.
        let status = served.change("base.toml", Some(&labelled("ture"))).await;
        assert!(!status.ok, "{status:?}");
        assert_eq!(status.loaded_at, loaded_at, "the old configuration runs");
        names(
            &status,
            "check[0].labels.needs_human",
            "check `kept`: label `needs_human` is `ture`",
        );
        assert_eq!(served.status_over_http().await, status);
        let monitor = served.reloader.into_monitor().await;
        assert_eq!(running(&monitor), ["kept"]);
        monitor.stop().await;
    }

    #[tokio::test]
    async fn a_reload_with_an_unknown_probe_class_keeps_the_last_good_configuration() {
        let (target, _accepting) = target().await;
        let mut served = Served::start(&target).await;
        let classed = |class: &str| {
            BASE.replace("TARGET", &target).replace(
                "confirm = 1\n",
                &format!("confirm = 1\nclass = \"{class}\"\n"),
            )
        };

        let status = served.change("base.toml", Some(&classed("drill"))).await;
        assert!(status.ok, "{status:?}");
        let loaded_at = status.loaded_at;

        // A class tocsin does not know would leave receivers routing on a
        // value nobody wrote a rule for: it is refused, and the drill runs on.
        let status = served.change("base.toml", Some(&classed("exercise"))).await;
        assert!(!status.ok, "{status:?}");
        assert_eq!(status.loaded_at, loaded_at, "the old configuration runs");
        assert!(
            status
                .errors
                .iter()
                .any(|error| error.message.contains("exercise")),
            "{status:?}"
        );
        let monitor = served.reloader.into_monitor().await;
        assert_eq!(running(&monitor), ["kept"]);
        monitor.stop().await;
    }

    #[tokio::test]
    async fn the_status_page_follows_a_reload() {
        let (target, _accepting) = target().await;
        let mut served = Served::start(&target).await;
        let page = |title: &str, link: &str| {
            format!(
                "[page]\ntitle = \"{title}\"\n\n[[page.link]]\nname = \"{link}\"\nurl = \"https://example.com/{link}\"\n"
            )
        };
        let status = served
            .change("page.toml", Some(&page("Kestrel", "Handbook")))
            .await;
        assert!(status.ok, "{status:?}");
        let (before, body) = served.page("/").await;
        assert!(
            body.contains("Kestrel") && body.contains("Handbook"),
            "{body}"
        );

        let status = served
            .change("page.toml", Some(&page("Osprey", "Runbook")))
            .await;
        assert!(status.ok, "{status:?}");
        let (after, body) = served.page("/").await;
        assert!(
            body.contains("Osprey") && body.contains("Runbook"),
            "{body}"
        );
        assert!(
            !body.contains("Kestrel") && !body.contains("Handbook"),
            "{body}"
        );
        assert_ne!(before, after, "a reloaded page is a new representation");
        let (_, history) = served.page("/history").await;
        assert!(history.contains("Osprey"), "{history}");
        served.reloader.into_monitor().await.stop().await;
    }

    #[tokio::test]
    async fn what_a_reload_removes_leaves_the_metrics() {
        let (target, _accepting) = target().await;
        let mut served = Served::start(&target).await;
        let edge = EDGE.replace("TARGET", &target);
        let status = served.change("edge.toml", Some(&edge)).await;
        assert!(status.ok, "{status:?}");
        served.first_state("edge-tcp").await;
        assert_eq!(served.ping("edge-sync").await, StatusCode::ACCEPTED);
        served.metrics.alerting.attempt_failed("edge-pager");
        let before = served.scrape().await;
        for series in [
            "tocsin_check_up{check=\"edge-tcp\",component=\"edge\",",
            "tocsin_check_duration_seconds_count{check=\"edge-tcp\",component=\"edge\",",
            "tocsin_check_last_run_timestamp{check=\"edge-tcp\",component=\"edge\",",
            "tocsin_heartbeat_last_ping_timestamp{check=\"edge-sync\",component=\"edge\",",
            "tocsin_component_status{component=\"edge\",",
            "tocsin_notification_failures_total{notifier=\"edge-pager\"} 1\n",
        ] {
            assert!(before.contains(series), "{series} missing from\n{before}");
        }

        let status = served.change("edge.toml", None).await;
        assert!(status.ok, "{status:?}");
        let after = served.scrape().await;
        for gone in ["\"edge", "edge-"] {
            assert!(!after.contains(gone), "{gone} still served:\n{after}");
        }
        assert!(
            after.contains("tocsin_check_up{check=\"kept\","),
            "what the reload kept is still served:\n{after}"
        );
        served.reloader.into_monitor().await.stop().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_of_changes_is_read_once_after_the_last() {
        let mut debounce = Debounce::new(DEBOUNCE);
        let start = Instant::now();
        debounce.restart();
        tokio::time::sleep(DEBOUNCE / 2).await;
        debounce.restart();
        debounce.elapsed().await;
        assert_eq!(start.elapsed(), DEBOUNCE / 2 + DEBOUNCE);
        assert!(
            timeout(Duration::from_secs(3600), debounce.elapsed())
                .await
                .is_err(),
            "and not again until something changes"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_reload_cancels_the_one_pending() {
        let mut debounce = Debounce::new(DEBOUNCE);
        debounce.restart();
        debounce.cancel();
        assert!(
            timeout(Duration::from_secs(3600), debounce.elapsed())
                .await
                .is_err()
        );
    }

    fn event(kind: EventKind, path: &Path) -> Event {
        Event::new(kind).add_path(path.to_owned())
    }

    #[test]
    fn reading_the_configuration_is_not_a_change_to_it() {
        let folder = Path::new("/etc/tocsin");
        let file = folder.join("checks.toml");
        let wrote = EventKind::Modify(ModifyKind::Any);
        assert!(may_change_config(&event(wrote, &file), folder, None));
        let closed = EventKind::Access(AccessKind::Close(AccessMode::Write));
        assert!(may_change_config(&event(closed, &file), folder, None));
        for read in [
            AccessKind::Open(AccessMode::Any),
            AccessKind::Read,
            AccessKind::Close(AccessMode::Read),
        ] {
            assert!(
                !may_change_config(&event(EventKind::Access(read), &file), folder, None),
                "{read:?}"
            );
        }
        let lost = Event::new(EventKind::Other).set_flag(Flag::Rescan);
        assert!(may_change_config(&lost, folder, None), "missed events");
    }

    #[test]
    fn only_what_a_reload_reads_counts() {
        let folder = Path::new("/etc/tocsin");
        let data = folder.join("data");
        let counts = |path: &str| is_config_path(&folder.join(path), folder, Some(&data));
        assert!(counts("checks.toml"));
        assert!(counts("team/checks.toml"));
        assert!(counts("team"), "a folder moved in");
        assert!(counts("..data"), "a ConfigMap swap");
        assert!(counts("..2026_10_23/checks.toml"));
        assert!(!counts(".checks.toml.swp"), "an editor's swap file");
        assert!(!counts(".git/index"));
        assert!(!counts("checks.toml~"));
        assert!(!counts("data/tocsin.db-wal"), "the database");
        assert!(counts("data/extra.toml"));
        let created = EventKind::Create(CreateKind::File);
        assert!(!may_change_config(
            &event(created, &data.join("tocsin.db")),
            folder,
            Some(&data)
        ));
    }
}
