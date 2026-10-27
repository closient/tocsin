// SPDX-License-Identifier: AGPL-3.0-only

//! The alert history API's document: each alert tocsin sent, from fire to
//! resolve, with the labels its messages carried and how each message's
//! delivery went. The server writes it and the CLI reads it back.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::Health;
use crate::incident::{Timestamp, text_enum};
use crate::status::CheckKind;

text_enum! {
    /// Whether an alert still fires.
    AlertState, "alert state" {
        /// Its check or heartbeat is still out.
        Firing => "firing",
        /// It ended, and its resolve was queued.
        Resolved => "resolved",
    }
}

text_enum! {
    /// Why an alert ended.
    AlertEnding, "ending" {
        /// Its check or heartbeat recovered.
        Recovery => "recovery",
        /// A reload or restart took its check, heartbeat or notifier out of
        /// the configuration, so nobody saw it recover.
        Config => "config",
    }
}

text_enum! {
    /// Which message a delivery carries.
    MessageKind, "message kind" {
        /// The alert started, or its health changed.
        Fire => "fire",
        /// The alert still fires; sent on its route's `repeat` interval.
        Repeat => "repeat",
        /// The alert ended.
        Resolve => "resolve",
        /// Stops a message that rings until acknowledged (a Pushover
        /// emergency).
        Cancel => "cancel",
    }
}

text_enum! {
    /// How a delivery stands.
    DeliveryStatus, "delivery status" {
        /// Not sent yet, or failed and waiting for its next attempt:
        /// `attempts` and `last_error` say which.
        Pending => "pending",
        /// The notifier accepted it.
        Delivered => "delivered",
        /// tocsin gave up on it: the notifier refused it for good, or every
        /// attempt failed.
        Failed => "failed",
        /// A silence held it back; `silence_id` names it.
        Silenced => "silenced",
    }
}

/// One message queued for an alert, and how its delivery went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AlertDelivery {
    /// Its id in the outbox.
    pub id: i64,
    /// Fire, repeat, resolve or cancel.
    pub kind: MessageKind,
    /// The alert's health when it was queued.
    pub health: Health,
    /// When it was queued.
    pub created_at: Timestamp,
    /// Pending, delivered, failed or silenced.
    pub status: DeliveryStatus,
    /// Attempts made so far.
    pub attempts: u32,
    /// The last failed attempt's error, secrets redacted; `null` if none
    /// failed.
    pub last_error: Option<String>,
    /// When it was delivered, given up on or held back; `null` while
    /// pending.
    pub finished_at: Option<Timestamp>,
    /// The silence that held it back, if one did.
    pub silence_id: Option<i64>,
}

/// One notifier's alert for one check or heartbeat on one node, from fire
/// to resolve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AlertRecord {
    /// Its id.
    pub id: i64,
    /// The node that saw it.
    pub node_id: String,
    /// Whether `subject_id` is a check or a heartbeat.
    pub subject_kind: CheckKind,
    /// The check or heartbeat.
    pub subject_id: String,
    /// The subject's component when it last fired, if any.
    pub component_id: Option<String>,
    /// The notifier it was sent to.
    pub notifier_id: String,
    /// The route that sent it there.
    pub route_id: String,
    /// Its latest health.
    pub health: Health,
    /// Firing or resolved.
    pub state: AlertState,
    /// When the outage it first fired for started.
    pub started_at: Timestamp,
    /// When it ended; `null` while it fires.
    pub ended_at: Option<Timestamp>,
    /// Why it ended; `null` while it fires.
    pub ended_by: Option<AlertEnding>,
    /// The labels every Alertmanager message of it carries, fire, repeat
    /// and resolve alike: the subject's configured labels as they were when
    /// it opened, its `class`, and `alertname`, `node`, `subject_kind` and
    /// `subject_id`. Other notifiers get the same identity.
    pub labels: BTreeMap<String, String>,
    /// The subject's configured annotations as they were when it opened.
    /// An Alertmanager message adds `summary`, `description` and the rest.
    pub annotations: BTreeMap<String, String>,
    /// Every message queued for it that is still kept, oldest first.
    pub deliveries: Vec<AlertDelivery>,
}

/// `GET /api/v1/alerts/history`: one page of alerts, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AlertHistory {
    /// The start of the window: alerts still firing at or after it.
    pub since: Timestamp,
    /// Newest first, by start.
    pub alerts: Vec<AlertRecord>,
    /// Pass as `before`, with the same `since` and `state`, for the next
    /// page; `null` on the last.
    pub next_before: Option<i64>,
}
