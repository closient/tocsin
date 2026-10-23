// SPDX-License-Identifier: AGPL-3.0-only

//! Rendering command results as text or as a single JSON document.

use std::ffi::OsString;
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
                println!("{}", report.json);
            } else {
                println!("{}", report.text);
            }
            Exit::Ok.into()
        }
        Err(failure) => {
            if json {
                println!("{}", failure.json());
            } else {
                eprintln!("error: {}", failure.message);
            }
            failure.exit.into()
        }
    }
}

/// `--help` prints help and exits 0; every other parse error exits 2.
pub fn emit_parse_error(error: &clap::Error, json: bool) -> ExitCode {
    use clap::error::ErrorKind;
    if matches!(error.kind(), ErrorKind::DisplayHelp) {
        print!("{error}");
        return Exit::Ok.into();
    }
    let failure = Failure::new(Exit::InvalidInput, error.render().to_string().trim());
    if json {
        println!("{}", failure.json());
    } else {
        eprint!("{error}");
    }
    failure.exit.into()
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
