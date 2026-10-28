// SPDX-License-Identifier: AGPL-3.0-only

//! Where each check and heartbeat runs in a federation, and which checks
//! are quorum checks: `docs/design/federation.md`, "Placement" and
//! "Multi-location checks with a quorum".

use super::values::Id;
use super::{Check, Config, Heartbeat};

impl Config {
    /// The nodes `check` runs on: its own `nodes`, else `[federation]
    /// default_nodes`. Empty without a federation, where the one server
    /// runs everything.
    #[must_use]
    pub fn check_nodes<'a>(&'a self, check: &'a Check) -> &'a [Id] {
        if !check.nodes.is_empty() {
            return &check.nodes;
        }
        self.federation
            .as_ref()
            .map_or(&[], |federation| &federation.default_nodes)
    }

    /// The nodes `heartbeat` is watched on: its own `nodes`, else
    /// `[federation] default_nodes`. Validation keeps it to one node in a
    /// federation; empty without one.
    #[must_use]
    pub fn heartbeat_nodes<'a>(&'a self, heartbeat: &'a Heartbeat) -> &'a [Id] {
        if !heartbeat.nodes.is_empty() {
            return &heartbeat.nodes;
        }
        self.federation
            .as_ref()
            .map_or(&[], |federation| &federation.default_nodes)
    }

    /// Whether the node `node_id` runs `check`: every check runs on a server
    /// outside a federation.
    #[must_use]
    pub fn runs_check(&self, check: &Check, node_id: &str) -> bool {
        self.federation.is_none()
            || self
                .check_nodes(check)
                .iter()
                .any(|node| node.as_str() == node_id)
    }

    /// Whether the node `node_id` watches `heartbeat`'s deadline. A
    /// heartbeat tocsin keeps itself (`node:<id>`) is outside placement:
    /// [`Id::parse`] refuses its colon, so no `[[heartbeat]]` has one.
    #[must_use]
    pub fn watches_heartbeat(&self, heartbeat: &Heartbeat, node_id: &str) -> bool {
        self.federation.is_none()
            || heartbeat.id.as_str().contains(':')
            || self
                .heartbeat_nodes(heartbeat)
                .iter()
                .any(|node| node.as_str() == node_id)
    }

    /// How many of `check`'s nodes must agree on a health for it to be the
    /// check's, if it is a quorum check: one run on two or more nodes. Its
    /// own `quorum`, else a majority of its nodes.
    #[must_use]
    pub fn quorum(&self, check: &Check) -> Option<usize> {
        let nodes = self.check_nodes(check).len();
        if nodes < 2 {
            return None;
        }
        Some(
            check
                .quorum
                .map_or(nodes / 2 + 1, |quorum| usize::try_from(quorum).unwrap_or(usize::MAX)),
        )
    }

    /// Whether `check`'s results all converge on the node `node_id`, so it
    /// derives the check's state: `check` is a quorum check and each of
    /// its nodes is `node_id` or pushes to it.
    #[must_use]
    pub fn evaluates_quorum(&self, check: &Check, node_id: &str) -> bool {
        self.quorum(check).is_some()
            && self.check_nodes(check).iter().all(|node| {
                node.as_str() == node_id
                    || self.nodes.iter().any(|listed| {
                        listed.id == *node
                            && listed.push_to.iter().any(|to| to.as_str() == node_id)
                    })
            })
    }

    /// The nodes that derive `check`'s state, in `[[node]]` order: those
    /// [`Config::evaluates_quorum`] holds for, retired ones aside.
    #[must_use]
    pub fn quorum_evaluators(&self, check: &Check) -> Vec<&Id> {
        self.nodes
            .iter()
            .filter(|node| !node.retired && self.evaluates_quorum(check, node.id.as_str()))
            .map(|node| &node.id)
            .collect()
    }

    /// The nodes that must all be down before the node `node_id` opens an
    /// alert it derived itself, such as a quorum check's: those ahead of it
    /// in `[federation] alert_order`. None when the list names other nodes
    /// but not this one, which then never opens one; empty when the list is
    /// empty or this node heads it.
    #[must_use]
    pub fn alerts_behind(&self, node_id: &str) -> Option<&[Id]> {
        let federation = self.federation.as_ref()?;
        if federation.alert_order.is_empty() {
            return Some(&[]);
        }
        let position = federation
            .alert_order
            .iter()
            .position(|node| node.as_str() == node_id)?;
        Some(&federation.alert_order[..position])
    }
}
