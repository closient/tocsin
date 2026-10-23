-- Whether opening the outage alerted. A recovery resolves only an outage
-- that alerted, so every alert gets its resolve and no resolve arrives
-- without an alert, even when a maintenance window starts or ends while the
-- check is out, and across restarts. Outages recorded before this column
-- existed alerted unless they opened in maintenance, which was not recorded:
-- they count as alerted.
ALTER TABLE outage ADD COLUMN alerted INTEGER NOT NULL DEFAULT 1 CHECK (alerted IN (0, 1));

DROP TRIGGER outage_only_ends;

CREATE TRIGGER outage_only_ends
BEFORE UPDATE ON outage
WHEN OLD.ended_at IS NOT NULL
    OR NEW.ended_at IS NULL
    OR NEW.id IS NOT OLD.id
    OR NEW.node_id IS NOT OLD.node_id
    OR NEW.check_id IS NOT OLD.check_id
    OR NEW.component_id IS NOT OLD.component_id
    OR NEW.health IS NOT OLD.health
    OR NEW.started_at IS NOT OLD.started_at
    OR NEW.alerted IS NOT OLD.alerted
BEGIN
    SELECT RAISE(ABORT, 'an outage can only have ended_at set, once');
END;
