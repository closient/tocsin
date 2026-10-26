-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Migration 15: the labels and annotations an alert carries, as JSON
-- objects of strings, pinned when the alert opens: every fire, repeat and
-- resolve of it sends this set, so a reload that changes a subject's labels
-- cannot change the identity Alertmanager knows the alert by mid-episode.
-- Alerts open at upgrade were sent with none, so '{}' keeps them as they
-- were.
ALTER TABLE alert ADD COLUMN labels TEXT NOT NULL DEFAULT '{}'
    CHECK (json_valid(labels) AND json_type(labels) = 'object');
ALTER TABLE alert ADD COLUMN annotations TEXT NOT NULL DEFAULT '{}'
    CHECK (json_valid(annotations) AND json_type(annotations) = 'object');
