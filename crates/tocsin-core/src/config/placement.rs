// SPDX-License-Identifier: AGPL-3.0-only

//! Where each check and heartbeat runs in a federation, and which checks
//! are quorum checks: `docs/design/federation.md`, "Placement" and
//! "Multi-location checks with a quorum".

use super::values::{Duration, Id};
use super::{Check, Config, Heartbeat};

/// The kind of the heartbeat a node keeps for each node that pushes to it,
/// `node:<id>`, pinged by its pushes.
pub const LIVENESS_HEARTBEAT: &str = "node";

/// The node a quorum check's evaluated results and outages are recorded
/// under, since no one node saw them. No node id holds `*`.
pub const QUORUM_NODE: &str = "*";

/// The kind of the heartbeat a node keeps for each quorum check it
/// evaluates, `quorum:<check>`: see [`Config::quorum_heartbeats`].
pub const QUORUM_HEARTBEAT: &str = "quorum";

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
        Some(check.quorum.map_or(nodes / 2 + 1, |quorum| {
            usize::try_from(quorum).unwrap_or(usize::MAX)
        }))
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
                        listed.id == *node && listed.push_to.iter().any(|to| to.as_str() == node_id)
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

    /// How long a node's latest result for the quorum check `check` counts
    /// as evidence of its health: two intervals, so one late run does not
    /// lose it, plus the time a pushed result can take to arrive,
    /// `[federation] push_interval` and `grace`.
    #[must_use]
    pub fn quorum_window(&self, check: &Check) -> std::time::Duration {
        let interval = check.interval.get();
        let travel = self
            .federation
            .as_ref()
            .map_or(std::time::Duration::ZERO, |federation| {
                federation
                    .push_interval
                    .get()
                    .saturating_add(federation.grace.get())
            });
        interval.saturating_mul(2).saturating_add(travel)
    }

    /// The heartbeats the node `node_id` keeps for the quorum checks it
    /// evaluates, `quorum:<check>`: each is pinged up while the check's
    /// nodes give enough fresh evidence to decide its health, down while
    /// they do not, and missed if the evaluation stops. It carries the
    /// check's labels, annotations and class, so its alerts look like the
    /// check's, and no component, so it is on the internal page only.
    #[must_use]
    pub fn quorum_heartbeats(&self, node_id: &str) -> Vec<Heartbeat> {
        self.checks
            .iter()
            .filter(|check| self.evaluates_quorum(check, node_id))
            .map(|check| Heartbeat {
                id: Id::synthetic(QUORUM_HEARTBEAT, &check.id),
                name: Some(format!(
                    "Quorum for {}",
                    check.name.as_deref().unwrap_or(check.id.as_str())
                )),
                component: None,
                interval: check.interval,
                grace: Duration::from_std(self.quorum_window(check)),
                nodes: Vec::new(),
                links: check.links.clone(),
                class: check.class,
                topology_node: check.topology_node.clone(),
                labels: check.labels.clone(),
                annotations: check.annotations.clone(),
            })
            .collect()
    }

    /// The check a `quorum:<check>` heartbeat id stands for, if
    /// `heartbeat_id` is one and the check exists.
    #[must_use]
    pub fn quorum_check_of(&self, heartbeat_id: &str) -> Option<&Check> {
        let check_id = heartbeat_id
            .strip_prefix(QUORUM_HEARTBEAT)?
            .strip_prefix(':')?;
        self.checks
            .iter()
            .find(|check| check.id.as_str() == check_id)
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

    /// Whether `[federation] alert_order` names other nodes but not
    /// `node_id`, and `node_id` pushes to one of them. Such a node leaves
    /// paging for its own checks and heartbeats to the nodes in the list,
    /// which page for them from its pushes; it still pages for those tocsin
    /// keeps (`node:<id>`, `upstream:<id>`, `quorum:<check>`), which each
    /// node watches itself. A node outside the list that pushes to none of
    /// them keeps paging for its own: nobody else would. False without a
    /// federation.
    #[must_use]
    pub fn outside_alert_order(&self, node_id: &str) -> bool {
        self.federation.as_ref().is_some_and(|federation| {
            let listed = |id: &Id| federation.alert_order.contains(id);
            !federation
                .alert_order
                .iter()
                .any(|node| node.as_str() == node_id)
                && self
                    .nodes
                    .iter()
                    .find(|node| node.id.as_str() == node_id)
                    .is_some_and(|node| node.push_to.iter().any(listed))
        })
    }
}
