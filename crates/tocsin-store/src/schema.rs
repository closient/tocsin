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
    M::up(include_str!("../migrations/0006_heartbeats.sql")),
    M::up(include_str!("../migrations/0007_read_indexes.sql")),
    M::up(include_str!("../migrations/0008_outage_ending.sql")),
    M::up(include_str!("../migrations/0009_subscribers.sql")),
    M::up(include_str!("../migrations/0010_slack.sql")),
    M::up(include_str!("../migrations/0011_incident_drafts.sql")),
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
    // connection, so no commit runs a checkpoint's copy or its fsyncs.
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
    use rusqlite::Connection;

    #[test]
    fn migrations_are_valid() {
        super::migrations().validate().unwrap();
    }

    /// Before migration 8 only a recovery ended an outage, so each one
    /// already ended reads back recovered; an open one has no ending yet.
    #[test]
    fn outages_ended_before_migration_8_ended_recovered() {
        let mut conn = Connection::open_in_memory().unwrap();
        let migrations = super::migrations();
        migrations.to_version(&mut conn, 7).unwrap();
        conn.execute_batch(
            "INSERT INTO outage (node_id, check_id, health, started_at, ended_at)
                 VALUES ('node-a', 'web', 'down', 1000, 2000);
             INSERT INTO outage (node_id, check_id, health, started_at)
                 VALUES ('node-a', 'api', 'degraded', 1000);",
        )
        .unwrap();
        migrations.to_latest(&mut conn).unwrap();
        let endings: Vec<(String, Option<String>)> = conn
            .prepare("SELECT check_id, ending FROM outage ORDER BY check_id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            endings,
            [
                ("api".to_owned(), None),
                ("web".to_owned(), Some("recovered".to_owned()))
            ]
        );
    }

    #[test]
    fn incidents_before_migration_11_are_published_by_their_creator() {
        let mut conn = Connection::open_in_memory().unwrap();
        let migrations = super::migrations();
        migrations.to_version(&mut conn, 10).unwrap();
        conn.execute_batch(
            "INSERT INTO incident (kind, title, impact, status, created_at,
                 created_by_kind, created_by, updated_at, updated_by_kind, updated_by)
                 VALUES ('incident', 'Slow', 'minor', 'investigating', 1000,
                 'token', 'deploy', 2000, 'person', 'ops@example.com');",
        )
        .unwrap();
        migrations.to_latest(&mut conn).unwrap();
        let published: (Option<i64>, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT published_at, published_by_kind, published_by FROM incident",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            published,
            (
                Some(1000),
                Some("token".to_owned()),
                Some("deploy".to_owned())
            )
        );
        let unpublish = conn.execute(
            "UPDATE incident SET published_at = NULL, published_by_kind = NULL, published_by = NULL",
            [],
        );
        assert!(
            unpublish.is_err(),
            "a published incident went back to draft"
        );
        let half = conn.execute(
            "INSERT INTO incident (kind, title, impact, status, created_at,
                 created_by_kind, created_by, updated_at, updated_by_kind, updated_by, published_at)
                 VALUES ('incident', 'Half', 'minor', 'investigating', 1000,
                 'token', 'deploy', 1000, 'token', 'deploy', 1000)",
            [],
        );
        assert!(half.is_err(), "published_at was set without published_by");
    }
}
