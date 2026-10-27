-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Migration 19: rows from other nodes' stores (docs/design/federation.md).
-- A result or outage another node pushed keeps its portable key: the store
-- it came from and its id there. Both are NULL on local rows, so a store
-- that never federates pays nothing for them.
ALTER TABLE check_result ADD COLUMN origin_store INTEGER REFERENCES node_store (id);
ALTER TABLE check_result ADD COLUMN origin_id INTEGER
    CHECK ((origin_store IS NULL) = (origin_id IS NULL));
CREATE UNIQUE INDEX check_result_origin ON check_result (origin_store, origin_id)
WHERE origin_store IS NOT NULL;

ALTER TABLE outage ADD COLUMN origin_store INTEGER REFERENCES node_store (id);
ALTER TABLE outage ADD COLUMN origin_id INTEGER
    CHECK ((origin_store IS NULL) = (origin_id IS NULL));
CREATE UNIQUE INDEX outage_origin ON outage (origin_store, origin_id)
WHERE origin_store IS NOT NULL;

-- The order its store opened and ended the outage in: one counter per
-- store (node_store.next_seq for this one) numbers both, so replaying a
-- store's changes in seq order never has two outages open for one check.
ALTER TABLE outage ADD COLUMN opened_seq INTEGER CHECK (opened_seq IS NULL OR opened_seq >= 1);
ALTER TABLE outage ADD COLUMN ended_seq INTEGER CHECK (ended_seq IS NULL OR ended_seq > opened_seq);

-- The trigger refuses any change to an ended outage, the backfill included.
DROP TRIGGER outage_only_ends;

-- Existing outages get seqs in id order, each end straight after its open.
-- Ids only grow, and an outage for a check opens only once the last one
-- has ended, so this order never has two open for one check either.
WITH ranked AS (SELECT id, row_number() OVER (ORDER BY id) AS rank FROM outage)
UPDATE outage
SET opened_seq = 2 * ranked.rank - 1,
    ended_seq = CASE WHEN outage.ended_at IS NULL THEN NULL ELSE 2 * ranked.rank END
FROM ranked
WHERE ranked.id = outage.id;

UPDATE node_store
SET next_seq = 1 + coalesce((SELECT max(max(opened_seq), coalesce(max(ended_seq), 0)) FROM outage), 0)
WHERE local = 1;

-- An outage ends once, with when, how and its seq.
CREATE TRIGGER outage_only_ends
BEFORE UPDATE ON outage
WHEN OLD.ended_at IS NOT NULL
    OR NEW.ended_at IS NULL
    OR NEW.ending IS NULL
    OR NEW.ended_seq IS NULL
    OR NEW.id IS NOT OLD.id
    OR NEW.node_id IS NOT OLD.node_id
    OR NEW.subject_kind IS NOT OLD.subject_kind
    OR NEW.check_id IS NOT OLD.check_id
    OR NEW.component_id IS NOT OLD.component_id
    OR NEW.health IS NOT OLD.health
    OR NEW.started_at IS NOT OLD.started_at
    OR NEW.alerted IS NOT OLD.alerted
    OR NEW.origin_store IS NOT OLD.origin_store
    OR NEW.origin_id IS NOT OLD.origin_id
    OR NEW.opened_seq IS NOT OLD.opened_seq
BEGIN
    SELECT RAISE(ABORT, 'an outage can only have ended_at and ending set, once, with ended_seq');
END;

-- Every outage is numbered from the start; an ended one carries both seqs.
CREATE TRIGGER outage_numbered
BEFORE INSERT ON outage
WHEN NEW.opened_seq IS NULL OR (NEW.ended_at IS NULL) <> (NEW.ended_seq IS NULL)
BEGIN
    SELECT RAISE(ABORT, 'an outage needs opened_seq, and ended_seq exactly when it has ended');
END;

-- How far each upstream has acknowledged this store's rows: the last
-- result id and outage seq it committed, and the first day whose rollups
-- it may not have yet.
CREATE TABLE push_cursor (
    upstream_id     TEXT    PRIMARY KEY CHECK (upstream_id <> ''),
    results_through INTEGER NOT NULL DEFAULT 0 CHECK (results_through >= 0),
    outages_through INTEGER NOT NULL DEFAULT 0 CHECK (outages_through >= 0),
    rollups_from    TEXT    CHECK (rollups_from IS NULL OR date(rollups_from) IS rollups_from),
    acked_at        INTEGER CHECK (acked_at IS NULL OR acked_at >= 0)
) STRICT;
