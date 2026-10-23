// SPDX-License-Identifier: AGPL-3.0-only

//! Command-line definition.

use std::path::PathBuf;

use clap::{ArgAction, Parser, Subcommand};

/// Single-binary uptime monitor, status page and release log.
#[derive(Debug, Parser)]
#[command(name = "tocsin", disable_version_flag = true)]
pub struct Cli {
    /// Print the version and exit.
    ///
    /// Declared by hand rather than through clap so that `--json` applies.
    #[arg(short = 'V', long, global = true, action = ArgAction::SetTrue)]
    pub version: bool,

    /// Write exactly one JSON document to stdout instead of text.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the monitor until SIGINT or SIGTERM.
    Serve {
        /// Folder of `*.toml` configuration files.
        #[arg(long, value_name = "DIR")]
        config: PathBuf,

        /// Folder for the SQLite database; created if missing. Defaults to
        /// `server.data_dir` from the configuration.
        #[arg(long, value_name = "DIR")]
        data: Option<PathBuf>,
    },
    /// Validate, show or describe the configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Validate every `*.toml` file in the folder, reporting every error
    /// with its file, line and column. Exits 2 if any error is found.
    Check {
        /// Folder of `*.toml` configuration files.
        #[arg(long, value_name = "DIR", default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// Print the merged configuration with defaults filled in and secrets
    /// redacted.
    Show {
        /// Folder of `*.toml` configuration files.
        #[arg(long, value_name = "DIR", default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// Print the JSON Schema that every configuration file follows.
    Schema,
}

/// Where `tocsin config` looks when `--config` is not given.
const DEFAULT_CONFIG: &str = "/etc/tocsin";
