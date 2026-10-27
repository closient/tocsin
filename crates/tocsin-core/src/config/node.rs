// SPDX-License-Identifier: AGPL-3.0-only

//! `[[node]]`: the tocsin servers of a federation. Listing any makes every
//! server name itself explicitly, so two servers never share a node id
//! because they share a hostname, and a renamed host does not orphan the
//! outages it opened.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::values::Id;

/// `[[node]]`: one tocsin server of a federation.
///
/// Today a node is only a name: listing nodes makes `node_id` required and
/// limits it to the listed ids. Without any `[[node]]`, a single server
/// runs as before, named by `node_id` or its hostname.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Node {
    /// The node's id, which a server running as it gives as `--node`,
    /// `TOCSIN_NODE_ID` or `[server] node_id`.
    pub id: Id,
}

/// Where a server's `node_id` came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeIdSource {
    /// `--node` or `TOCSIN_NODE_ID`.
    Argument,
    /// `node_id` in `[server]`.
    Config,
    /// The machine's hostname, because nothing else named the node.
    Hostname,
}
