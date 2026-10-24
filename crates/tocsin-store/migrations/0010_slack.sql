-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Migration 10: Slack channels connected by signing in to Slack.
--
-- A connection is one Slack incoming webhook, made when someone approves
-- the operator's own Slack app on Slack's consent screen. An operator
-- connection has a name a `[[notifier]]` refers to; a subscriber
-- connection has preferences, as an email subscriber does, and a manage
-- secret for the link in each of its messages. The webhook URL is a
-- secret: it is read only to post, and never shown.
--
-- A connection Slack has revoked (the channel archived or deleted, the app
-- removed) is marked dead rather than deleted, so the admin page can say
-- so; nothing is sent to it again.
CREATE TABLE slack_connection (
    id                INTEGER PRIMARY KEY,
    purpose           TEXT    NOT NULL CHECK (purpose IN ('operator', 'subscriber')),
    -- What a notifier calls it, for an operator connection.
    name              TEXT    UNIQUE CHECK (name IS NULL OR name <> ''),
    webhook_url       TEXT    NOT NULL CHECK (webhook_url <> ''),
    channel           TEXT    NOT NULL,
    channel_id        TEXT,
    team              TEXT    NOT NULL,
    team_id           TEXT,
    -- Where the workspace's admins manage the webhook on Slack.
    configuration_url TEXT,
    -- The admin who connected it, for an operator connection.
    created_by        TEXT,
    created_at        INTEGER NOT NULL CHECK (created_at >= 0),
    manage_secret     TEXT    CHECK (manage_secret IS NULL OR length(manage_secret) >= 32),
    dead_at           INTEGER,
    dead_reason       TEXT,
    CHECK ((purpose = 'operator') = (name IS NOT NULL)),
    CHECK ((purpose = 'subscriber') = (manage_secret IS NOT NULL)),
    CHECK ((dead_at IS NULL) = (dead_reason IS NULL))
) STRICT;

-- What a subscriber connection hears about, as `subscription` is for an
-- email subscriber.
CREATE TABLE slack_subscription (
    connection_id INTEGER NOT NULL REFERENCES slack_connection (id) ON DELETE CASCADE,
    kind          TEXT    NOT NULL CHECK (kind IN ('component', 'project')),
    target        TEXT    NOT NULL CHECK (target <> ''),
    PRIMARY KEY (connection_id, kind, target)
) STRICT, WITHOUT ROWID;

-- The delivery log of each announcement (`subscriber_event`) to each
-- subscriber connection, as `subscriber_delivery` is for email: one row
-- per (connection, event), and one found `sending` at startup is failed,
-- never posted again.
CREATE TABLE slack_delivery (
    id              INTEGER PRIMARY KEY,
    connection_id   INTEGER NOT NULL REFERENCES slack_connection (id) ON DELETE CASCADE,
    event_id        INTEGER NOT NULL REFERENCES subscriber_event (id) ON DELETE CASCADE,
    status          TEXT    NOT NULL DEFAULT 'pending'
                            CHECK (status IN ('pending', 'sending', 'sent', 'failed')),
    attempts        INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    next_attempt_at INTEGER NOT NULL,
    last_error      TEXT,
    finished_at     INTEGER,
    UNIQUE (connection_id, event_id),
    CHECK ((status IN ('pending', 'sending')) = (finished_at IS NULL))
) STRICT;

CREATE INDEX slack_delivery_due ON slack_delivery (next_attempt_at)
    WHERE status = 'pending';
CREATE INDEX slack_delivery_sending ON slack_delivery (id)
    WHERE status = 'sending';
CREATE INDEX slack_delivery_finished ON slack_delivery (finished_at)
    WHERE finished_at IS NOT NULL;
CREATE INDEX slack_delivery_by_event ON slack_delivery (event_id);
