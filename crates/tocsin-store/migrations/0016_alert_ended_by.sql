-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Migration 16: why an alert ended. 'recovery' when its check or heartbeat
-- recovered; 'config' when a reload or restart took its check, heartbeat
-- or notifier out of the configuration, so nobody saw it recover. The
-- resolve says which. Alerts that ended before the upgrade keep NULL, read
-- as a recovery: only recoveries were ever resolved before.
ALTER TABLE alert ADD COLUMN ended_by TEXT
    CHECK (ended_by IS NULL OR ended_by IN ('recovery', 'config'));
