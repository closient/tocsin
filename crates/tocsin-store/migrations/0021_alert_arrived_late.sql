-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Migration 21: when an alert for another node's outage arrived, if it
-- arrived late: the node detected the outage more than its push interval
-- and grace (plus the time a check takes to confirm) before this node heard
-- of it, as after a partition or with this node down. Its messages say
-- when the node detected it and that it arrived late. NULL for every alert
-- that did not, and for every alert this node fired for its own checks.
ALTER TABLE alert ADD COLUMN arrived_late_at INTEGER
    CHECK (arrived_late_at IS NULL OR arrived_late_at >= 0);
