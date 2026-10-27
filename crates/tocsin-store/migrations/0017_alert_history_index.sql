-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Migration 17: the alert history reads alerts newest start first, ties by
-- id, and resumes a page after a cursor's (started_at, id); this walks that
-- order instead of sorting the whole table.
CREATE INDEX alert_by_start ON alert (started_at, id);
