-- Operator alerting: the durable outbox.
--
-- An alert is one notifier's view of one alert identity (the node plus the
-- check or heartbeat) from the change that fires it to the recovery that
-- resolves it. It spans every outage in between: a check that goes from
-- degraded to down stays one alert, re-fired with the new health.
--
-- Rows are written in the same transaction as the outage change that causes
-- them, so an outage that alerted always has its deliveries queued and a
-- recovery always has its resolve queued, however far the delivery worker is
-- behind and across restarts.
CREATE TABLE alert (
    id             INTEGER PRIMARY KEY,
    node_id        TEXT    NOT NULL CHECK (node_id <> ''),
    subject_kind   TEXT    NOT NULL CHECK (subject_kind IN ('check', 'heartbeat')),
    subject_id     TEXT    NOT NULL CHECK (subject_id <> ''),
    component_id   TEXT    CHECK (component_id IS NULL OR component_id <> ''),
    notifier_id    TEXT    NOT NULL CHECK (notifier_id <> ''),
    -- The first route that sent this identity to this notifier.
    route_id       TEXT    NOT NULL CHECK (route_id <> ''),
    -- The outage it fired for most recently.
    outage_id      INTEGER NOT NULL REFERENCES outage (id),
    health         TEXT    NOT NULL CHECK (health IN ('degraded', 'down')),
    started_at     INTEGER NOT NULL CHECK (started_at >= 0),
    -- Whether the recovery sends a resolve.
    resolve        INTEGER NOT NULL CHECK (resolve IN (0, 1)),
    -- How often to re-send while it fires; NULL never re-sends.
    repeat_ms      INTEGER CHECK (repeat_ms IS NULL OR repeat_ms > 0),
    next_repeat_at INTEGER,
    ended_at       INTEGER CHECK (ended_at IS NULL OR ended_at >= started_at)
) STRICT;

-- At most one firing alert per identity per notifier.
CREATE UNIQUE INDEX alert_open ON alert (node_id, subject_id, notifier_id)
    WHERE ended_at IS NULL;
CREATE INDEX alert_by_outage ON alert (outage_id);

-- One message to send for an alert, in id order: a resolve is never sent
-- while an earlier delivery for the same alert is still pending.
CREATE TABLE alert_delivery (
    id              INTEGER PRIMARY KEY,
    alert_id        INTEGER NOT NULL REFERENCES alert (id),
    kind            TEXT    NOT NULL CHECK (kind IN ('fire', 'repeat', 'resolve')),
    health          TEXT    NOT NULL CHECK (health IN ('degraded', 'down')),
    created_at      INTEGER NOT NULL CHECK (created_at >= 0),
    status          TEXT    NOT NULL DEFAULT 'pending'
                            CHECK (status IN ('pending', 'delivered', 'failed')),
    attempts        INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    next_attempt_at INTEGER NOT NULL,
    last_error      TEXT,
    finished_at     INTEGER,
    CHECK ((status = 'pending') = (finished_at IS NULL))
) STRICT;

CREATE INDEX alert_delivery_pending ON alert_delivery (alert_id, id)
    WHERE status = 'pending';
CREATE INDEX alert_delivery_failed ON alert_delivery (finished_at)
    WHERE status = 'failed';
CREATE INDEX alert_delivery_by_alert ON alert_delivery (alert_id);
