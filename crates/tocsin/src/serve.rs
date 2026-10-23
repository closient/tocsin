// SPDX-License-Identifier: AGPL-3.0-only

//! `tocsin serve`: run until told to stop.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tocsin_core::config::Config;
use tocsin_store::{DB_FILE, Retention, Store};
use tokio::signal::unix::{Signal, SignalKind, signal};
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
/// Resolves with the signal's name.
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
    let mut server = tokio::spawn(async move {
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
    let signal_name = tokio::select! {
        signal_name = shutdown.wait() => signal_name,
        result = &mut server => {
            let message = match result {
                Ok(Ok(())) => "the HTTP server stopped on its own".to_owned(),
                Ok(Err(error)) => format!("the HTTP server failed: {error}"),
                Err(error) => format!("the HTTP server panicked: {error}"),
            };
            return Err(Failure::new(Exit::Runtime, message));
        }
    };
    tracing::info!(signal = signal_name, "shutting down");
    // Stop accepting, then let requests in flight finish so their
    // writes commit before the store closes.
    let _ = stop.send(());
    match tokio::time::timeout(DRAIN_FOR, &mut server).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(error))) => tracing::warn!(%error, "HTTP server stopped with an error"),
        Ok(Err(error)) => tracing::warn!(%error, "HTTP server panicked while draining"),
        Err(_) => {
            tracing::warn!(
                seconds = DRAIN_FOR.as_secs(),
                "requests still running; dropping them"
            );
            server.abort();
            let _ = server.await;
        }
    }
    maintenance.abort();
    // An aborted task drops its handle on the store at its next await,
    // which `await` on the join handle waits for.
    let _ = maintenance.await;
    let Ok(store) = Arc::try_unwrap(store) else {
        tracing::warn!("database still in use at shutdown; not closed");
        return Ok(signal_name);
    };
    if let Err(error) = store.close().await {
        tracing::warn!(%error, "database did not close cleanly");
    }
    Ok(signal_name)
}

/// How long requests in flight may run after a shutdown signal.
const DRAIN_FOR: Duration = Duration::from_secs(10);

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
