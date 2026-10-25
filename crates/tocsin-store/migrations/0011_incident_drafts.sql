-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Migration 11: draft incidents. An incident with no `published_at` is a
-- draft: it shows on the internal board and in the admin, never on a
-- public surface, and notifies nobody until a person publishes it, which
-- stamps when and by whom. Before this column every incident was public
-- the moment it was written, so every existing incident is backfilled as
-- published by its creator when it was created; none becomes a draft.
ALTER TABLE incident ADD COLUMN published_at INTEGER CHECK (published_at IS NULL OR published_at >= 0);
ALTER TABLE incident ADD COLUMN published_by_kind TEXT CHECK (published_by_kind IN ('token', 'person'));
-- When, by which kind, and by whom are set together or not at all.
ALTER TABLE incident ADD COLUMN published_by TEXT CHECK (
    published_by <> ''
    AND (published_at IS NULL) = (published_by IS NULL)
    AND (published_by_kind IS NULL) = (published_by IS NULL)
);

UPDATE incident
SET published_at = created_at,
    published_by_kind = created_by_kind,
    published_by = created_by;

-- Publishing happens once: a published incident cannot go back to being a
-- draft, nor change who published it.
CREATE TRIGGER incident_publishes_once
BEFORE UPDATE OF published_at, published_by_kind, published_by ON incident
WHEN OLD.published_at IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'an incident is published once');
END;

-- The drafts, which the internal board lists first.
CREATE INDEX incident_draft ON incident (id) WHERE published_at IS NULL;
