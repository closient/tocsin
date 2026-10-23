// SPDX-License-Identifier: AGPL-3.0-only

//! What the engine announces: confirmed check state changes and component
//! status changes. Nothing is emitted for a result that changes nothing.

use serde::Serialize;
use tocsin_core::Health;
use tocsin_core::state::ComponentStatus;

/// One change the engine confirmed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EngineEvent {
    /// A check's confirmed state changed.
    CheckChanged(CheckChange),
    /// A component's displayed status changed.
    ComponentChanged(ComponentChange),
}

/// A check's confirmed state changed: emitted once per change, after
/// `confirm` agreeing results, never once per result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckChange {
    /// The check.
    pub check_id: String,
    /// The check's component, if it has one.
    pub component_id: Option<String>,
    /// The state before, or `None` when the check had no state yet: the
    /// first state after the engine starts, unless an open outage carried
    /// one over from before a restart.
    pub from: Option<Health>,
    /// The state after.
    pub to: Health,
    /// When the first result of the confirming run started (unix
    /// milliseconds). The outage records use this time too.
    pub since_ms: i64,
    /// When the result that completed the run started.
    pub confirmed_at_ms: i64,
    /// The outage this change ended, if the check was degraded or down.
    pub ended_outage_id: Option<i64>,
    /// The outage this change opened, if the check is now degraded or down.
    pub opened_outage_id: Option<i64>,
    /// True when a maintenance window covering the check's component was
    /// active at the change. The change is recorded as usual, but alerting
    /// must not notify anyone about it.
    pub maintenance: bool,
}

impl CheckChange {
    /// Whether alerting should notify about this change: false during
    /// maintenance.
    #[must_use]
    pub fn alerts(&self) -> bool {
        !self.maintenance
    }
}

/// A component's displayed status changed, because a check's state changed,
/// or an incident or maintenance window started or ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ComponentChange {
    /// The component.
    pub component_id: String,
    /// The status before.
    pub from: ComponentStatus,
    /// The status after.
    pub to: ComponentStatus,
    /// The component's health from its checks alone, which `to` may hide
    /// during maintenance or override with a worse incident.
    pub health: Option<Health>,
    /// When the engine saw the change (unix milliseconds).
    pub at_ms: i64,
}
