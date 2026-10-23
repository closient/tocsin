// SPDX-License-Identifier: AGPL-3.0-only

//! The SQLite store: check results, outages, incidents and releases.
//!
//! The database holds history, never configuration. Results are
//! append-only and every row carries the `node_id` that produced it.
