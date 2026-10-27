-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Migration 18: the stores this database knows, one row each. A store is
-- one database: its id is made here, on the first open, and kept for as
-- long as the file is, so a wiped database is a new store even when it
-- runs as the same node. The local row is this database's own; rows for
-- other nodes' stores come with federation (docs/design/federation.md).
CREATE TABLE node_store (
    id            INTEGER PRIMARY KEY,
    -- The node the store belongs to. NULL only on the local row: which
    -- node a database runs as is configuration, not data.
    node_id       TEXT    CHECK (node_id IS NULL OR node_id <> ''),
    -- 128 random bits as 32 lowercase hex digits.
    store_id      TEXT    NOT NULL
                          CHECK (length(store_id) = 32 AND store_id NOT GLOB '*[^0-9a-f]*'),
    local         INTEGER NOT NULL CHECK (local IN (0, 1)),
    state         TEXT    NOT NULL DEFAULT 'active'
                          CHECK (state IN ('active', 'retired', 'conflicted')),
    first_seen_at INTEGER NOT NULL CHECK (first_seen_at >= 0),
    last_push_at  INTEGER CHECK (last_push_at IS NULL OR last_push_at >= 0),
    -- The sequence number the next local change gets; the local row's only.
    next_seq      INTEGER CHECK (next_seq IS NULL OR next_seq >= 1),
    CHECK (local = 1 OR node_id IS NOT NULL),
    CHECK ((local = 1) = (next_seq IS NOT NULL)),
    UNIQUE (node_id, store_id)
) STRICT;

-- At most one local row: UNIQUE (node_id, store_id) cannot say so, since
-- the local row's node_id is NULL.
CREATE UNIQUE INDEX node_store_local ON node_store (local) WHERE local = 1;

INSERT INTO node_store (node_id, store_id, local, first_seen_at, next_seq)
VALUES (
    NULL,
    lower(hex(randomblob(16))),
    1,
    CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER),
    1
);
