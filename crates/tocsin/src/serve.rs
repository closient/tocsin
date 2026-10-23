// SPDX-License-Identifier: AGPL-3.0-only

//! `tocsin serve`: run until told to stop.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tocsin_core::config::Config;
use tocsin_store::{DB_FILE, Retention, Store};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::exit::Exit;
use crate::http::{self, AppState, events::EventBus};
use crate::lock;
use crate::output::{Failure, Report};

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

/// Serves until a shutdown signal, then drains and closes the store.
/// Resolves with the signal's name, or fails if the server ends first.
async fn serve(
    config_folder: &Path,
    data: &Path,
    config: &Arc<Config>,
    files: usize,
    retention_days: u32,
) -> Result<&'static str, Failure> {
    let shutdown = Shutdown::install()?;
    let path = data.join(DB_FILE);
    let store = Store::open(&path).map_err(|error| {
        Failure::new(
            Exit::Runtime,
            format!("cannot open database {}: {error}", path.display()),
        )
    })?;
    let store = Arc::new(store);
    let listen = config.server.listen;
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|error| {
            Failure::new(Exit::Runtime, format!("cannot listen on {listen}: {error}"))
        })?;
    let address = listener.local_addr().map_err(|error| {
        Failure::new(Exit::Runtime, format!("cannot listen on {listen}: {error}"))
    })?;
    let maintenance = tokio::spawn(maintain(
        Arc::clone(&store),
        Retention::days(retention_days),
    ));
    let app = http::router(AppState {
        store: Arc::clone(&store),
        config: Arc::clone(config),
        events: EventBus::new(),
    });
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
    });
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
        "tocsin serving; waiting for SIGINT or SIGTERM"
    );
    let (outcome, _closed) = supervise(shutdown.wait(), stop, server, maintenance, store).await;
    outcome
}

/// Waits for `shutdown` or for the server to end on its own, then stops
/// everything that holds the store and closes it. Every way out runs the
/// same cleanup; the flag says whether the store closed.
async fn supervise(
    shutdown: impl Future<Output = &'static str>,
    stop: oneshot::Sender<()>,
    mut server: JoinHandle<io::Result<()>>,
    maintenance: JoinHandle<()>,
    store: Arc<Store>,
) -> (Result<&'static str, Failure>, bool) {
    let ended = tokio::select! {
        signal_name = shutdown => Ok(signal_name),
        result = &mut server => Err(result),
    };
    let outcome = match ended {
        Ok(signal_name) => {
            tracing::info!(signal = signal_name, "shutting down");
            drain(stop, server).await;
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

/// Hourly: roll up the days since the last pass, then delete results older
/// than the retention window. The first pass runs at startup. Failures are
/// logged and retried on the next tick; a pass never stops the monitor.
async fn maintain(store: Arc<Store>, retention: Retention) {
    let mut ticks = tokio::time::interval(MAINTENANCE_EVERY);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
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
}

/// Handlers for the signals that stop `tocsin serve`.
struct Shutdown {
    interrupt: Signal,
    terminate: Signal,
}

impl Shutdown {
    fn install() -> Result<Self, Failure> {
        let install = |kind: SignalKind| {
            signal(kind).map_err(|error| {
                Failure::new(
                    Exit::Runtime,
                    format!("cannot install signal handler: {error}"),
                )
            })
        };
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
    use std::sync::{Arc, Weak};

    use tocsin_store::{DB_FILE, Store};
    use tokio::sync::oneshot;

    use super::{io, supervise};
    use crate::exit::Exit;

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

        let (outcome, closed) = supervise(pending(), stop, server, maintenance, store).await;

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

        let (outcome, closed) = supervise(ready("SIGTERM"), stop, server, maintenance, store).await;

        assert_eq!(outcome.unwrap(), "SIGTERM");
        assert!(closed, "the store was not closed");
        assert!(weak.upgrade().is_none(), "something still holds the store");
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

        let (outcome, closed) = supervise(ready("SIGINT"), stop, server, maintenance, store).await;

        assert_eq!(outcome.unwrap(), "SIGINT");
        assert!(!closed, "closed a store something else still holds");
        assert_eq!(Arc::strong_count(&straggler), 1);
    }
}
