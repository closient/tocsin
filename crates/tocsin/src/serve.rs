// SPDX-License-Identifier: AGPL-3.0-only

//! `tocsin serve`: run until told to stop.

use std::path::{Path, PathBuf};

use serde_json::json;
use tokio::signal::unix::{Signal, SignalKind, signal};

use crate::exit::Exit;
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

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| Failure::new(Exit::Runtime, format!("cannot start runtime: {error}")))?;
    let signal_name = runtime.block_on(async {
        let shutdown = Shutdown::install()?;
        // Logged only once both handlers exist, so a signal sent after this
        // line is always handled rather than killing the process outright.
        tracing::info!(
            config = %config.display(),
            data = %data.display(),
            files = loaded.files.len(),
            checks = loaded.config.checks.len(),
            "tocsin serving; waiting for SIGINT or SIGTERM"
        );
        Ok::<_, Failure>(shutdown.wait().await)
    })?;
    tracing::info!(signal = signal_name, "shutting down");

    Ok(Report {
        json: json!({ "status": "stopped", "signal": signal_name }),
        text: format!("tocsin stopped ({signal_name})"),
    })
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
