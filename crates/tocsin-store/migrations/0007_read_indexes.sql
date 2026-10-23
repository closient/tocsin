-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Migration 7: indexes for the read API, so no public read walks a whole
-- table.

-- Outages that ended at or after a window's start but began before it:
-- the outages a window catches mid-way. Open outages ride outage_open.
CREATE INDEX outage_ended ON outage (ended_at) WHERE ended_at IS NOT NULL;

-- Incidents still open, which every status.json and llms.txt lists.
CREATE INDEX incident_unresolved ON incident (id) WHERE resolved_at IS NULL;
