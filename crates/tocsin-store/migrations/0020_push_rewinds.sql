-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Migration 20: how many times rewritten rollups have moved a push cursor
-- back. A push records the count it read and moves `rollups_from` past the
-- days it sent only while the count is unchanged, so a rewrite committed
-- while the push was in flight is not skipped, even one of the very day
-- the cursor was already at.
ALTER TABLE push_cursor ADD COLUMN rollups_rewound INTEGER NOT NULL DEFAULT 0
    CHECK (rollups_rewound >= 0);
