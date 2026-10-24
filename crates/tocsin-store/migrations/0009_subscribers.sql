-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Migration 9: email subscribers on the public page.
--
-- A subscriber is one address. Until it is confirmed it has a confirmation
-- token (stored as its SHA-256, so the database never holds a working
-- link) that expires; nothing but the confirmation is ever sent to it.
-- Every email to a confirmed address carries its manage link, whose token
-- is the row id and `manage_secret`, compared in constant time.
CREATE TABLE subscriber (
    id                 INTEGER PRIMARY KEY,
    -- Lowercased, so one address is one row whatever its spelling.
    email              TEXT    NOT NULL UNIQUE CHECK (email <> '' AND email = lower(email)),
    created_at         INTEGER NOT NULL CHECK (created_at >= 0),
    confirmed_at       INTEGER CHECK (confirmed_at IS NULL OR confirmed_at >= created_at),
    -- Hex SHA-256 of the confirmation token; NULL once confirmed.
    confirm_hash       TEXT    UNIQUE CHECK (confirm_hash IS NULL OR length(confirm_hash) = 64),
    confirm_expires_at INTEGER,
    manage_secret      TEXT    NOT NULL CHECK (length(manage_secret) >= 32),
    CHECK ((confirm_hash IS NULL) = (confirm_expires_at IS NULL)),
    CHECK ((confirmed_at IS NULL) = (confirm_hash IS NOT NULL))
) STRICT;

CREATE INDEX subscriber_unconfirmed ON subscriber (confirm_expires_at)
    WHERE confirm_expires_at IS NOT NULL;

-- What a subscriber hears about: incidents on a component, or releases of
-- a project. `*` is every component or every public project, including
-- ones added later.
CREATE TABLE subscription (
    subscriber_id INTEGER NOT NULL REFERENCES subscriber (id) ON DELETE CASCADE,
    kind          TEXT    NOT NULL CHECK (kind IN ('component', 'project')),
    target        TEXT    NOT NULL CHECK (target <> ''),
    PRIMARY KEY (subscriber_id, kind, target)
) STRICT, WITHOUT ROWID;

-- One thing to announce: an incident opened, an update posted, a release
-- published. `key` names it (`incident:7`, `incident:7:update:12`,
-- `release:api:1.4.0`) and is unique, so it is queued at most once. The
-- subject and body are rendered when it is queued; each delivery adds the
-- recipient's own manage link.
CREATE TABLE subscriber_event (
    id          INTEGER PRIMARY KEY,
    key         TEXT    NOT NULL UNIQUE CHECK (key <> ''),
    -- Which subscriptions it matches.
    scope       TEXT    NOT NULL CHECK (scope IN ('component', 'project')),
    subject     TEXT    NOT NULL CHECK (subject <> ''),
    body        TEXT    NOT NULL,
    created_at  INTEGER NOT NULL CHECK (created_at >= 0),
    -- When its deliveries were queued, in the same transaction.
    expanded_at INTEGER CHECK (expanded_at IS NULL OR expanded_at >= created_at)
) STRICT;

CREATE INDEX subscriber_event_unexpanded ON subscriber_event (id)
    WHERE expanded_at IS NULL;

-- The components or project an event is about. An incident naming no
-- component reaches every subscriber to incidents.
CREATE TABLE subscriber_event_target (
    event_id INTEGER NOT NULL REFERENCES subscriber_event (id) ON DELETE CASCADE,
    target   TEXT    NOT NULL CHECK (target <> ''),
    PRIMARY KEY (event_id, target)
) STRICT, WITHOUT ROWID;

-- The delivery log: one row per (subscriber, event), so an event reaches
-- an address at most once across restarts. A delivery is marked `sending`
-- in its own committed transaction before the SMTP server sees it; one
-- found `sending` at startup was interrupted and is failed, never resent,
-- because the server may already have accepted it.
CREATE TABLE subscriber_delivery (
    id              INTEGER PRIMARY KEY,
    subscriber_id   INTEGER NOT NULL REFERENCES subscriber (id) ON DELETE CASCADE,
    event_id        INTEGER NOT NULL REFERENCES subscriber_event (id) ON DELETE CASCADE,
    status          TEXT    NOT NULL DEFAULT 'pending'
                            CHECK (status IN ('pending', 'sending', 'sent', 'failed')),
    attempts        INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    next_attempt_at INTEGER NOT NULL,
    last_error      TEXT,
    finished_at     INTEGER,
    UNIQUE (subscriber_id, event_id),
    CHECK ((status IN ('pending', 'sending')) = (finished_at IS NULL))
) STRICT;

CREATE INDEX subscriber_delivery_due ON subscriber_delivery (next_attempt_at)
    WHERE status = 'pending';
CREATE INDEX subscriber_delivery_sending ON subscriber_delivery (id)
    WHERE status = 'sending';
CREATE INDEX subscriber_delivery_finished ON subscriber_delivery (finished_at)
    WHERE finished_at IS NOT NULL;
CREATE INDEX subscriber_delivery_by_event ON subscriber_delivery (event_id);
