-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Migration 8: how each outage ended. `recovered` when tocsin saw the check
-- leave the outage's health (back up, or between degraded and down);
-- `withdrawn` when the check or heartbeat was removed from the
-- configuration while out, so its end is when tocsin stopped watching, not
-- a recovery. NULL while open. Before this column only recoveries ended an
-- outage, so every outage already ended recovered.
ALTER TABLE outage ADD COLUMN ending TEXT CHECK (ending IN ('recovered', 'withdrawn'));

-- The trigger refuses any change to an ended outage, the backfill included.
DROP TRIGGER outage_only_ends;

UPDATE outage SET ending = 'recovered' WHERE ended_at IS NOT NULL;

-- An outage ends once, with both when and how.
CREATE TRIGGER outage_only_ends
BEFORE UPDATE ON outage
WHEN OLD.ended_at IS NOT NULL
    OR NEW.ended_at IS NULL
    OR NEW.ending IS NULL
    OR NEW.id IS NOT OLD.id
    OR NEW.node_id IS NOT OLD.node_id
    OR NEW.subject_kind IS NOT OLD.subject_kind
    OR NEW.check_id IS NOT OLD.check_id
    OR NEW.component_id IS NOT OLD.component_id
    OR NEW.health IS NOT OLD.health
    OR NEW.started_at IS NOT OLD.started_at
    OR NEW.alerted IS NOT OLD.alerted
BEGIN
    SELECT RAISE(ABORT, 'an outage can only have ended_at and ending set, once');
END;
