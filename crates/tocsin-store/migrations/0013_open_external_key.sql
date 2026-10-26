-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Migration 13: an external key is unique only among open incidents. A
-- drafter's key names what it reports, such as an alert cluster, and the
-- same trouble firing again weeks after it was resolved is a new episode:
-- a create with the key then opens a new incident instead of answering the
-- resolved one. Resolving an incident (or completing a maintenance, which
-- also sets `resolved_at`) frees its key, as discarding a draft does.
DROP INDEX incident_external_key;

CREATE UNIQUE INDEX incident_open_external_key ON incident (external_key)
WHERE external_key IS NOT NULL AND resolved_at IS NULL;

-- Every incident ever given a key, for the list by key.
CREATE INDEX incident_external_key ON incident (external_key)
WHERE external_key IS NOT NULL;
