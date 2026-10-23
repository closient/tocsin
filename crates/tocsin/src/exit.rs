// SPDX-License-Identifier: AGPL-3.0-only

//! Process exit codes. These are a public contract: scripts and agents
//! branch on them, so a value never changes meaning.

use std::process::ExitCode;

/// Every exit code `tocsin` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// The command did what was asked.
    Ok = 0,
    /// The command ran, and a check it ran failed; or `tocsin status`
    /// found something degraded or down.
    CheckFailed = 1,
    /// Bad arguments, or configuration that does not validate.
    InvalidInput = 2,
    /// Anything else that stopped the command: I/O, the runtime, the database.
    Runtime = 3,
}

impl Exit {
    /// The stable machine-readable name used in JSON error documents.
    pub fn kind(self) -> &'static str {
        match self {
            Exit::Ok => "ok",
            Exit::CheckFailed => "check_failed",
            Exit::InvalidInput => "invalid_input",
            Exit::Runtime => "runtime_error",
        }
    }
}

impl From<Exit> for ExitCode {
    fn from(exit: Exit) -> Self {
        ExitCode::from(exit as u8)
    }
}
