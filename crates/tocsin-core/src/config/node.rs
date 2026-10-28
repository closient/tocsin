// SPDX-License-Identifier: AGPL-3.0-only

//! `[[node]]` and `[federation]`: the tocsin servers of a federation and
//! how they work together. Listing any node makes every server name itself
//! explicitly, so two servers never share a node id because they share a
//! hostname, and a renamed host does not orphan the outages it opened.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::values::{Duration, Id};
use crate::federation::PublicKey;

/// `[[node]]`: one tocsin server of a federation.
///
/// Listing nodes makes `node_id` required and limits it to the listed ids.
/// Without any `[[node]]`, a single server runs as before, named by
/// `node_id` or its hostname.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Node {
    /// The node's id, which a server running as it gives as `--node`,
    /// `TOCSIN_NODE_ID` or `[server] node_id`.
    pub id: Id,
    /// Where the node takes pushes from other nodes. Omit it for a node
    /// that only pushes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The public keys of the node's `[server] node_key`, as `tocsin node
    /// keygen` prints them. A push from the node is accepted if any of
    /// them verifies it, so two are listed during a rotation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub public_keys: Vec<PublicKey>,
    /// The nodes this node pushes its results and outages to. Each needs
    /// a `url`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub push_to: Vec<Id>,
    /// Whether the node has been decommissioned. Its entry stays, so its
    /// id is never reused for another machine whose history would merge
    /// into its own; no server may run as it.
    #[serde(default)]
    pub retired: bool,
}

/// `[federation]`: settings every node of a federation shares. Required
/// once any `[[node]]` is listed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Federation {
    /// The nodes a check with no `nodes` of its own runs on.
    pub default_nodes: Vec<Id>,
    /// How often each node pushes to the nodes in its `push_to`, with
    /// nothing new if need be, so the push doubles as a heartbeat.
    #[serde(default = "default_push_interval")]
    pub push_interval: Duration,
    /// How long past `push_interval` a node may go unheard before the
    /// nodes it pushes to count it down.
    #[serde(default = "default_grace")]
    pub grace: Duration,
    /// The nodes that alert for outages other nodes report, in order: a
    /// node alerts only while every node ahead of it is down. A node left
    /// out of a non-empty list leaves its own checks and configured
    /// heartbeats to them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alert_order: Vec<Id>,
}

fn default_push_interval() -> Duration {
    Duration::from_secs(15)
}

fn default_grace() -> Duration {
    Duration::from_secs(60)
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
