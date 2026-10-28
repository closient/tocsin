// SPDX-License-Identifier: AGPL-3.0-only

//! What the engine runs, compiled from a configuration.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use tocsin_core::config::{
    ComponentRule, Config, Heartbeat, Id, LIVENESS_HEARTBEAT, QUORUM_HEARTBEAT,
};
use tocsin_core::probe::CompiledCheck;

/// The checks, heartbeats and components the engine runs, compiled once.
#[derive(Debug, Clone)]
pub struct Plan {
    pub(crate) node_id: String,
    pub(crate) max_concurrent: usize,
    pub(crate) checks: Vec<Arc<CompiledCheck>>,
    /// The part each quorum check's local run plays, by check id; any
    /// other check's is [`Role::Own`].
    pub(crate) roles: HashMap<String, Role>,
    pub(crate) heartbeats: Vec<Heartbeat>,
    /// The quorum checks this node evaluates.
    pub(crate) quorums: Vec<Arc<QuorumPlan>>,
    pub(crate) components: Vec<(String, ComponentRule)>,
}

/// The part a check's run on this node plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// It is the check's state here, and opens alerts of its own.
    Own,
    /// A quorum check another node evaluates: it is the check's state on
    /// this node's own pages, and opens no alert, since the evaluator
    /// alerts for the check as a whole.
    Quiet,
    /// A quorum check this node evaluates: one node's evidence, which
    /// opens no alert and is not shown as the check's state; the
    /// evaluation is.
    Evidence,
}

/// A quorum check this node evaluates: its state is the one at least
/// `quorum` of its `nodes` agree on, recorded under
/// [`QUORUM_NODE`](tocsin_core::config::QUORUM_NODE)
/// (`docs/design/federation.md`, "Multi-location checks with a quorum").
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QuorumPlan {
    pub(crate) check_id: String,
    pub(crate) component: Option<String>,
    pub(crate) nodes: Vec<String>,
    pub(crate) quorum: usize,
    /// How often it is evaluated: the check's interval.
    pub(crate) interval: Duration,
    /// How recent a node's latest result must be to count, in
    /// milliseconds: [`Config::quorum_window`].
    pub(crate) window_ms: i64,
    /// The `quorum:<check>` heartbeat each evaluation pings, if the
    /// configuration has it.
    pub(crate) heartbeat: Option<String>,
    /// The `node:<id>` heartbeats of the nodes ahead of this one in
    /// `[federation] alert_order`, which must all be down before this one
    /// opens an alert for the check; `None` when it never does.
    pub(crate) ahead: Option<Vec<String>>,
}

/// A configuration the engine cannot run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanError(pub String);

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PlanError {}

impl Plan {
    /// Compiles `config`'s checks, heartbeats and components: in a
    /// federation, those placed on this node, and an evaluation of each
    /// quorum check this node evaluates.
    ///
    /// # Errors
    ///
    /// The configuration has no `node_id` (the loader fills in the
    /// hostname, so this means it did not come from the loader), a check's
    /// assertions do not compile, or `max_concurrent_checks` is zero.
    pub fn from_config(config: &Config) -> Result<Plan, PlanError> {
        let node_id = config
            .server
            .node_id
            .clone()
            .ok_or_else(|| PlanError("the configuration has no node_id".to_owned()))?;
        let max_concurrent = usize::try_from(config.server.max_concurrent_checks)
            .map_err(|_| PlanError("max_concurrent_checks is too large".to_owned()))?;
        if max_concurrent == 0 {
            return Err(PlanError(
                "max_concurrent_checks must be at least 1".to_owned(),
            ));
        }
        let placed = config
            .checks
            .iter()
            .filter(|check| config.runs_check(check, &node_id));
        let checks = placed
            .clone()
            .map(|check| {
                CompiledCheck::new(check.clone())
                    .map(Arc::new)
                    .map_err(|message| PlanError(format!("check {}: {message}", check.id)))
            })
            .collect::<Result<_, _>>()?;
        let components = config
            .components
            .iter()
            .map(|component| (component.id.to_string(), component.rule))
            .collect();
        let roles = placed
            .filter(|check| config.quorum(check).is_some())
            .map(|check| {
                let role = if config.evaluates_quorum(check, &node_id) {
                    Role::Evidence
                } else {
                    Role::Quiet
                };
                (check.id.to_string(), role)
            })
            .collect();
        let heartbeats: Vec<Heartbeat> = config
            .heartbeats
            .iter()
            .filter(|heartbeat| config.watches_heartbeat(heartbeat, &node_id))
            .cloned()
            .collect();
        let quorums = quorum_plans(config, &node_id, &heartbeats);
        Ok(Plan {
            node_id,
            max_concurrent,
            checks,
            roles,
            heartbeats,
            quorums,
            components,
        })
    }

    /// The node every result is recorded under.
    #[must_use]
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// The compiled checks.
    #[must_use]
    pub fn checks(&self) -> &[Arc<CompiledCheck>] {
        &self.checks
    }

    /// The heartbeats.
    #[must_use]
    pub fn heartbeats(&self) -> &[Heartbeat] {
        &self.heartbeats
    }
}

/// An evaluation of each quorum check the node `node_id` evaluates, given
/// the `heartbeats` it watches.
fn quorum_plans(config: &Config, node_id: &str, heartbeats: &[Heartbeat]) -> Vec<Arc<QuorumPlan>> {
    let ahead: Option<Vec<String>> = config.alerts_behind(node_id).map(|nodes| {
        nodes
            .iter()
            .map(|node| Id::synthetic(LIVENESS_HEARTBEAT, node).to_string())
            .collect()
    });
    config
        .checks
        .iter()
        .filter(|check| config.evaluates_quorum(check, node_id))
        .filter_map(|check| {
            let quorum = config.quorum(check)?;
            let heartbeat = Id::synthetic(QUORUM_HEARTBEAT, &check.id).to_string();
            Some(Arc::new(QuorumPlan {
                check_id: check.id.to_string(),
                component: check.component.as_ref().map(ToString::to_string),
                nodes: config
                    .check_nodes(check)
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
                quorum,
                interval: check.interval.get(),
                window_ms: i64::try_from(config.quorum_window(check).as_millis())
                    .unwrap_or(i64::MAX),
                heartbeat: heartbeats
                    .iter()
                    .any(|watched| watched.id.as_str() == heartbeat)
                    .then_some(heartbeat),
                ahead: ahead.clone(),
            }))
        })
        .collect()
}
