// SPDX-License-Identifier: AGPL-3.0-only

//! Rendering command results as text or as a single JSON document.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::Write;
use std::process::ExitCode;

use serde_json::{Value, json};
use tocsin_core::config::Diagnostic;

use crate::exit::Exit;

/// A result the command ran to the end, renderable either way.
pub struct Report {
    pub json: Value,
    pub text: String,
    /// [`Exit::Ok`], or [`Exit::CheckFailed`] when the command ran a check
    /// and the check failed; the output is printed the same either way.
    pub exit: Exit,
}

impl Report {
    pub fn version() -> Self {
        let version = env!("CARGO_PKG_VERSION");
        Report {
            json: json!({ "version": version }),
            text: format!("tocsin {version}"),
            exit: Exit::Ok,
        }
    }
}

/// A failed result: the exit code, a human-readable message and, for
/// invalid configuration, every problem found.
#[derive(Debug)]
pub struct Failure {
    pub exit: Exit,
    pub message: String,
    pub errors: Vec<Diagnostic>,
}

impl Failure {
    pub fn new(exit: Exit, message: impl Into<String>) -> Self {
        Failure {
            exit,
            message: message.into(),
            errors: Vec::new(),
        }
    }

    /// Configuration that does not validate: exit 2 with every problem.
    pub fn invalid_config(message: impl Into<String>, errors: Vec<Diagnostic>) -> Self {
        Failure {
            exit: Exit::InvalidInput,
            message: message.into(),
            errors,
        }
    }

    /// `{"error":{"kind","message"}}`, plus `errors` when there are any.
    fn json(&self) -> Value {
        let mut error = json!({ "kind": self.exit.kind(), "message": self.message });
        if !self.errors.is_empty() {
            error["errors"] = json!(self.errors);
        }
        json!({ "error": error })
    }

    /// The message, then each problem on its own lines.
    fn text(&self) -> String {
        let mut text = format!("error: {}\n", self.message);
        for diagnostic in &self.errors {
            let _ = writeln!(text, "{diagnostic}");
        }
        text
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
            report.exit.into()
        }
        Err(failure) => {
            if json {
                write_stdout(&format!("{}\n", failure.json()));
            } else {
                write_stderr(&failure.text());
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
