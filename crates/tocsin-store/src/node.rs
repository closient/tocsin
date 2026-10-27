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
