-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Migration 2: incidents and scheduled maintenance. 0001 created placeholder incident
-- tables that no code path ever wrote; they are replaced here with the
-- shape the incident API needs. Every timestamp is unix milliseconds, UTC.
-- Every row records who wrote it: `*_by_kind` is 'token' (a configured
-- [[token]], by id) or 'person' (named by the authenticating proxy).

DROP TABLE incident_update;
DROP TABLE incident;

-- An incident or scheduled maintenance. Edits change this row in place
-- (and are logged in incident_edit); status follows the latest update.
-- AUTOINCREMENT: an id is never reused, so links to an incident stay true.
CREATE TABLE incident (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    kind            TEXT    NOT NULL CHECK (kind IN ('incident', 'maintenance')),
    title           TEXT    NOT NULL CHECK (trim(title) <> ''),
    impact          TEXT    NOT NULL CHECK (impact IN ('none', 'minor', 'major', 'critical')),
    status          TEXT    NOT NULL,
    scheduled_start INTEGER,
    scheduled_end   INTEGER,
    resolved_at     INTEGER CHECK (resolved_at IS NULL OR resolved_at >= 0),
    created_at      INTEGER NOT NULL CHECK (created_at >= 0),
    created_by_kind TEXT    NOT NULL CHECK (created_by_kind IN ('token', 'person')),
    created_by      TEXT    NOT NULL CHECK (created_by <> ''),
    updated_at      INTEGER NOT NULL CHECK (updated_at >= 0),
    updated_by_kind TEXT    NOT NULL CHECK (updated_by_kind IN ('token', 'person')),
    updated_by      TEXT    NOT NULL CHECK (updated_by <> ''),
    -- Each kind has its own statuses.
    CHECK (
        (kind = 'incident' AND status IN ('investigating', 'identified', 'monitoring', 'resolved'))
        OR (kind = 'maintenance' AND status IN ('scheduled', 'in_progress', 'completed'))
    ),
    -- Maintenance always has a forward schedule; an incident never has one.
    CHECK (
        (kind = 'incident' AND scheduled_start IS NULL AND scheduled_end IS NULL)
        OR (kind = 'maintenance' AND scheduled_start IS NOT NULL AND scheduled_end > scheduled_start)
    ),
    -- Resolved exactly when the status says it is over.
    CHECK ((resolved_at IS NOT NULL) = (status IN ('resolved', 'completed')))
) STRICT;

CREATE TRIGGER incident_identity_is_fixed
BEFORE UPDATE OF id, kind, created_at, created_by_kind, created_by ON incident
BEGIN
    SELECT RAISE(ABORT, 'an incident''s id, kind and creation are fixed');
END;

-- The components an incident affects, by [[component]] id. Replaced as a
-- set by an edit.
CREATE TABLE incident_component (
    incident_id  INTEGER NOT NULL REFERENCES incident (id) ON DELETE CASCADE,
    component_id TEXT    NOT NULL CHECK (component_id <> ''),
    PRIMARY KEY (incident_id, component_id)
) STRICT, WITHOUT ROWID;

CREATE INDEX incident_component_by_component ON incident_component (component_id, incident_id);

-- The timeline: one row per update, append-only. The first is the opening
-- message.
CREATE TABLE incident_update (
    id              INTEGER PRIMARY KEY,
    incident_id     INTEGER NOT NULL REFERENCES incident (id) ON DELETE CASCADE,
    status          TEXT    NOT NULL CHECK (status IN (
        'investigating', 'identified', 'monitoring', 'resolved',
        'scheduled', 'in_progress', 'completed'
    )),
    -- Markdown.
    message         TEXT    NOT NULL CHECK (trim(message) <> ''),
    created_at      INTEGER NOT NULL CHECK (created_at >= 0),
    created_by_kind TEXT    NOT NULL CHECK (created_by_kind IN ('token', 'person')),
    created_by      TEXT    NOT NULL CHECK (created_by <> '')
) STRICT;

CREATE INDEX incident_update_by_incident ON incident_update (incident_id, id);

CREATE TRIGGER incident_update_append_only
BEFORE UPDATE ON incident_update
BEGIN
    SELECT RAISE(ABORT, 'incident_update is append-only');
END;

-- One row per edit (PATCH), append-only, so every change to an incident's
-- fields keeps who made it even after a later edit overwrites the row.
CREATE TABLE incident_edit (
    id              INTEGER PRIMARY KEY,
    incident_id     INTEGER NOT NULL REFERENCES incident (id) ON DELETE CASCADE,
    -- The fields the edit set, as a JSON object. Diagnostic only: never
    -- queried on; the incident row is the source of truth.
    changes         TEXT    NOT NULL CHECK (json_valid(changes)),
    created_at      INTEGER NOT NULL CHECK (created_at >= 0),
    created_by_kind TEXT    NOT NULL CHECK (created_by_kind IN ('token', 'person')),
    created_by      TEXT    NOT NULL CHECK (created_by <> '')
) STRICT;

CREATE INDEX incident_edit_by_incident ON incident_edit (incident_id, id);

CREATE TRIGGER incident_edit_append_only
BEFORE UPDATE ON incident_edit
BEGIN
    SELECT RAISE(ABORT, 'incident_edit is append-only');
END;
