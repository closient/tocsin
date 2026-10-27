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

/// Reads, merges and validates every `*.toml` file under `folder`, for the
/// node `node` names (`--node` or `TOCSIN_NODE_ID`) when it names one.
pub fn load(folder: &Path, node: Option<&str>) -> Result<Loaded, Failure> {
    load_with(folder, node, &|name| std::env::var(name).ok())
}

/// [`load`], reading `${ENV:NAME}` secrets through `variable`.
pub fn load_with(
    folder: &Path,
    node: Option<&str>,
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
        node_id: node.map(str::to_owned),
        variable,
    };
    let config = config::load(&sources, &environment).map_err(|errors| invalid(folder, errors))?;
    // `[telemetry.otlp]` is only whole with the `OTEL_*` variables it
    // leaves to the environment.
    crate::otlp::check(&config, variable).map_err(|message| {
        invalid(
            folder,
            vec![Diagnostic {
                file: None,
                line: None,
                column: None,
                path: Some("telemetry.otlp".to_owned()),
                message,
                hint: Some(
                    "the export's settings come from `[telemetry.otlp]` and the OTEL_EXPORTER_OTLP_* \
                     variables; docs/metrics.md says which wins"
                        .to_owned(),
                ),
            }],
        )
    })?;
    Ok(Loaded { config, files })
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
///
/// A notifier naming a Slack `connection` that is not connected, or that
/// Slack revoked, is a warning, not an error: the channel is connected on
/// the admin page of a running server, after the configuration is valid.
/// The connections are read from the database in `data`, or in `[server]
/// data_dir`, without writing to it. So is `[telemetry.otlp]` that
/// `OTEL_SDK_DISABLED` turns off.
pub fn check(folder: &Path, node: Option<&str>, data: Option<&Path>) -> Result<Report, Failure> {
    let Loaded { config, files } = load(folder, node)?;
    let mut warnings = slack_warnings(&config, folder, data);
    if crate::otlp::disabled(&config, &|name| std::env::var(name).ok()) {
        warnings.push(crate::otlp::DISABLED.to_owned());
    }
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
    let mut text = format!(
        "configuration in {} is valid: {} {file_noun}; {summary}",
        folder.display(),
        files.len()
    );
    for warning in &warnings {
        let _ = write!(text, "\nwarning: {warning}");
    }
    Ok(Report {
        text,
        exit: Exit::Ok,
        json: json!({
            "ok": true,
            "files": files,
            "counts": counts.into_iter().collect::<serde_json::Map<String, Value>>(),
            "warnings": warnings,
        }),
    })
}

/// What is wrong with the Slack connections the notifiers name: each one
/// not connected, or dead, in the database. Nothing when no notifier names
/// one.
fn slack_warnings(config: &Config, folder: &Path, data: Option<&Path>) -> Vec<String> {
    let named: Vec<(&str, &str)> = config
        .notifiers
        .iter()
        .filter_map(|notifier| {
            let connection = notifier.slack.as_ref()?.connection.as_ref()?;
            Some((notifier.id.as_str(), connection.as_str()))
        })
        .collect();
    if named.is_empty() {
        return Vec::new();
    }
    let mut warnings = Vec::new();
    if config.slack.is_none() {
        warnings.push(
            "notifiers name Slack connections, but without `[slack]` none can be connected"
                .to_owned(),
        );
    }
    let database = data
        .map(Path::to_path_buf)
        .or_else(|| {
            config
                .server
                .data_dir
                .as_ref()
                .map(|data_dir| folder.join(data_dir))
        })
        .map(|folder| folder.join(tocsin_store::DB_FILE));
    let connections = match database {
        None => {
            warnings.push(
                "cannot check the Slack connections notifiers name: no data folder; pass --data or set `data_dir` in `[server]`"
                    .to_owned(),
            );
            return warnings;
        }
        Some(path) if !path.is_file() => Vec::new(),
        Some(path) => match tocsin_store::inspect_slack(&path) {
            Ok(connections) => connections.unwrap_or_default(),
            Err(error) => {
                warnings.push(format!(
                    "cannot check the Slack connections notifiers name: cannot read {}: {error}",
                    path.display()
                ));
                return warnings;
            }
        },
    };
    for (notifier, name) in named {
        match connections.iter().find(|(known, _)| known == name) {
            None => warnings.push(format!(
                "notifier `{notifier}` names Slack connection `{name}`, which is not connected; its alerts fail until it is connected on /admin/slack"
            )),
            Some((_, false)) => warnings.push(format!(
                "notifier `{notifier}` names Slack connection `{name}`, which Slack revoked; its alerts fail until it is connected again on /admin/slack"
            )),
            Some((_, true)) => {}
        }
    }
    warnings
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
pub fn show(folder: &Path, node: Option<&str>) -> Result<Report, Failure> {
    let Loaded { config, .. } = load(folder, node)?;
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
