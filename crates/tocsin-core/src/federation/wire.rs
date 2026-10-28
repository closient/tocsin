// SPDX-License-Identifier: AGPL-3.0-only

//! What one node sends another: the body of `POST /api/v1/federation/batch`,
//! the headers that sign it, and the answer. The pusher writes these and
//! the receiver reads them, so both sides share the types
//! (`docs/design/federation.md`).

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::Health;
use crate::incident::{Timestamp, text_enum};
use crate::status::{CheckKind, OutageEnding};

/// Where a push goes on the receiver.
pub const BATCH_PATH: &str = "/api/v1/federation/batch";

/// The header naming the node that pushed.
pub const NODE_HEADER: &str = "tocsin-node";

/// The header naming the store the rows come from.
pub const STORE_HEADER: &str = "tocsin-store";

/// The header holding the signature over
/// [`push_message`](super::push_message), base64.
pub const SIGNATURE_HEADER: &str = "tocsin-signature";

/// One push: the rows a node's store has past the receiver's cursor, up to
/// a batch's worth, and where the store's own rows end.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Batch {
    /// The node that pushed it; must match `Tocsin-Node`.
    pub node: String,
    /// The store the rows come from, 32 lowercase hex digits; must match
    /// `Tocsin-Store`.
    pub store: String,
    /// When the sender sent it, by its own clock.
    pub sent_at: Timestamp,
    /// The digest of the sender's merged configuration, if it gives one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_digest: Option<String>,
    /// Where the batch starts: the last result id and outage seq this
    /// receiver acknowledged, as the sender's cursor holds them.
    #[serde(default)]
    pub cursor: Positions,
    /// Where the sender's store ends: its last result id and outage seq.
    #[serde(default)]
    pub head: Positions,
    /// Results, in ascending `origin_id`.
    #[serde(default)]
    pub results: Vec<BatchResult>,
    /// Outage changes, in ascending seq.
    #[serde(default)]
    pub outages: Vec<BatchOutageChange>,
    /// The sender's own daily rollups.
    #[serde(default)]
    pub rollups: Vec<BatchRollup>,
}

/// A result id and an outage seq in one store; `null` before the first.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Positions {
    /// A result id.
    #[serde(default)]
    pub results: Option<i64>,
    /// An outage seq.
    #[serde(default)]
    pub outages: Option<i64>,
}

/// A result as the sender's store recorded it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct BatchResult {
    /// Its id in the sender's store.
    pub origin_id: i64,
    /// The node that ran it: the sender.
    pub node_id: String,
    /// Whether `check_id` names a check or a heartbeat.
    pub kind: CheckKind,
    /// The check or heartbeat.
    pub check_id: String,
    /// When the run started.
    pub started_at: Timestamp,
    /// How long the run took.
    pub duration_ms: i64,
    /// The verdict.
    pub health: Health,
    /// How each assertion fared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub detail: Option<serde_json::Value>,
    /// Why the run failed, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The protocol status code, when the check has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
}

/// An outage opening or ending, as the sender's store numbered it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "change", rename_all = "snake_case")]
pub enum BatchOutageChange {
    /// The store opened an outage.
    Open {
        /// Its seq in the sender's store.
        seq: i64,
        /// The outage's id there.
        origin_id: i64,
        /// The node that saw it: the sender.
        node_id: String,
        /// Whether `check_id` names a check or a heartbeat.
        kind: CheckKind,
        /// The failing check or heartbeat.
        check_id: String,
        /// The component the check belongs to, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        component_id: Option<String>,
        /// `degraded` or `down`.
        health: Health,
        /// When it began.
        started_at: Timestamp,
        /// Whether opening it alerted on the sender.
        alerted: bool,
    },
    /// The store ended an outage it opened earlier.
    End {
        /// Its seq in the sender's store.
        seq: i64,
        /// The outage's id there.
        origin_id: i64,
        /// When it ended.
        ended_at: Timestamp,
        /// How it ended.
        ending: OutageEnding,
    },
}

