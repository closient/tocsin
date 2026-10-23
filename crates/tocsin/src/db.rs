// SPDX-License-Identifier: AGPL-3.0-only

//! `tocsin db`: inspect and maintain the SQLite database.

use std::fmt::Write;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use tocsin_store::{DB_FILE, Day, Store, day};

use crate::cli::DbCommand;
use crate::exit::Exit;
use crate::lock;
use crate::output::{Failure, Report};

pub fn run(command: DbCommand) -> Result<Report, Failure> {
    match command {
        DbCommand::Info { data } => info(&existing_database(&data)?),
        DbCommand::Vacuum { data } => {
            // Held through the vacuum, so a server cannot start mid-rebuild.
            let Some(_lock) = lock::try_take(&data)? else {
                return Err(Failure::new(
                    Exit::InvalidInput,
                    format!(
                        "tocsin serve is running on {}; stop it before vacuuming, which would hold the database's write lock for the whole rebuild",
                        data.display()
                    ),
                ));
            };
            with_store(&data, vacuum)
        }
        DbCommand::Rollup { data, since } => {
            let since: Day = since.parse().map_err(|error| {
                Failure::new(Exit::InvalidInput, format!("--since {since:?}: {error}"))
            })?;
            with_store(&data, async move |store: &Store| rollup(store, since).await)
        }
    }
}

/// The database under `data`. A missing database is the caller's mistake,
/// not ours: these commands never create one.
fn existing_database(data: &Path) -> Result<PathBuf, Failure> {
    let path = data.join(DB_FILE);
    if path.is_file() {
        return Ok(path);
    }
    Err(Failure::new(
        Exit::InvalidInput,
        format!(
            "no database at {}; `tocsin serve --data {}` creates one",
            path.display(),
            data.display()
        ),
    ))
}

/// Opens the existing database under `data` (migrating it, like the server
/// would), runs `action` on it, and closes it.
fn with_store(
    data: &Path,
    action: impl AsyncFnOnce(&Store) -> Result<Report, Failure>,
) -> Result<Report, Failure> {
    let path = existing_database(data)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| Failure::new(Exit::Runtime, format!("cannot start runtime: {error}")))?;
    runtime.block_on(async {
        let store = Store::open(&path).map_err(|error| {
            Failure::new(
                Exit::Runtime,
                format!("cannot open {}: {error}", path.display()),
            )
        })?;
        let report = action(&store).await;
        let closed = store.close().await;
        let report = report?;
        closed.map_err(runtime_failure)?;
        Ok(report)
    })
}

#[allow(clippy::needless_pass_by_value)] // shaped for `map_err`
fn runtime_failure(error: tocsin_store::Error) -> Failure {
    Failure::new(Exit::Runtime, error.to_string())
}

/// A timestamp as both unix milliseconds and RFC 3339, or null.
fn instant(ms: Option<i64>) -> Value {
    ms.map_or(
        Value::Null,
        |ms| json!({ "ms": ms, "utc": day::rfc3339(ms) }),
    )
}

/// Describes the database read-only: no migrations, so `db info` works on a
/// database a newer build wrote and never changes the one it looks at.
fn info(path: &Path) -> Result<Report, Failure> {
    let info = tocsin_store::inspect(path).map_err(|error| {
        Failure::new(
            Exit::Runtime,
            format!("cannot read {}: {error}", path.display()),
        )
    })?;
    let supported = tocsin_store::latest_schema_version();
    let newer = usize::try_from(info.schema_version).is_ok_and(|version| version > supported);
    let rows: Map<String, Value> = info
        .rows
        .iter()
        .map(|(table, count)| ((*table).to_owned(), json!(count)))
        .collect();
    let mut text = format!(
        "path: {}\nsize: {} bytes\nschema version: {}{}\noldest result: {}\nnewest result: {}\nraw results purged before: {}\nrows:",
        info.path.display(),
        info.size_bytes,
        info.schema_version,
        if newer {
            format!(" (newer than this build's {supported}; upgrade tocsin)")
        } else {
            String::new()
        },
        info.oldest_result_ms
            .map_or("none".to_owned(), day::rfc3339),
        info.newest_result_ms
            .map_or("none".to_owned(), day::rfc3339),
        info.purged_before_ms
            .map_or("never".to_owned(), day::rfc3339),
    );
    for (table, count) in &info.rows {
        let _ = write!(text, "\n  {table}: {count}");
    }
    Ok(Report {
        exit: Exit::Ok,
        json: json!({
            "path": info.path.display().to_string(),
            "size_bytes": info.size_bytes,
            "schema_version": info.schema_version,
            "supported_schema_version": supported,
            "rows": rows,
            "oldest_result": instant(info.oldest_result_ms),
            "newest_result": instant(info.newest_result_ms),
            "purged_before": instant(info.purged_before_ms),
        }),
        text,
    })
}

async fn vacuum(store: &Store) -> Result<Report, Failure> {
    let report = store.vacuum().await.map_err(runtime_failure)?;
    Ok(Report {
        exit: Exit::Ok,
        json: json!({
            "size_before_bytes": report.size_before,
            "size_after_bytes": report.size_after,
        }),
        text: format!(
            "vacuumed {}: {} -> {} bytes",
            store.path().display(),
            report.size_before,
            report.size_after
        ),
    })
}

async fn rollup(store: &Store, since: Day) -> Result<Report, Failure> {
    let report = store.rollup_since(since).await.map_err(runtime_failure)?;
    // `since` comes back later than asked when retention has already
    // finalized the earlier days; None means nothing was left to recompute.
    let effective = report.since.map(|day| day.to_string());
    Ok(Report {
        exit: Exit::Ok,
        json: json!({
            "requested_since": since.to_string(),
            "since": effective,
            "rows": report.rows,
        }),
        text: match &effective {
            Some(day) => format!("recomputed {} daily rollup rows from {day}", report.rows),
            None => format!("no raw results on or after {since} to roll up"),
        },
    })
}
