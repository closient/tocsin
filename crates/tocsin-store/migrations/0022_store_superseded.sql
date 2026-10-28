-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Migration 22: when a newer store of the same node replaced this one, as a
-- rebuilt node's wiped database does (docs/design/federation.md, "Duplicate
-- node ids"). Such a store is retired, its open outages withdrawn, and if
-- it pushes again two machines are pushing as one node, which marks the
-- node conflicted. NULL for a store nothing replaced: the local row, one
-- still active, and one retired because its node was.
ALTER TABLE node_store ADD COLUMN superseded_at INTEGER
    CHECK (superseded_at IS NULL OR superseded_at >= 0);
