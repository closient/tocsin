// SPDX-License-Identifier: AGPL-3.0-only

//! What the engine runs, compiled from a configuration.

use std::fmt;
use std::sync::Arc;

use tocsin_core::config::{ComponentRule, Config, Heartbeat};
use tocsin_core::probe::CompiledCheck;

/// The checks, heartbeats and components the engine runs, compiled once.
#[derive(Debug, Clone)]
pub struct Plan {
    pub(crate) node_id: String,
    pub(crate) max_concurrent: usize,
    pub(crate) checks: Vec<Arc<CompiledCheck>>,
    pub(crate) heartbeats: Vec<Heartbeat>,
    pub(crate) components: Vec<(String, ComponentRule)>,
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
    /// Compiles `config`'s checks, heartbeats and components.
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
        let checks = config
            .checks
            .iter()
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
        Ok(Plan {
            node_id,
            max_concurrent,
            checks,
            heartbeats: config.heartbeats.clone(),
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
