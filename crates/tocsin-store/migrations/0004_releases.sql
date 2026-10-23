-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Migration 4: the release log. 0001 created a placeholder release table
-- that no code path ever wrote; it is replaced here with the shape the
-- release API needs. Every timestamp is unix milliseconds, UTC. Every row
-- records who wrote it: `*_by_kind` is 'token' (a configured [[token]], by
-- id) or 'person' (named by the authenticating proxy).

DROP TABLE release;

-- One release of a [[project]] to one environment. Publishing the same
-- (project_id, version, environment) again updates this row; a deploy
-- stamped before the publish creates it with an empty title and notes and
-- a NULL released_at, which the publish fills in.
-- AUTOINCREMENT: an id is never reused, so links to a release stay true.
CREATE TABLE release (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id      TEXT    NOT NULL CHECK (project_id <> ''),
    -- Appears in URLs: ASCII letters, digits and . _ + -, starting with a
    -- letter or digit; the API enforces the same rule with a message.
    version         TEXT    NOT NULL CHECK (
        length(version) BETWEEN 1 AND 64
        AND version NOT GLOB '*[^A-Za-z0-9._+-]*'
        AND substr(version, 1, 1) GLOB '[A-Za-z0-9]'
    ),
    environment     TEXT    NOT NULL CHECK (
        length(environment) BETWEEN 1 AND 32
        AND environment NOT GLOB '*[^A-Za-z0-9._+-]*'
        AND substr(environment, 1, 1) GLOB '[A-Za-z0-9]'
    ),
    -- One line; empty only while the row is a deploy stamp awaiting its
    -- publish.
    title           TEXT    NOT NULL CHECK (length(title) <= 200),
    -- Markdown, at most 64 KiB.
    notes           TEXT    NOT NULL CHECK (length(CAST(notes AS BLOB)) <= 65536),
    url             TEXT    CHECK (url IS NULL OR url <> ''),
    released_at     INTEGER CHECK (released_at IS NULL OR released_at >= 0),
    deployed_at     INTEGER CHECK (deployed_at IS NULL OR deployed_at >= 0),
    created_at      INTEGER NOT NULL CHECK (created_at >= 0),
    created_by_kind TEXT    NOT NULL CHECK (created_by_kind IN ('token', 'person')),
    created_by      TEXT    NOT NULL CHECK (created_by <> ''),
    updated_at      INTEGER NOT NULL CHECK (updated_at >= 0),
    updated_by_kind TEXT    NOT NULL CHECK (updated_by_kind IN ('token', 'person')),
    updated_by      TEXT    NOT NULL CHECK (updated_by <> ''),
    -- The idempotency key: a re-post updates, never duplicates.
    UNIQUE (project_id, version, environment),
    -- A row is published, deployed, or both.
    CHECK (released_at IS NOT NULL OR deployed_at IS NOT NULL),
    -- A published release has a title.
    CHECK (released_at IS NULL OR title <> '')
) STRICT;

-- The release log, newest first, per project or across all.
CREATE INDEX release_by_project ON release (project_id, released_at);
CREATE INDEX release_by_released ON release (released_at);
-- The timeline places a release at its deploy, or at its publish until it
-- is deployed.
CREATE INDEX release_by_event ON release (coalesce(deployed_at, released_at));

CREATE TRIGGER release_identity_is_fixed
BEFORE UPDATE OF id, project_id, version, environment, created_at, created_by_kind, created_by
ON release
BEGIN
    SELECT RAISE(ABORT, 'a release''s id, key and creation are fixed');
END;

-- A published release is never moved earlier, nor unpublished.
CREATE TRIGGER release_never_moves_earlier
BEFORE UPDATE OF released_at ON release
WHEN OLD.released_at IS NOT NULL
    AND (NEW.released_at IS NULL OR NEW.released_at < OLD.released_at)
BEGIN
    SELECT RAISE(ABORT, 'a release''s released_at never moves earlier');
END;

-- The timeline reads outages from a start time across every check.
CREATE INDEX outage_by_start ON outage (started_at);
-- ...and incidents from a creation time.
CREATE INDEX incident_by_created ON incident (created_at);
