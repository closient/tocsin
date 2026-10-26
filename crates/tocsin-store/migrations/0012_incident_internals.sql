-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Migration 12: what an incident keeps for the people who run it. The
-- internal note is evidence and links a drafter (a person or an automated
-- one) leaves for whoever publishes; it is never shown publicly, before or
-- after publishing. The external key is the drafter's own name for what
-- it is reporting, such as an alert cluster: a second create with the same
-- key answers the incident already there instead of opening another. Both
-- are empty on every existing incident.
ALTER TABLE incident ADD COLUMN internal_note TEXT CHECK (internal_note <> '');
ALTER TABLE incident ADD COLUMN external_key TEXT CHECK (external_key <> '');

-- One incident per key. Discarding a draft deletes its row and frees its
-- key; a published incident keeps its key for good.
CREATE UNIQUE INDEX incident_external_key ON incident (external_key)
WHERE external_key IS NOT NULL;

-- The key is the drafter's, given once at creation.
CREATE TRIGGER incident_external_key_is_fixed
BEFORE UPDATE OF external_key ON incident
WHEN OLD.external_key IS NOT NEW.external_key
BEGIN
    SELECT RAISE(ABORT, 'an incident''s external key is set at creation only');
END;
