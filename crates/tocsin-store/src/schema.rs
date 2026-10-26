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
    M::up(include_str!("../migrations/0012_incident_internals.sql")),
    M::up(include_str!("../migrations/0013_open_external_key.sql")),
    M::up(include_str!("../migrations/0014_silences.sql")),
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

    /// Existing incidents come through migration 12 with no note and no
    /// key; a key is then unique among open incidents and fixed, and an
    /// empty one is refused.
    #[test]
    fn incidents_before_migration_12_have_no_note_or_key() {
        let mut conn = Connection::open_in_memory().unwrap();
        let migrations = super::migrations();
        migrations.to_version(&mut conn, 11).unwrap();
        conn.execute(
            "INSERT INTO incident (kind, title, impact, status, created_at,
                 created_by_kind, created_by, updated_at, updated_by_kind, updated_by)
                 VALUES ('incident', 'Slow', 'minor', 'investigating', 1000,
                 'token', 'deploy', 1000, 'token', 'deploy')",
            [],
        )
        .unwrap();
        migrations.to_latest(&mut conn).unwrap();
        let internals: (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT internal_note, external_key FROM incident",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(internals, (None, None));
        let keyed = |key: &str| {
            conn.execute(
                "INSERT INTO incident (kind, title, impact, status, created_at,
                     created_by_kind, created_by, updated_at, updated_by_kind, updated_by,
                     external_key)
                     VALUES ('incident', 'Slow', 'minor', 'investigating', 1000,
                     'token', 'deploy', 1000, 'token', 'deploy', ?1)",
                [key],
            )
        };
        keyed("cluster-a").unwrap();
        assert!(keyed("cluster-a").is_err(), "a key was used twice");
        assert!(keyed("").is_err(), "an empty key was stored");
        let moved = conn.execute(
            "UPDATE incident SET external_key = 'cluster-b' WHERE external_key = 'cluster-a'",
            [],
        );
        assert!(moved.is_err(), "a key changed after creation");
        let noted = conn.execute("UPDATE incident SET internal_note = 'evidence'", []);
        assert_eq!(noted.unwrap(), 2, "the note is not settable later");
    }

    /// Since migration 13 a key is unique among open incidents only: a
    /// resolved incident's key is taken again, and reopening it while
    /// another holds the key is refused by the index.
    #[test]
    fn a_resolved_incident_frees_its_key_for_the_index() {
        let mut conn = Connection::open_in_memory().unwrap();
        let migrations = super::migrations();
        migrations.to_version(&mut conn, 12).unwrap();
        let keyed = |conn: &Connection, resolved_at: Option<i64>| {
            conn.execute(
                "INSERT INTO incident (kind, title, impact, status, resolved_at, created_at,
                     created_by_kind, created_by, updated_at, updated_by_kind, updated_by,
                     external_key)
                     VALUES ('incident', 'Slow', 'minor', ?1, ?2, 1000,
                     'token', 'deploy', 1000, 'token', 'deploy', 'cluster-a')",
                rusqlite::params![
                    if resolved_at.is_some() {
                        "resolved"
                    } else {
                        "investigating"
                    },
                    resolved_at
                ],
            )
        };
        keyed(&conn, Some(2000)).unwrap();
        assert!(
            keyed(&conn, None).is_err(),
            "migration 12 freed a resolved key"
        );
        migrations.to_latest(&mut conn).unwrap();
        keyed(&conn, None).unwrap();
        assert!(
            keyed(&conn, None).is_err(),
            "two open incidents share a key"
        );
        keyed(&conn, Some(3000)).unwrap();
        let reopened = conn.execute(
            "UPDATE incident SET status = 'investigating', resolved_at = NULL \
             WHERE resolved_at = 2000",
            [],
        );
        assert!(reopened.is_err(), "a reopen took a key another holds");
    }

    /// Migration 14 rebuilds the outbox: every delivery comes through with
    /// its state, alerts already there count as notified and attempted (so
    /// their resolves and cancels are sent), and a silenced delivery must name its silence.
    #[test]
    fn deliveries_before_migration_14_are_kept() {
        type Delivery = (i64, String, i64, Option<String>, Option<i64>);
        let mut conn = Connection::open_in_memory().unwrap();
        let migrations = super::migrations();
        migrations.to_version(&mut conn, 13).unwrap();
        conn.execute_batch(
            "INSERT INTO outage (id, node_id, check_id, health, started_at)
                 VALUES (1, 'node-a', 'web', 'down', 1000);
             INSERT INTO alert (id, node_id, subject_kind, subject_id, notifier_id, route_id,
                 outage_id, health, started_at)
                 VALUES (1, 'node-a', 'check', 'web', 'pager', 'all', 1, 'down', 1000);
             INSERT INTO alert_delivery (id, alert_id, kind, health, created_at, status,
                 attempts, next_attempt_at, last_error, finished_at)
                 VALUES (5, 1, 'fire', 'down', 1000, 'failed', 3, 1500, 'refused', 2000),
                        (6, 1, 'repeat', 'down', 3000, 'pending', 0, 3000, NULL, NULL);",
        )
        .unwrap();
        migrations.to_latest(&mut conn).unwrap();
        let rows: Vec<Delivery> = conn
            .prepare(
                "SELECT id, status, attempts, last_error, silence_id FROM alert_delivery ORDER BY id",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            [
                (5, "failed".to_owned(), 3, Some("refused".to_owned()), None),
                (6, "pending".to_owned(), 0, None, None),
            ]
        );
        let marks: (i64, i64) = conn
            .query_row("SELECT notified, attempted FROM alert", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(marks, (1, 1));
        let unnamed = conn.execute(
            "UPDATE alert_delivery SET status = 'silenced', finished_at = 4000 WHERE id = 6",
            [],
        );
        assert!(unnamed.is_err(), "a delivery was silenced by no silence");
        let forever = conn.execute(
            "INSERT INTO silence (subject_id, reason, created_at, created_by_kind, created_by,
                 expires_at)
                 VALUES ('web', 'deploying', 1000, 'token', 'ops', 1000)",
            [],
        );
        assert!(forever.is_err(), "a silence did not expire after it began");
        let everything = conn.execute(
            "INSERT INTO silence (reason, created_at, created_by_kind, created_by, expires_at)
                 VALUES ('deploying', 1000, 'token', 'ops', 2000)",
            [],
        );
        assert!(everything.is_err(), "a silence matched every alert");
    }
}
