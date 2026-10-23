-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Migration 1: the history schema. Shipped migrations are never edited;
-- change the schema by adding the next file. See docs/storage.md.
--
-- Every timestamp is INTEGER unix milliseconds, UTC. Every enum is TEXT
-- with a CHECK constraint. No JSON column is ever filtered on.

-- One row per probe run. Append-only: the trigger below rejects UPDATE;
-- DELETE stays open for retention.
-- AUTOINCREMENT: ids are never reused, so "id > n" means "committed after
-- a reader saw max(id) = n"; retention relies on that.
CREATE TABLE check_result (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    node_id     TEXT    NOT NULL CHECK (node_id <> ''),
    check_id    TEXT    NOT NULL CHECK (check_id <> ''),
    started_at  INTEGER NOT NULL CHECK (started_at >= 0),
    duration_ms INTEGER NOT NULL CHECK (duration_ms >= 0),
    health      TEXT    NOT NULL CHECK (health IN ('up', 'degraded', 'down')),
    -- Diagnostic only: shown to people, never queried or filtered on.
    detail      TEXT    CHECK (detail IS NULL OR json_valid(detail)),
    error       TEXT,
    status_code INTEGER CHECK (status_code IS NULL OR status_code BETWEEN 0 AND 999)
) STRICT;

-- History of one check (status page, probe explain).
CREATE INDEX check_result_by_check ON check_result (check_id, started_at);
-- Everything one node reported (federation).
CREATE INDEX check_result_by_node ON check_result (node_id, started_at);
-- Retention and rollups scan by time; covering, so neither touches the table.
CREATE INDEX check_result_by_time ON check_result (started_at, check_id, node_id, health);

CREATE TRIGGER check_result_append_only
BEFORE UPDATE ON check_result
BEGIN
    SELECT RAISE(ABORT, 'check_result is append-only');
END;

-- A machine-detected outage of one check on one node. The only change ever
-- made to a row is setting ended_at, once.
CREATE TABLE outage (
    id           INTEGER PRIMARY KEY,
    node_id      TEXT    NOT NULL CHECK (node_id <> ''),
    check_id     TEXT    NOT NULL CHECK (check_id <> ''),
    component_id TEXT    CHECK (component_id IS NULL OR component_id <> ''),
    health       TEXT    NOT NULL CHECK (health IN ('degraded', 'down')),
    started_at   INTEGER NOT NULL CHECK (started_at >= 0),
    ended_at     INTEGER CHECK (ended_at IS NULL OR ended_at >= started_at)
) STRICT;

CREATE INDEX outage_by_check ON outage (check_id, started_at);
CREATE INDEX outage_by_component ON outage (component_id, started_at)
    WHERE component_id IS NOT NULL;
-- At most one open outage per check per node.
CREATE UNIQUE INDEX outage_open ON outage (check_id, node_id) WHERE ended_at IS NULL;

CREATE TRIGGER outage_only_ends
BEFORE UPDATE ON outage
WHEN OLD.ended_at IS NOT NULL
    OR NEW.ended_at IS NULL
    OR NEW.id IS NOT OLD.id
    OR NEW.node_id IS NOT OLD.node_id
    OR NEW.check_id IS NOT OLD.check_id
    OR NEW.component_id IS NOT OLD.component_id
    OR NEW.health IS NOT OLD.health
    OR NEW.started_at IS NOT OLD.started_at
BEGIN
    SELECT RAISE(ABORT, 'an outage can only have ended_at set, once');
END;

-- Per UTC day counts of check_result, kept indefinitely; the 90-day bar is
-- drawn from here. Recomputable from check_result for any day not yet
-- purged by retention.
CREATE TABLE daily_rollup (
    check_id TEXT    NOT NULL CHECK (check_id <> ''),
    node_id  TEXT    NOT NULL CHECK (node_id <> ''),
    -- UTC day as YYYY-MM-DD.
    date     TEXT    NOT NULL CHECK (date(date) IS date),
    total    INTEGER NOT NULL CHECK (total = up + degraded + down),
    up       INTEGER NOT NULL CHECK (up >= 0),
    degraded INTEGER NOT NULL CHECK (degraded >= 0),
    down     INTEGER NOT NULL CHECK (down >= 0),
    worst    TEXT    NOT NULL CHECK (worst IN ('up', 'degraded', 'down')),
    PRIMARY KEY (check_id, node_id, date)
) STRICT, WITHOUT ROWID;

