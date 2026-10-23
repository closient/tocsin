-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Migration 6: heartbeat pings are check results. 0001 created a
-- placeholder heartbeat_ping table that no code path ever wrote; a ping is
-- now a row in check_result (health from the ping's status, its message in
-- detail), and a missed deadline is a down row the engine writes, so a
-- heartbeat shares the history, outages, rollups and retention of a probe.
-- Checks and heartbeats share one id space, which the configuration loader
-- enforces.

DROP TRIGGER heartbeat_ping_append_only;
DROP INDEX heartbeat_ping_by_check;
DROP TABLE heartbeat_ping;

-- Which kind of member produced a result. A check's day is rolled up by
-- counting its results; a heartbeat reports only when a job runs or a
-- deadline passes, so counting would weigh an hour of pings every minute
-- against a day of hourly misses. A heartbeat's day is rolled up by how
-- long each state held instead, in milliseconds, and its rollup row says so.
ALTER TABLE check_result
    ADD COLUMN kind TEXT NOT NULL DEFAULT 'check' CHECK (kind IN ('check', 'heartbeat'));
ALTER TABLE daily_rollup
    ADD COLUMN kind TEXT NOT NULL DEFAULT 'check' CHECK (kind IN ('check', 'heartbeat'));

-- The rollup finds a heartbeat's results here, so a day of checks alone
-- scans nothing more. It counts checks through check_result_by_time and
-- skips the days a heartbeat has results on: kind in that index would widen
-- the entry of every result, which every write and every purge pays for.
CREATE INDEX check_result_heartbeat_by_time ON check_result (started_at, check_id, node_id, health)
    WHERE kind = 'heartbeat';
