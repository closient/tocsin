// SPDX-License-Identifier: AGPL-3.0-only

//! The `tocsin` command: CLI and server.
//!
//! Output contract (see `AGENTS.md`): with `--json`, exactly one JSON
//! document goes to stdout and logs go to stderr. Exit codes are defined
//! in [`exit::Exit`].

#[cfg(not(unix))]
compile_error!("tocsin supports Unix-like systems (Linux, macOS) only");

mod cli;
mod client;
mod config;
mod db;
mod exit;
mod export;
mod heartbeat;
mod http;
mod incident;
mod liveness;
mod lock;
mod mcp;
mod metrics;
mod net;
mod node;
mod notify;
mod otlp;
mod output;
mod probe;
mod push;
mod query;
mod release;
mod reload;
mod serve;
mod silence;
mod slack;
mod subscribers;
#[cfg(test)]
mod testing;
mod web;

use std::io::IsTerminal;
use std::process::ExitCode;

use clap::Parser;

use crate::cli::{
    Cli, Command, ConfigCommand, NodeCommand, NotifyCommand, ProbeCommand, SlackCommand,
};
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
    if let Some(Command::Mcp { server }) = &cli.command {
        // Stdout is the protocol's, so there is no report to print.
        return output::emit_quiet(mcp::stdio::run(server));
    }
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
        Some(Command::Serve { config, node, data }) => {
            serve::run(&config, node.as_deref(), data.as_deref())
        }
        Some(Command::Config { command }) => match command {
            ConfigCommand::Check { config, node, data } => {
                config::check(&config, node.as_deref(), data.as_deref())
            }
            ConfigCommand::Show { config, node } => config::show(&config, node.as_deref()),
            ConfigCommand::Schema => Ok(config::schema()),
            ConfigCommand::Status { server } => config::status(&server),
        },
        Some(Command::Db { command }) => db::run(command),
        Some(Command::Probe { command }) => match command {
            ProbeCommand::Run {
                check,
                config,
                node,
            } => probe::run(&check, &config, node.as_deref()),
        },
        Some(Command::Incident { command }) => incident::run(command),
        Some(Command::Maintenance { command }) => incident::run_maintenance(command),
        Some(Command::Release { command }) => release::run(command),
        Some(Command::Silence { command }) => silence::run(command),
        Some(Command::Notify { command }) => match command {
            NotifyCommand::Test {
                notifier,
                config,
                node,
            } => notify::test(&notifier, &config, node.as_deref()),
        },
        Some(Command::Slack { command }) => match command {
            SlackCommand::Setup { config, node } => slack::setup(&config, node.as_deref()),
        },
        Some(Command::Node { command }) => match command {
            NodeCommand::Keygen => node::keygen(),
        },
        Some(Command::Heartbeat { command }) => heartbeat::run(command),
        Some(Command::Openapi) => Ok(Report {
            exit: Exit::Ok,
            json: serde_json::to_value(http::openapi::spec())
                .expect("the OpenAPI document serializes"),
            text: http::openapi::pretty().trim_end().to_owned(),
        }),
        Some(Command::Status { server }) => query::status(&server),
        Some(Command::History {
            server,
            check,
            page,
        }) => query::history(&server, &check, &page),
        Some(Command::Outages { server, page }) => query::outages(&server, &page),
        Some(Command::Alerts {
            server,
            state,
            page,
        }) => query::alerts(&server, state, &page),
        Some(Command::Timeline {
            server,
            component,
            since,
            after,
            limit,
        }) => query::timeline(
            &server,
            component.as_deref(),
            since,
            after.as_deref(),
            limit,
        ),
        Some(Command::Mcp { .. }) => unreachable!("main serves `tocsin mcp` before run"),
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
