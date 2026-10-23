// SPDX-License-Identifier: AGPL-3.0-only

//! The `tocsin` command: CLI and server.
//!
//! Output contract (see `AGENTS.md`): with `--json`, exactly one JSON
//! document goes to stdout and logs go to stderr. Exit codes are defined
//! in [`exit::Exit`].

#[cfg(not(unix))]
compile_error!("tocsin supports Unix-like systems (Linux, macOS) only");

mod cli;
mod config;
mod db;
mod exit;
mod lock;
mod output;
mod probe;
mod serve;

use std::io::IsTerminal;
use std::process::ExitCode;

use clap::Parser;

use crate::cli::{Cli, Command, ConfigCommand, ProbeCommand};
use crate::exit::Exit;
use crate::output::{Failure, Report};

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            // The arguments did not parse, so the raw scan is the only
            // way to know whether the caller asked for JSON.
            let json = output::json_requested(std::env::args_os());
            return output::emit_parse_error(&error, json);
        }
    };
    init_tracing();
    let json = cli.json;
    output::emit(run(cli), json)
}

fn run(cli: Cli) -> Result<Report, Failure> {
    if cli.version {
        if cli.command.is_some() {
            return Err(Failure::new(
                Exit::InvalidInput,
                "--version cannot be combined with a command",
            ));
        }
        return Ok(Report::version());
    }
    match cli.command {
        Some(Command::Serve { config, data }) => serve::run(&config, data.as_deref()),
        Some(Command::Config { command }) => match command {
            ConfigCommand::Check { config } => config::check(&config),
            ConfigCommand::Show { config } => config::show(&config),
            ConfigCommand::Schema => Ok(config::schema()),
        },
        Some(Command::Db { command }) => db::run(command),
        Some(Command::Probe { command }) => match command {
            ProbeCommand::Run { check, config } => probe::run(&check, &config),
        },
        None => Err(Failure::new(
            Exit::InvalidInput,
            "no command given; run `tocsin --help` for usage",
        )),
    }
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let (filter, rejected) = match std::env::var("TOCSIN_LOG") {
        Ok(directives) => match EnvFilter::try_new(&directives) {
            Ok(filter) => (filter, None),
            Err(error) => (EnvFilter::new("info"), Some((directives, error))),
        },
        Err(_) => (EnvFilter::new("info"), None),
    };
    let logs = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        // A failed log write is otherwise reported with `eprintln!`, which
        // panics when stderr is a broken pipe and takes the monitor down.
        .log_internal_errors(false);
    tracing_subscriber::registry()
        .with(filter)
        .with(logs)
        .init();
    if let Some((directives, error)) = rejected {
        tracing::warn!(%directives, %error, "ignoring invalid TOCSIN_LOG; logging at info");
    }
}
