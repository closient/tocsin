// SPDX-License-Identifier: AGPL-3.0-only

//! The store's error type.

use std::fmt;

use tocsin_core::incident::FieldError;

/// Anything that stopped a store operation.
#[derive(Debug)]
pub enum Error {
    /// SQLite rejected a statement, including a constraint or trigger.
    Sqlite(rusqlite::Error),
    /// The schema could not be brought to the latest version.
    Migration(rusqlite_migration::Error),
    /// Reading the database files from disk failed.
    Io(std::io::Error),
    /// The transaction a write was batched into could not commit.
    Batch(String),
    /// An incident edit could not be encoded as JSON for the edit log.
    Encode(serde_json::Error),
    /// SQLite would not switch the database to write-ahead logging, for
    /// example because the filesystem cannot share memory between processes.
    NotWal(String),
    /// The writer has stopped, so the store is closed.
    Closed,
    /// A read panicked; its connection was replaced.
    ReaderPanicked,
    /// A write panicked; its transaction was rolled back and the writer
    /// carried on.
    WriterPanicked,
    /// The writer thread itself panicked, outside any one write, while the
    /// store was closing.
    WriterThreadPanicked,
    /// The checkpointer thread panicked. Writes went on, checkpointing the
    /// log themselves, but the panic is a bug.
    CheckpointerPanicked,
    /// A check result starts more than [`crate::FUTURE_TOLERANCE`] after
    /// now, too far to be clock skew between nodes; storing it would give a
    /// day that has not happened a rollup.
    FutureResult {
        /// When the result says it started (unix milliseconds).
        started_at_ms: i64,
        /// This node's clock when it was refused.
        now_ms: i64,
    },
    /// A write broke a rule that depends on what is already stored, such as
    /// an update whose status does not suit the incident's kind.
    Invalid(Vec<FieldError>),
    /// The incident with this id is published, and the write is one only a
    /// draft takes: publishing, discarding, or a drafts-only writer's
    /// change.
    Published(i64),
}

/// The result of a store operation.
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Sqlite(error) => write!(f, "database: {error}"),
            Error::Migration(error) => write!(f, "migrating the database: {error}"),
            Error::Io(error) => write!(f, "database file: {error}"),
            Error::Batch(message) => write!(f, "committing a write batch: {message}"),
            Error::Encode(error) => write!(f, "encoding the edit log entry: {error}"),
            Error::NotWal(mode) => write!(
                f,
                "the database could not use write-ahead logging (journal mode is {mode})"
            ),
            Error::Closed => f.write_str("the store is closed"),
            Error::ReaderPanicked => f.write_str("a database read panicked"),
            Error::WriterPanicked => f.write_str("a database write panicked and was rolled back"),
            Error::WriterThreadPanicked => f.write_str("the database writer thread panicked"),
            Error::CheckpointerPanicked => f.write_str("the database checkpointer thread panicked"),
            Error::FutureResult {
                started_at_ms,
                now_ms,
            } => write!(
                f,
                "a check result starts at {started_at_ms} ms, more than an hour after now \
                 ({now_ms} ms); check the clock of the node that ran it"
            ),
            Error::Published(id) => write!(f, "incident {id} is already published"),
            Error::Invalid(errors) => {
                f.write_str("invalid write")?;
                for (index, error) in errors.iter().enumerate() {
                    f.write_str(if index == 0 { ": " } else { "; " })?;
                    write!(f, "{error}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Sqlite(error) => Some(error),
            Error::Migration(error) => Some(error),
            Error::Io(error) => Some(error),
            Error::Encode(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        Error::Sqlite(error)
    }
}

impl From<rusqlite_migration::Error> for Error {
    fn from(error: rusqlite_migration::Error) -> Self {
        Error::Migration(error)
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Error::Io(error)
    }
}
