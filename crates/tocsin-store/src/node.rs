// SPDX-License-Identifier: AGPL-3.0-only

//! `node_store`: the stores this database knows, its own among them.

use rusqlite::Connection;
use rusqlite::types::Type;
use tocsin_core::federation::wire::StoreState;

use crate::error::Result;

/// Another node's store, as this database knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteStore {
    /// The node it belongs to.
    pub node_id: String,
    /// Its id: 32 lowercase hex digits.
    pub store_id: String,
    /// Whether its pushes are taken.
    pub state: StoreState,
    /// When its first push arrived, by this node's clock (unix
    /// milliseconds).
    pub first_seen_at_ms: i64,
    /// When its last push was applied, by this node's clock.
    pub last_push_at_ms: Option<i64>,
}

/// Every other node's store this database knows, by node, then by when
/// each was first seen.
pub(crate) fn remote_stores(conn: &Connection) -> Result<Vec<RemoteStore>> {
    let mut statement = conn.prepare_cached(
        "SELECT node_id, store_id, state, first_seen_at, last_push_at FROM node_store
         WHERE local = 0 ORDER BY node_id, first_seen_at, id",
    )?;
    let rows = statement.query_map([], |row| {
        let state: String = row.get(2)?;
        Ok(RemoteStore {
            node_id: row.get(0)?,
            store_id: row.get(1)?,
            state: state.parse().map_err(|error: String| {
                rusqlite::Error::FromSqlConversionFailure(2, Type::Text, error.into())
            })?,
            first_seen_at_ms: row.get(3)?,
            last_push_at_ms: row.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

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
