// SPDX-License-Identifier: AGPL-3.0-only

//! Rendering command results as text or as a single JSON document.

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use serde_json::{Value, json};

use crate::exit::Exit;

/// A successful result, renderable either way.
pub struct Report {
    pub json: Value,
    pub text: String,
}

impl Report {
    pub fn version() -> Self {
        let version = env!("CARGO_PKG_VERSION");
        Report {
            json: json!({ "version": version }),
            text: format!("tocsin {version}"),
        }
    }
}

/// A failed result: the exit code plus a human-readable message.
#[derive(Debug)]
pub struct Failure {
    pub exit: Exit,
    pub message: String,
}

impl Failure {
    pub fn new(exit: Exit, message: impl Into<String>) -> Self {
        Failure {
            exit,
            message: message.into(),
        }
    }

    fn json(&self) -> Value {
        json!({ "error": { "kind": self.exit.kind(), "message": self.message } })
    }
}

/// Whether `--json` appears before any `--` terminator.
///
/// Read from the raw arguments so a parse error can still be reported as
/// JSON when the arguments themselves are what is wrong.
pub fn json_requested(args: impl IntoIterator<Item = OsString>) -> bool {
    args.into_iter()
        .skip(1)
        .take_while(|arg| arg != "--")
        .any(|arg| arg == "--json")
}

pub fn emit(result: Result<Report, Failure>, json: bool) -> ExitCode {
    match result {
        Ok(report) => {
            if json {
                write_stdout(&format!("{}\n", report.json));
            } else {
                write_stdout(&format!("{}\n", report.text));
            }
            Exit::Ok.into()
        }
        Err(failure) => {
            if json {
                write_stdout(&format!("{}\n", failure.json()));
            } else {
                write_stderr(&format!("error: {}\n", failure.message));
            }
            failure.exit.into()
        }
    }
}

/// `--help` prints help and exits 0; every other parse error exits 2.
pub fn emit_parse_error(error: &clap::Error, json: bool) -> ExitCode {
    use clap::error::ErrorKind;
    let rendered = error.render().to_string();
    if matches!(error.kind(), ErrorKind::DisplayHelp) {
        if json {
            write_stdout(&format!("{}\n", json!({ "help": rendered })));
        } else {
            write_stdout(&rendered);
        }
        return Exit::Ok.into();
    }
    let failure = Failure::new(Exit::InvalidInput, rendered.trim());
    if json {
        write_stdout(&format!("{}\n", failure.json()));
    } else {
        write_stderr(&rendered);
    }
    failure.exit.into()
}

// `print!` and friends panic when the reader has gone away, which would turn
// a closed pipe into exit 101. The exit code is the contract; a reader that
// left early has already given up on the output, so a failed write is dropped.
fn write_stdout(text: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush());
}

fn write_stderr(text: &str) {
    let _ = std::io::stderr().lock().write_all(text.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::json_requested;

    fn args(list: &[&str]) -> Vec<std::ffi::OsString> {
        std::iter::once("tocsin")
            .chain(list.iter().copied())
            .map(Into::into)
            .collect()
    }

    #[test]
    fn json_flag_found_anywhere_before_terminator() {
        assert!(json_requested(args(&["--json", "serve"])));
        assert!(json_requested(args(&["serve", "--json"])));
        assert!(!json_requested(args(&["serve", "--", "--json"])));
        assert!(!json_requested(args(&["serve"])));
    }
}
