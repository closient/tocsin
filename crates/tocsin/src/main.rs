// SPDX-License-Identifier: AGPL-3.0-only

//! The `tocsin` command: CLI and server.
//!
//! Output contract (see `AGENTS.md`): with `--json`, exactly one JSON
//! document goes to stdout and logs go to stderr. Exit codes are defined
//! in [`exit::Exit`].

mod cli;
mod exit;
mod output;
mod serve;

use std::io::IsTerminal;
use std::process::ExitCode;

use clap::Parser;

use crate::cli::{Cli, Command};
use crate::exit::Exit;
use crate::output::{Failure, Report};

fn main() -> ExitCode {
    let json = output::json_requested(std::env::args_os());
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => return output::emit_parse_error(&error, json),
    };
    init_tracing();
    let result = run(cli);
    output::emit(result, json)
}

fn run(cli: Cli) -> Result<Report, Failure> {
    if cli.version {
        return Ok(Report::version());
    }
    match cli.command {
        Some(Command::Serve { config, data }) => serve::run(&config, &data),
        None => Err(Failure::new(
            Exit::InvalidInput,
            "no command given; run `tocsin --help` for usage",
        )),
    }
}

fn init_tracing() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = tracing_subscriber::EnvFilter::try_from_env("TOCSIN_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
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
}
