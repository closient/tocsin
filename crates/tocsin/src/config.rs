// SPDX-License-Identifier: AGPL-3.0-only

//! `tocsin config`: reading a configuration folder, and the `check`, `show`
//! and `schema` commands.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use tocsin_core::config::{self, Config, Diagnostic, Environment, Source};

use crate::exit::Exit;
use crate::output::{Failure, Report};

/// A loaded configuration and the files it came from, in merge order.
pub struct Loaded {
    pub config: Config,
    pub files: Vec<String>,
}

/// Reads, merges and validates every `*.toml` file under `folder`.
pub fn load(folder: &Path) -> Result<Loaded, Failure> {
    if !folder.is_dir() {
        return Err(Failure::new(
            Exit::InvalidInput,
            format!("config folder {} is not a directory", folder.display()),
        ));
    }
    let mut paths = Vec::new();
    find_toml_files(folder, Path::new(""), &mut paths).map_err(|error| {
        Failure::new(
            Exit::Runtime,
            format!("cannot read config folder {}: {error}", folder.display()),
        )
    })?;
    paths.sort();

    let mut sources = Vec::new();
    let mut unreadable = Vec::new();
    for relative in paths {
        let name = folder.join(&relative).display().to_string();
        match std::fs::read(folder.join(&relative)) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => sources.push(Source { name, text }),
                Err(_) => unreadable.push(Diagnostic::in_file(name, "the file is not UTF-8 text")),
            },
            Err(error) => unreadable.push(Diagnostic::in_file(
                name,
                format!("cannot read the file: {error}"),
            )),
        }
    }
    let files: Vec<String> = sources.iter().map(|source| source.name.clone()).collect();

    let variable = |name: &str| std::env::var(name).ok();
    let environment = Environment {
        hostname: gethostname::gethostname().to_string_lossy().into_owned(),
        variable: &variable,
    };
    let result = config::load(&sources, &environment);
    match result {
        Ok(config) if unreadable.is_empty() => Ok(Loaded { config, files }),
        Ok(_) => Err(invalid(folder, unreadable)),
        Err(mut errors) => {
            errors.extend(unreadable);
            Err(invalid(folder, errors))
        }
    }
}

fn invalid(folder: &Path, errors: Vec<Diagnostic>) -> Failure {
    let count = errors.len();
    let noun = if count == 1 { "error" } else { "errors" };
    Failure::invalid_config(
        format!("configuration in {} has {count} {noun}", folder.display()),
        errors,
    )
}

/// Collects the paths, relative to `root`, of every `*.toml` file below it.
///
/// Hidden entries are skipped, as editors and tools keep state in them.
/// Symlinked files are read; symlinked folders are not followed, so a link
/// cannot loop or pull in a tree from elsewhere.
fn find_toml_files(root: &Path, relative: &Path, found: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(root.join(relative))? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let path = relative.join(&name);
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            find_toml_files(root, &path, found)?;
        } else if Path::new(&name)
            .extension()
            .is_some_and(|extension| extension == "toml")
            && (file_type.is_file() || root.join(&path).is_file())
        {
            found.push(path);
        }
    }
    Ok(())
}

/// `tocsin config check`.
pub fn check(folder: &Path) -> Result<Report, Failure> {
    let Loaded { config, files } = load(folder)?;
    let counts = counts(&config);
    let summary = counts
        .iter()
        .map(|(kind, count)| {
            let plural = if count == 1 { "" } else { "s" };
            format!("{count} {kind}{plural}")
        })
        .collect::<Vec<_>>()
        .join(", ");
    let file_noun = if files.len() == 1 { "file" } else { "files" };
    Ok(Report {
        text: format!(
            "configuration in {} is valid: {} {file_noun}; {summary}",
            folder.display(),
            files.len()
        ),
        json: json!({
            "ok": true,
            "files": files,
            "counts": counts.into_iter().collect::<serde_json::Map<String, Value>>(),
        }),
    })
}

fn counts(config: &Config) -> Vec<(String, Value)> {
    [
        ("component", config.components.len()),
        ("group", config.groups.len()),
        ("check", config.checks.len()),
        ("heartbeat", config.heartbeats.len()),
        ("project", config.projects.len()),
        ("notifier", config.notifiers.len()),
        ("route", config.routes.len()),
        ("token", config.tokens.len()),
    ]
    .into_iter()
    .map(|(kind, count)| (kind.to_owned(), Value::from(count)))
    .collect()
}

/// `tocsin config show`: the merged configuration, secrets redacted.
pub fn show(folder: &Path) -> Result<Report, Failure> {
    let Loaded { config, .. } = load(folder)?;
    let json = serde_json::to_value(&config).map_err(|error| {
        Failure::new(
            Exit::Runtime,
            format!("cannot render the configuration: {error}"),
        )
    })?;
    Ok(Report {
        text: pretty(&json),
        json,
    })
}

/// `tocsin config schema`.
pub fn schema() -> Report {
    let json = config::schema();
    Report {
        text: pretty(&json),
        json,
    }
}

fn pretty(json: &Value) -> String {
    serde_json::to_string_pretty(json).unwrap_or_else(|_| json.to_string())
}
