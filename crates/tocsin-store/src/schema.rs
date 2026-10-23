// SPDX-License-Identifier: AGPL-3.0-only

//! Migrations and per-connection settings.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use rusqlite_migration::{M, Migrations};

use crate::checkpoint;
use crate::error::{Error, Result};

/// Every migration, oldest first. Append only: a shipped migration is never
/// edited, because databases in the field have already run it.
static MIGRATIONS: &[M<'static>] = &[
    M::up(include_str!("../migrations/0001_history.sql")),
    M::up(include_str!("../migrations/0002_incidents.sql")),
    M::up(include_str!("../migrations/0003_outage_alerted.sql")),
    M::up(include_str!("../migrations/0004_releases.sql")),
    M::up(include_str!("../migrations/0005_alerting.sql")),
];

/// How long a connection waits for another process's lock before failing.
pub(crate) const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The migrations, for running and validation.
pub(crate) fn migrations() -> Migrations<'static> {
    Migrations::from_slice(MIGRATIONS)
}

/// The schema version a fully migrated database has.
pub(crate) fn latest_version() -> usize {
    MIGRATIONS.len()
}

/// Opens the write connection: switches to WAL and migrates to the latest
/// schema.
pub(crate) fn open_writer(path: &Path) -> Result<Connection> {
    let mut conn = Connection::open(path)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    let mode: String =
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(Error::NotWal(mode));
    }
    // In WAL mode NORMAL is durable against application crashes; a power loss
    // can drop the last commits but never corrupts the database.
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // Commits never checkpoint: the checkpointer thread does, on its own
    // connection, so a checkpoint's fsyncs never hold up a write.
    // The pragma replaces any WAL hook, so it comes first.
    conn.pragma_update(None, "wal_autocheckpoint", 0)?;
    checkpoint::watch(&conn);
    conn.pragma_update(None, "journal_size_limit", checkpoint::LOG_LIMIT_BYTES)?;
    migrations().to_latest(&mut conn)?;
    Ok(conn)
}

/// Opens the checkpointer's connection. It only ever runs `PRAGMA
/// wal_checkpoint`, which works with writes refused.
pub(crate) fn open_checkpointer(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "query_only", "ON")?;
    Ok(conn)
}

/// Opens a read connection. It refuses writes, so a bug cannot make it a
/// second writer.
pub(crate) fn open_reader(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "query_only", "ON")?;
    Ok(conn)
}

/// Opens a connection that cannot write at all, for looking at a database
/// this build may not own: no WAL switch, no migrations.
pub(crate) fn open_inspector(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    #[test]
    fn migrations_are_valid() {
        super::migrations().validate().unwrap();
    }
}
