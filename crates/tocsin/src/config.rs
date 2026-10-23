// SPDX-License-Identifier: AGPL-3.0-only

//! `tocsin config`: reading a configuration folder, the `check`, `show`
//! and `schema` commands, and `status` for a running server.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use tocsin_core::config::{self, Config, Diagnostic, Environment, Source};

use crate::cli::ServerArgs;
use crate::client::Client;
use crate::exit::Exit;
use crate::output::{Failure, Report};
use crate::reload::ConfigStatus;

/// A loaded configuration and the files it came from, in merge order.
pub struct Loaded {
    pub config: Config,
    pub files: Vec<String>,
}

/// Reads, merges and validates every `*.toml` file under `folder`.
pub fn load(folder: &Path) -> Result<Loaded, Failure> {
    load_with(folder, &|name| std::env::var(name).ok())
}

/// [`load`], reading `${ENV:NAME}` secrets through `variable`.
pub fn load_with(
    folder: &Path,
    variable: &dyn Fn(&str) -> Option<String>,
) -> Result<Loaded, Failure> {
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
    if paths.is_empty() {
        let diagnostic = Diagnostic {
            file: None,
            line: None,
            column: None,
            path: None,
            message: "the folder holds no `*.toml` files".to_owned(),
            hint: Some(
                "add at least one, such as server.toml with a `[server]` table; docs/config.md describes every table"
                    .to_owned(),
            ),
        };
        return Err(Failure::invalid_config(
            format!("configuration in {} is empty", folder.display()),
            vec![diagnostic],
        ));
    }

    let sources: Vec<Source> = paths
        .into_iter()
        .map(|relative| {
            let path = folder.join(relative);
            let text = std::fs::read(&path)
                .map_err(|error| format!("cannot read the file: {error}"))
                .and_then(|bytes| {
                    String::from_utf8(bytes).map_err(|_| "the file is not UTF-8 text".to_owned())
                });
            Source {
                name: path.display().to_string(),
                text,
            }
        })
        .collect();
    let files: Vec<String> = sources.iter().map(|source| source.name.clone()).collect();

    let environment = Environment {
        hostname: gethostname::gethostname().to_string_lossy().into_owned(),
        variable,
    };
    match config::load(&sources, &environment) {
        Ok(config) => Ok(Loaded { config, files }),
        Err(errors) => Err(invalid(folder, errors)),
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
    let counts = counts(&config)?;
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
        exit: Exit::Ok,
        json: json!({
            "ok": true,
            "files": files,
            "counts": counts.into_iter().collect::<serde_json::Map<String, Value>>(),
        }),
    })
}

/// How many items of each `[[kind]]` there are, read from the serialized
/// configuration so a new kind is counted without being listed here.
fn counts(config: &Config) -> Result<Vec<(String, Value)>, Failure> {
    let Value::Object(tables) = render(config)? else {
        return Ok(Vec::new());
    };
    Ok(tables
        .into_iter()
        .filter_map(|(kind, value)| match value {
            Value::Array(items) => Some((kind, Value::from(items.len()))),
            _ => None,
        })
        .collect())
}

fn render(config: &Config) -> Result<Value, Failure> {
    serde_json::to_value(config).map_err(|error| {
        Failure::new(
            Exit::Runtime,
            format!("cannot render the configuration: {error}"),
        )
    })
}

/// `tocsin config show`: the merged configuration, secrets redacted.
pub fn show(folder: &Path) -> Result<Report, Failure> {
    let Loaded { config, .. } = load(folder)?;
    let json = render(&config)?;
    Ok(Report {
        text: pretty(&json),
        json,
        exit: Exit::Ok,
    })
}

/// `tocsin config schema`.
pub fn schema() -> Report {
    let json = config::schema();
    Report {
        text: pretty(&json),
        json,
        exit: Exit::Ok,
    }
}

/// `tocsin config status`: whether a running server's configuration last
/// reloaded, exiting [`Exit::CheckFailed`] when it did not.
pub fn status(server: &ServerArgs) -> Result<Report, Failure> {
    let status: ConfigStatus = Client::new(server)?.get_with_token("api/v1/config/status")?;
    let mut text = if status.ok {
        format!(
            "configuration loaded at {}; the last reload succeeded",
            status.loaded_at
        )
    } else {
        format!(
            "the reload at {} failed; the configuration loaded at {} is still running",
            status.last_attempt_at, status.loaded_at
        )
    };
    for diagnostic in &status.errors {
        let _ = write!(text, "\n{diagnostic}");
    }
    Ok(Report {
        exit: if status.ok {
            Exit::Ok
        } else {
            Exit::CheckFailed
        },
        json: json!(status),
        text,
    })
}

fn pretty(json: &Value) -> String {
    serde_json::to_string_pretty(json).unwrap_or_else(|_| json.to_string())
}
