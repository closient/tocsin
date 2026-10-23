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
    #[arg(short = 'V', long, action = ArgAction::SetTrue)]
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

        /// Folder for the SQLite database; created if missing.
        #[arg(long, value_name = "DIR")]
        data: PathBuf,
    },
}