/// One day of one check's results on the sender.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct BatchRollup {
    /// The check or heartbeat.
    pub check_id: String,
    /// The node that recorded the results: the sender.
    pub node_id: String,
    /// The UTC day, `YYYY-MM-DD`.
    pub date: String,
    /// Whether the figures are result counts (a check) or milliseconds (a
    /// heartbeat).
    pub kind: CheckKind,
    /// `up + degraded + down`.
    pub total: i64,
    /// Results, or milliseconds, up.
    pub up: i64,
    /// Results, or milliseconds, degraded.
    pub degraded: i64,
    /// Results, or milliseconds, down.
    pub down: i64,
    /// The worst health of any result that day.
    pub worst: Health,
}

/// What the receiver did with a batch, answered only once it committed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct BatchReceipt {
    /// Results stored now.
    pub results_applied: usize,
    /// Results skipped as older than the receiver keeps; their days are
    /// covered by the sender's rollups.
    pub results_expired: usize,
    /// Outage changes made now.
    pub outage_changes_applied: usize,
    /// Rollups written.
    pub rollups_applied: usize,
    /// The new cursor: the last result id and outage seq the receiver is
    /// done with, the batch's `cursor` where it took none of a kind.
    pub cursor: Positions,
    /// The earliest time a row too far ahead of the receiver's clock says
    /// it happened. It and the rest of its kind wait for the clocks to
    /// agree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skewed_at: Option<Timestamp>,
}

text_enum! {
    /// Whether the receiver takes a store's pushes.
    StoreState, "store state" {
        /// Its pushes are taken.
        Active => "active",
        /// Its node was retired: its pushes are refused, its history kept.
        Retired => "retired",
        /// Two machines pushed as one node; both are refused until an
        /// operator retires one.
        Conflicted => "conflicted",
    }
}

/// The answer to `GET /api/v1/federation/nodes`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct NodeList {
    /// Every `[[node]]` the receiver's configuration lists, in its order.
    pub nodes: Vec<NodeStatus>,
}

/// A configured node, as the receiver sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct NodeStatus {
    /// The node's id.
    pub id: String,
    /// Whether the configuration marks it decommissioned; its pushes are
    /// refused.
    pub retired: bool,
    /// Whether it is the receiver itself.
    pub local: bool,
    /// Each of its stores whose pushes were applied here, oldest first.
    pub stores: Vec<NodeStore>,
    /// Its last push since the receiver started, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_push: Option<LastPush>,
    /// Its liveness heartbeat (`node:<id>`) as the receiver has confirmed
    /// it: `up` while its pushes arrive within `push_interval + grace`;
    /// `null` until confirmed, and for the receiver itself.
    pub liveness: Option<Health>,
    /// Each configured check it has pushed results or outages for, in
    /// configuration order; empty for the receiver itself.
    #[serde(default)]
    pub checks: Vec<RemoteCheck>,
}

/// One of a node's checks, as the receiver last heard of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RemoteCheck {
    /// The check's id.
    pub id: String,
    /// Its confirmed health on that node: from its open outage, else `up`
    /// once it has an `up` result. `null` until then, and whenever the
    /// node's liveness is not `up`, since its last word may be stale.
    pub health: Option<Health>,
    /// Since when it has had that health.
    pub since: Option<Timestamp>,
    /// When its latest result received here started.
    pub last_result_at: Option<Timestamp>,
}

/// One of a node's stores, as the receiver keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct NodeStore {
    /// The store's id, 32 lowercase hex digits.
    pub store: String,
    /// Whether its pushes are taken.
    pub state: StoreState,
    /// When the receiver first applied a push from it, by its own clock.
    pub first_seen_at: Timestamp,
    /// When the receiver last applied one, by its own clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_push_at: Option<Timestamp>,
}

/// What a node's last push said of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct LastPush {
    /// The store it came from.
    pub store: String,
    /// When the node sent it, by its clock.
    pub sent_at: Timestamp,
    /// When the receiver took it, by its own clock.
    pub received_at: Timestamp,
    /// `sent_at` less `received_at`, in seconds: how far ahead the node's
    /// clock is, transit included; negative when behind.
    pub clock_offset_seconds: f64,
    /// The results and outage changes the node had yet to push.
    pub backlog_rows: i64,
    /// The digest of the node's merged configuration, if it gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_digest: Option<String>,
}
