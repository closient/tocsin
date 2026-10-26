-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Migration 13: silences. A silence holds back the messages for the alerts
-- its matchers name, never the alerts themselves: the outage, the alert
-- and the delivery are still written, and the delivery is marked silenced
-- with the silence's id instead of being sent.
--
-- Every silence ends at `expires_at`, set when it is made and never moved.
-- One scoped to an episode (an outage) also stops applying when that
-- outage ends. Who made it is the caller the server authenticated.
CREATE TABLE silence (
    id              INTEGER PRIMARY KEY,
    -- Matchers: each one set must equal the alert's; NULL matches anything.
    node_id         TEXT    CHECK (node_id IS NULL OR node_id <> ''),
    subject_kind    TEXT    CHECK (subject_kind IS NULL OR subject_kind IN ('check', 'heartbeat')),
    subject_id      TEXT    CHECK (subject_id IS NULL OR subject_id <> ''),
    component_id    TEXT    CHECK (component_id IS NULL OR component_id <> ''),
    notifier_id     TEXT    CHECK (notifier_id IS NULL OR notifier_id <> ''),
    -- The outage it is scoped to: it stops applying when that outage ends.
    episode         INTEGER REFERENCES outage (id),
    reason          TEXT    NOT NULL CHECK (reason <> ''),
    created_at      INTEGER NOT NULL CHECK (created_at >= 0),
    created_by_kind TEXT    NOT NULL CHECK (created_by_kind IN ('token', 'person')),
    created_by      TEXT    NOT NULL CHECK (created_by <> ''),
    expires_at      INTEGER NOT NULL CHECK (expires_at > created_at),
    -- Set when someone ends it before it expires.
    ended_at        INTEGER CHECK (ended_at IS NULL OR ended_at >= created_at),
    ended_by_kind   TEXT    CHECK (ended_by_kind IS NULL OR ended_by_kind IN ('token', 'person')),
    ended_by        TEXT    CHECK (ended_by IS NULL OR ended_by <> ''),
    CHECK ((ended_at IS NULL) = (ended_by_kind IS NULL)),
    CHECK ((ended_at IS NULL) = (ended_by IS NULL)),
    -- A silence never matches every alert.
    CHECK (
        node_id IS NOT NULL OR subject_kind IS NOT NULL OR subject_id IS NOT NULL
        OR component_id IS NOT NULL OR notifier_id IS NOT NULL OR episode IS NOT NULL
    )
) STRICT;

CREATE INDEX silence_unended ON silence (expires_at) WHERE ended_at IS NULL;

-- A silence is never edited except to end it early, once.
CREATE TRIGGER silence_only_ends
BEFORE UPDATE ON silence
WHEN OLD.ended_at IS NOT NULL
    OR NEW.ended_at IS NULL
    OR NEW.id IS NOT OLD.id
    OR NEW.node_id IS NOT OLD.node_id
    OR NEW.subject_kind IS NOT OLD.subject_kind
    OR NEW.subject_id IS NOT OLD.subject_id
    OR NEW.component_id IS NOT OLD.component_id
    OR NEW.notifier_id IS NOT OLD.notifier_id
    OR NEW.episode IS NOT OLD.episode
    OR NEW.reason IS NOT OLD.reason
    OR NEW.created_at IS NOT OLD.created_at
    OR NEW.created_by_kind IS NOT OLD.created_by_kind
    OR NEW.created_by IS NOT OLD.created_by
    OR NEW.expires_at IS NOT OLD.expires_at
BEGIN
    SELECT RAISE(ABORT, 'a silence can only have ended_at set, once');
END;

-- Whether any fire or repeat for the alert has been delivered. A resolve
-- is held back only when nothing was: the receiver never heard of the
-- alert, so a resolve would answer nothing. Alerts from before this column
-- count as delivered, so their resolves are always sent.
ALTER TABLE alert ADD COLUMN notified INTEGER NOT NULL DEFAULT 1 CHECK (notified IN (0, 1));

-- The outbox gains a fourth end state, silenced, naming the silence that
-- held the message back. SQLite cannot change a CHECK in place, so the
-- table is rebuilt with every row and index kept. Nothing references it.
CREATE TABLE alert_delivery_new (
    id              INTEGER PRIMARY KEY,
    alert_id        INTEGER NOT NULL REFERENCES alert (id),
    kind            TEXT    NOT NULL CHECK (kind IN ('fire', 'repeat', 'resolve', 'cancel')),
    health          TEXT    NOT NULL CHECK (health IN ('degraded', 'down')),
    created_at      INTEGER NOT NULL CHECK (created_at >= 0),
    status          TEXT    NOT NULL DEFAULT 'pending'
                            CHECK (status IN ('pending', 'delivered', 'failed', 'silenced')),
    attempts        INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    next_attempt_at INTEGER NOT NULL,
    last_error      TEXT,
    finished_at     INTEGER,
    silence_id      INTEGER REFERENCES silence (id),
    CHECK ((status = 'pending') = (finished_at IS NULL)),
    CHECK ((status = 'silenced') = (silence_id IS NOT NULL)),
    -- A cancel stops a message already ringing; it is never held back.
    CHECK (status <> 'silenced' OR kind <> 'cancel')
) STRICT;

INSERT INTO alert_delivery_new (
    id, alert_id, kind, health, created_at, status, attempts, next_attempt_at,
    last_error, finished_at
)
SELECT
    id, alert_id, kind, health, created_at, status, attempts, next_attempt_at,
    last_error, finished_at
FROM alert_delivery;

DROP TABLE alert_delivery;
ALTER TABLE alert_delivery_new RENAME TO alert_delivery;

CREATE INDEX alert_delivery_pending ON alert_delivery (alert_id, id)
    WHERE status = 'pending';
CREATE INDEX alert_delivery_failed ON alert_delivery (finished_at)
    WHERE status = 'failed';
CREATE INDEX alert_delivery_delivered ON alert_delivery (finished_at)
    WHERE status = 'delivered';
CREATE INDEX alert_delivery_silenced ON alert_delivery (finished_at)
    WHERE status = 'silenced';
CREATE INDEX alert_delivery_by_alert ON alert_delivery (alert_id);
CREATE INDEX alert_delivery_by_silence ON alert_delivery (silence_id, id)
    WHERE silence_id IS NOT NULL;