-- Single row: check_result rows started before purged_before (a UTC day
-- boundary) are rolled up and being, or already, deleted. Days before it are
-- final in daily_rollup and never recomputed.
CREATE TABLE retention (
    id            INTEGER PRIMARY KEY CHECK (id = 1),
    purged_before INTEGER NOT NULL CHECK (purged_before >= 0 AND purged_before % 86400000 = 0)
) STRICT;

-- A result for a day retention has finalized would be deleted without ever
-- being counted in its rollup, so it is refused outright.
CREATE TRIGGER check_result_not_before_retention
BEFORE INSERT ON check_result
WHEN NEW.started_at < (SELECT purged_before FROM retention WHERE id = 1)
BEGIN
    SELECT RAISE(ABORT, 'check_result: started_at is before the retention watermark; that day is final');
END;

-- A push heartbeat received for a check. Append-only. Shape owned by the
-- heartbeat ticket, which extends it with a new migration.
CREATE TABLE heartbeat_ping (
    id          INTEGER PRIMARY KEY,
    node_id     TEXT    NOT NULL CHECK (node_id <> ''),
    check_id    TEXT    NOT NULL CHECK (check_id <> ''),
    received_at INTEGER NOT NULL CHECK (received_at >= 0),
    -- Remote address as the node saw it, for diagnosis.
    source      TEXT
) STRICT;

CREATE INDEX heartbeat_ping_by_check ON heartbeat_ping (check_id, received_at);

CREATE TRIGGER heartbeat_ping_append_only
BEFORE UPDATE ON heartbeat_ping
BEGIN
    SELECT RAISE(ABORT, 'heartbeat_ping is append-only');
END;

-- A human-declared incident or scheduled maintenance. Shape owned by the
-- incidents ticket.
CREATE TABLE incident (
    id          INTEGER PRIMARY KEY,
    kind        TEXT    NOT NULL CHECK (kind IN ('incident', 'maintenance')),
    title       TEXT    NOT NULL CHECK (title <> ''),
    impact      TEXT    NOT NULL CHECK (impact IN ('none', 'degraded', 'down')),
    -- When it began, or for maintenance when it is scheduled to begin.
    starts_at   INTEGER NOT NULL CHECK (starts_at >= 0),
    -- Scheduled end of a maintenance; NULL for an incident.
    ends_at     INTEGER CHECK (ends_at IS NULL OR ends_at >= starts_at),
    resolved_at INTEGER CHECK (resolved_at IS NULL OR resolved_at >= 0),
    created_at  INTEGER NOT NULL CHECK (created_at >= 0)
) STRICT;

CREATE INDEX incident_by_start ON incident (starts_at);

CREATE TABLE incident_update (
    id          INTEGER PRIMARY KEY,
    incident_id INTEGER NOT NULL REFERENCES incident (id) ON DELETE CASCADE,
    status      TEXT    NOT NULL CHECK (status IN (
        'investigating', 'identified', 'monitoring', 'resolved',
        'scheduled', 'in_progress', 'completed'
    )),
    -- Markdown.
    body        TEXT    NOT NULL,
    created_at  INTEGER NOT NULL CHECK (created_at >= 0)
) STRICT;

CREATE INDEX incident_update_by_incident ON incident_update (incident_id, created_at);

-- A release or deploy pushed by CI. Shape owned by the release-log ticket.
CREATE TABLE release (
    id           INTEGER PRIMARY KEY,
    version      TEXT    NOT NULL CHECK (version <> ''),
    component_id TEXT    CHECK (component_id IS NULL OR component_id <> ''),
    environment  TEXT    CHECK (environment IS NULL OR environment <> ''),
    -- Markdown.
    notes        TEXT,
    deployed_at  INTEGER NOT NULL CHECK (deployed_at >= 0),
    received_at  INTEGER NOT NULL CHECK (received_at >= 0)
) STRICT;

CREATE INDEX release_by_deploy ON release (deployed_at);
