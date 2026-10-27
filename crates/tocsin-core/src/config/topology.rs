// SPDX-License-Identifier: AGPL-3.0-only

//! `[topology]`: the node ids a check's or heartbeat's `topology_node` may
//! name, so a misspelt one fails the load instead of grouping its alerts
//! under a node nothing else names.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::values::TopologyNode;

/// `[topology]`: the known nodes of the map that `topology_node` refers to.
/// Without it, any well-formed id is allowed.
///
/// The list is usually generated from the map itself into a file of its
/// own in the configuration folder, so a node added to the map is allowed
/// on the next reload.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Topology {
    /// Every node id a `topology_node` may name.
    #[serde(default)]
    pub nodes: Vec<TopologyNode>,
}
