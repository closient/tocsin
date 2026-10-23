// SPDX-License-Identifier: AGPL-3.0-only

//! The store's error type.

use std::fmt;

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
    /// SQLite would not switch the database to write-ahead logging, for
    /// example because the filesystem cannot share memory between processes.
    NotWal(String),
    /// The writer has stopped, so the store is closed.
    Closed,
    /// A read panicked; its connection was discarded.
    ReaderPanicked,
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
            Error::NotWal(mode) => write!(
                f,
                "the database could not use write-ahead logging (journal mode is {mode})"
            ),
            Error::Closed => f.write_str("the store is closed"),
            Error::ReaderPanicked => f.write_str("a database read panicked"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Sqlite(error) => Some(error),
            Error::Migration(error) => Some(error),
            Error::Io(error) => Some(error),
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
