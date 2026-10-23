// SPDX-License-Identifier: AGPL-3.0-only

//! The read API's documents: the current status, one component, the
//! checks, a check's raw results and outages. The server writes them and
//! the CLI reads them back, so both sides share these types.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::Health;
use crate::incident::{Incident, Timestamp};
use crate::state::ComponentStatus;

/// The whole page's status: the worst of `statuses`, ranked operational,
/// unknown, maintenance, degraded, down. With no components it is
/// unknown: nothing says the service works.
#[must_use]
pub fn overall(statuses: impl IntoIterator<Item = ComponentStatus>) -> ComponentStatus {
    statuses
        .into_iter()
        .max_by_key(|status| severity(*status))
        .unwrap_or(ComponentStatus::Unknown)
}

const fn severity(status: ComponentStatus) -> u8 {
    match status {
        ComponentStatus::Operational => 0,
        ComponentStatus::Unknown => 1,
        ComponentStatus::Maintenance => 2,
        ComponentStatus::Degraded => 3,
        ComponentStatus::Down => 4,
    }
}

/// `GET /api/v1/status.json`: everything a status page shows right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct StatusDocument {
    /// The `[page]` title.
    pub title: String,
    /// The worst component status: operational, unknown, maintenance,
    /// degraded, down, in that order; unknown with no components.
    pub status: ComponentStatus,
    /// When the server read the status.
    pub generated_at: Timestamp,
    /// Every `[[component]]`, in configuration order.
    pub components: Vec<ComponentState>,
    /// Incidents not yet resolved and maintenance not yet completed,
    /// scheduled ones included, newest first.
    pub incidents: Vec<Incident>,
}

/// One `[[component]]` and what the status page shows for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ComponentState {
    /// Its id.
    pub id: String,
    /// Its display name.
    pub name: String,
    /// The line shown with it.
    pub description: Option<String>,
    /// The `[[group]]` it is shown under.
    pub group: Option<String>,
    /// What the status page shows: its checks' health, overridden by
    /// incidents and maintenance. `unknown` until a check reports.
    pub status: ComponentStatus,
    /// Its health from its checks alone; `null` until one reports.
    pub health: Option<Health>,
}

/// `GET /api/v1/components/{id}`: one component, its checks and the open
/// incidents naming it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ComponentDetail {
    /// The component.
    pub component: ComponentState,
    /// The checks and heartbeats that set its health.
    pub checks: Vec<CheckState>,
    /// Open incidents and maintenance naming it, newest first.
    pub incidents: Vec<Incident>,
}

/// Whether a member of a component is probed or pinged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    /// A `[[check]]`: tocsin probes it.
    Check,
    /// A `[[heartbeat]]`: it pings tocsin.
    Heartbeat,
}

/// `GET /api/v1/checks`: every check and heartbeat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CheckList {
    /// Checks, then heartbeats, each in configuration order.
    pub checks: Vec<CheckState>,
}

/// One check or heartbeat and its current state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CheckState {
    /// Its id; checks and heartbeats never share one.
    pub id: String,
    /// Check or heartbeat.
    pub kind: CheckKind,
    /// Its display name: the id when none is configured.
    pub name: String,
    /// The `[[component]]` whose health it sets.
    pub component: Option<String>,
    /// The probe a check runs (`http`, `tcp`, `dns`, `icmp`, `tls`);
    /// `null` for a heartbeat.
    #[serde(rename = "type")]
    pub check_type: Option<String>,
    /// How often it runs, or how often a heartbeat expects a ping, such
    /// as `30s` or `1h30m`.
    pub interval: String,
    /// What a check probes. Only with a `status:read` token, since it can
    /// name internal hosts; absent otherwise and for heartbeats.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Its confirmed health; `null` until confirmed.
    pub health: Option<Health>,
    /// Since when it has had that health.
    pub since: Option<Timestamp>,
    /// Its latest result's health, which may not be confirmed yet.
    pub last_health: Option<Health>,
    /// When its latest result started.
    pub last_result_at: Option<Timestamp>,
}

/// `GET /api/v1/checks/{id}/results`: one page of raw results, newest
/// first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct CheckResults {
    /// The check or heartbeat.
    pub check_id: String,
    /// The start of the window: results that started at or after it.
    pub since: Timestamp,
    /// Newest first.
    pub results: Vec<CheckResult>,
    /// Pass as `before`, with the same `since`, for the next page; `null`
    /// on the last.
    pub next_before: Option<i64>,
}

/// One probe run, as stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct CheckResult {
    /// Its id.
    pub id: i64,
    /// The node that ran it.
    pub node_id: String,
    /// When it started.
    pub started_at: Timestamp,
    /// How long it took.
    pub duration_ms: i64,
    /// The verdict.
    pub health: Health,
    /// Why it failed, when it did.
    pub error: Option<String>,
    /// The protocol status code, when the check has one (HTTP).
    pub status_code: Option<u16>,
    /// How each assertion fared, as `tocsin probe run` reports it.
    /// Diagnostic: its shape may change between versions.
    #[schema(value_type = Option<Object>)]
    pub detail: Option<serde_json::Value>,
}

/// `GET /api/v1/outages`: one page of outages, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OutageList {
    /// The start of the window: outages still open at or after it.
    pub since: Timestamp,
    /// Newest first, by start.
    pub outages: Vec<OutageRecord>,
    /// Pass as `before`, with the same `since`, for the next page; `null`
    /// on the last.
    pub next_before: Option<i64>,
}

/// A machine-detected outage: a check confirmed degraded or down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OutageRecord {
    /// Its id.
    pub id: i64,
    /// The node that saw it.
    pub node_id: String,
    /// The failing check.
    pub check_id: String,
    /// The check's component when the outage opened, if any.
    pub component_id: Option<String>,
    /// `degraded` or `down`.
    pub health: Health,
    /// When it began.
    pub started_at: Timestamp,
    /// When it ended; `null` while it is open.
    pub ended_at: Option<Timestamp>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_worst_status_wins() {
        use ComponentStatus::{Degraded, Down, Maintenance, Operational, Unknown};
        assert_eq!(overall([]), Unknown);
        assert_eq!(overall([Operational, Operational]), Operational);
        assert_eq!(overall([Operational, Unknown]), Unknown);
        assert_eq!(overall([Unknown, Maintenance, Operational]), Maintenance);
        assert_eq!(overall([Maintenance, Degraded]), Degraded);
        assert_eq!(overall([Down, Degraded, Operational]), Down);
    }
}
