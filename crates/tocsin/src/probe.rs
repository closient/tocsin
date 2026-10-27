// SPDX-License-Identifier: AGPL-3.0-only

//! `tocsin probe run`: one check, once, from the configuration.
//!
//! The JSON output is the probe's outcome as `tocsin_core::probe` defines
//! it; `docs/probes.md` documents it.

use std::fmt::Write as _;
use std::path::Path;

use serde_json::Value;
use tocsin_core::Health;
use tocsin_core::config::{Config, closest};
use tocsin_core::probe::{CompiledCheck, ProbeErrorKind, ProbeOutcome};
use tocsin_probe::ProbeContext;

use crate::config::{self, Loaded};
use crate::exit::Exit;
use crate::output::{Failure, Report};

/// The longest a single fact's text is shown in text output; JSON output
/// is never shortened beyond the probe's own limits.
const TEXT_FACT_LIMIT: usize = 200;

/// `tocsin probe run`.
pub fn run(check_id: &str, folder: &Path, node: Option<&str>) -> Result<Report, Failure> {
    let Loaded {
        config: configuration,
        ..
    } = config::load(folder, node)?;
    let Some(check) = configuration
        .checks
        .iter()
        .find(|check| check.id.as_str() == check_id)
    else {
        return Err(unknown_check(check_id, &configuration));
    };
    // Without a system root store an HTTP or TLS check ends in a setup
    // error, which fails below; the other checks run regardless.
    let context = ProbeContext::system_or_without_roots();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| Failure::new(Exit::Runtime, format!("cannot start runtime: {error}")))?;
    // The loader has already reported anything that would not compile.
    let compiled =
        CompiledCheck::new(check.clone()).map_err(|error| Failure::new(Exit::Runtime, error))?;
    let outcome = runtime.block_on(tocsin_probe::run(&compiled, &context));

    // A setup error means the probe never reached the target, so it says
    // nothing about the target's health.
    if let Some(error) = &outcome.error
        && error.kind == ProbeErrorKind::Setup
    {
        return Err(Failure::new(
            Exit::Runtime,
            format!("check `{check_id}` could not run: {}", error.message),
        ));
    }
    let json = serde_json::to_value(&outcome).map_err(|error| {
        Failure::new(Exit::Runtime, format!("cannot render the outcome: {error}"))
    })?;
    Ok(Report {
        text: text(&outcome, &json),
        json,
        exit: if outcome.health == Health::Up {
            Exit::Ok
        } else {
            Exit::CheckFailed
        },
    })
}

fn unknown_check(check_id: &str, configuration: &Config) -> Failure {
    let ids = configuration.checks.iter().map(|check| check.id.as_str());
    let hint = match closest(check_id, ids.clone()) {
        Some(near) => format!("did you mean `{near}`?"),
        None if configuration.checks.is_empty() => "the configuration defines no checks".to_owned(),
        None => format!(
            "the configured checks are {}",
            ids.map(|id| format!("`{id}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    Failure::new(
        Exit::InvalidInput,
        format!("no check has the id `{check_id}`; {hint}"),
    )
}

/// The headline, the facts one per line, then each assertion's verdict.
fn text(outcome: &ProbeOutcome, json: &Value) -> String {
    let mut text = format!(
        "check `{}` ({} {}) is {} after {} ms\n",
        outcome.check,
        json["type"].as_str().unwrap_or_default(),
        outcome.target,
        outcome.health.as_str(),
        outcome.duration_ms,
    );
    if let Some(error) = &outcome.error {
        let _ = writeln!(
            text,
            "error ({}): {}",
            json["error"]["kind"].as_str().unwrap_or_default(),
            error.message
        );
    }
    if let Some(facts) = json.get("facts") {
        let mut lines = Vec::new();
        flatten("", facts, &mut lines);
        text.push_str("facts:\n");
        for (name, value) in lines.iter().filter(|(name, _)| name != "type") {
            let _ = writeln!(text, "  {name}: {value}");
        }
    }
    if !outcome.assertions.is_empty() {
        text.push_str("assertions:\n");
    }
    for verdict in &outcome.assertions {
        let _ = write!(
            text,
            "  {} {}{}: expected {}, actual {}",
            if verdict.passed { "pass" } else { "FAIL" },
            verdict.condition,
            if verdict.implicit { " (implicit)" } else { "" },
            shorten(&verdict.expected.to_string()),
            shorten(&verdict.actual.to_string()),
        );
        if !verdict.passed {
            let _ = write!(text, " -> {}", Health::from(verdict.on_fail).as_str());
        }
        if let Some(message) = &verdict.message {
            let _ = write!(text, " ({message})");
        }
        text.push('\n');
    }
    // `emit` adds the final newline.
    text.pop();
    text
}

/// One `name: value` pair per scalar; lists of scalars on one line.
fn flatten(prefix: &str, value: &Value, lines: &mut Vec<(String, String)>) {
    let join = |name: &str| {
        if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}.{name}")
        }
    };
    match value {
        Value::Object(fields) => {
            for (name, field) in fields {
                flatten(&join(name), field, lines);
            }
        }
        Value::Array(items)
            if items
                .iter()
                .all(|item| !item.is_object() && !item.is_array()) =>
        {
            let shown: Vec<String> = items.iter().map(scalar).collect();
            lines.push((
                prefix.to_owned(),
                if shown.is_empty() {
                    "(none)".to_owned()
                } else {
                    shown.join(", ")
                },
            ));
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                flatten(&format!("{prefix}[{index}]"), item, lines);
            }
        }
        scalar_value => lines.push((prefix.to_owned(), scalar(scalar_value))),
    }
}

/// A scalar on one line, shortened to [`TEXT_FACT_LIMIT`] characters.
fn scalar(value: &Value) -> String {
    let Value::String(text) = value else {
        return value.to_string();
    };
    let one_line = text.replace('\r', "\\r").replace('\n', "\\n");
    if one_line.chars().count() <= TEXT_FACT_LIMIT {
        return one_line;
    }
    format!("{} ({} bytes in all)", shorten(&one_line), text.len())
}

/// `text` cut to [`TEXT_FACT_LIMIT`] characters, marked when cut.
fn shorten(text: &str) -> String {
    if text.chars().count() <= TEXT_FACT_LIMIT {
        return text.to_owned();
    }
    let shown: String = text.chars().take(TEXT_FACT_LIMIT).collect();
    format!("{shown}...")
}
