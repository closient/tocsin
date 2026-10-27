// SPDX-License-Identifier: AGPL-3.0-only

//! `node_store`: the stores this database knows, its own among them.

use rusqlite::Connection;

use crate::error::Result;

/// The id of this database's own store, made when it was first opened.
pub(crate) fn local_store_id(conn: &Connection) -> Result<String> {
    Ok(conn.query_row(
        "SELECT store_id FROM node_store WHERE local = 1",
        [],
        |row| row.get(0),
    )?)
}

/// Takes the next seq from this store's counter, for an outage it opens or
/// ends: the order its outage changes are pushed and replayed in.
pub(crate) fn take_seq(conn: &Connection) -> rusqlite::Result<i64> {
    conn.prepare_cached(
        "UPDATE node_store SET next_seq = next_seq + 1 WHERE local = 1 RETURNING next_seq - 1",
    )?
    .query_row([], |row| row.get(0))
}
