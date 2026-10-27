// SPDX-License-Identifier: AGPL-3.0-only

//! The Alertmanager v2 alerts API.
//!
//! Alertmanager expects a firing alert to be posted again and again: an
//! alert not re-posted before its `endsAt` resolves on its own. tocsin sends
//! a firing alert's `endsAt` [`FIRING_HOLD`] after sending it, and re-posts
//! it every [`REPOST_INTERVAL`], well inside that hold; the resolve sets
//! `endsAt` to when the outage ended.
//!
//! Alertmanager identifies an alert by its whole label set, so tocsin's own
//! labels hold the identity and nothing else: `alertname`, `node`,
//! `subject_kind` and `subject_id`, fixed when the alert opens. Anything of
//! tocsin's that can change while it fires, such as the component or the
//! health, is an annotation; a changed label would open a second alert.
//!
//! The check's or heartbeat's configured `labels` join the label set too,
//! because routing is what labels are for. They are pinned on the alert
//! when it opens, with its configured `annotations`, and every fire, repeat
//! and resolve posts that set: a reload that changes a firing check's
//! labels takes effect from its next alert, never mid-episode. A resolve
//! already queued when a reload removes the check still carries the pinned
//! labels; a check removed while it fires is withdrawn without a resolve,
//! and Alertmanager expires its alert at `endsAt`.

use std::time::Duration;

use serde_json::{Map, Value, json};
use tocsin_engine::ports::BoxFuture;
use tocsin_store::SubjectKind;

use crate::alert::{Alert, rfc3339};
use crate::notifier::{Notifier, Redactor, SendError};

/// How far ahead of now a firing alert's `endsAt` is.
pub const FIRING_HOLD: Duration = Duration::from_mins(5);
/// How often a firing alert is re-posted.
pub const REPOST_INTERVAL: Duration = Duration::from_secs(60);

/// Posts to Alertmanager.
#[derive(Debug)]
pub struct Alertmanager {
    client: reqwest::Client,
    url: String,
    bearer_token: Option<String>,
    redactor: Redactor,
}

impl Alertmanager {
    /// A notifier posting to the Alertmanager at `url` (its base, without
    /// `/api/v2/alerts`), sending `bearer_token` if given.
    #[must_use]
    pub fn new(client: reqwest::Client, url: &str, bearer_token: Option<String>) -> Self {
        let redactor = Redactor::new(bearer_token.as_deref());
        Alertmanager {
            client,
            url: format!("{}/api/v2/alerts", url.trim_end_matches('/')),
            bearer_token,
            redactor,
        }
    }
}

/// The alert's labels: its identity, the same on every post, and the
/// subject's configured `labels` as pinned when the alert opened, which
/// tocsin's own outrank. A test alert
/// has an `alertname` of its own, so it never pairs with a real one.
#[must_use]
pub fn labels(alert: &Alert) -> Map<String, Value> {
    let alertname = match (alert.test, alert.subject_kind) {
        (true, _) => "TocsinTestAlert",
        (false, SubjectKind::Check) => "TocsinCheckFailing",
        (false, SubjectKind::Heartbeat) => "TocsinHeartbeatFailing",
    };
    let mut labels: Map<String, Value> = alert
        .labels
        .iter()
        .map(|(name, value)| (name.clone(), value.clone().into()))
        .collect();
    labels.insert("alertname".into(), alertname.into());
    labels.insert("node".into(), alert.node_id.clone().into());
    labels.insert("subject_kind".into(), alert.subject_kind.as_str().into());
    labels.insert("subject_id".into(), alert.subject_id.clone().into());
    labels
}

/// The alert's annotations: everything about it that may change while it
/// fires.
#[must_use]
pub fn annotations(alert: &Alert) -> Map<String, Value> {
    let mut annotations: Map<String, Value> = alert
        .annotations()
        .into_iter()
        .map(|(name, value)| (name, value.into()))
        .collect();
    annotations.insert("summary".into(), alert.title().into());
    annotations.insert("description".into(), alert.body().into());
    annotations.insert("health".into(), alert.health.as_str().into());
    annotations.insert("name".into(), alert.subject_name.clone().into());
    if let Some(component) = &alert.component_id {
        annotations.insert("component".into(), component.clone().into());
    }
    if let Some(name) = &alert.component_name {
        annotations.insert("component_name".into(), name.clone().into());
    }
    if alert.test {
        annotations.insert("tocsin_test".into(), "true".into());
    }
    annotations
}

/// The body posted for `alert`: a list holding one alert.
#[must_use]
pub fn payload(alert: &Alert) -> Value {
    let ends_at = match alert.ended_at_ms {
        Some(ended) if alert.is_resolve() => ended,
        _ => alert
            .sent_at_ms
            .saturating_add(i64::try_from(FIRING_HOLD.as_millis()).unwrap_or(i64::MAX)),
    };
    json!([{
        "labels": labels(alert),
        "annotations": annotations(alert),
        "startsAt": rfc3339(alert.started_at_ms),
        "endsAt": rfc3339(ends_at),
    }])
}

impl Notifier for Alertmanager {
    fn send<'a>(&'a self, alert: &'a Alert) -> BoxFuture<'a, Result<(), SendError>> {
        Box::pin(async move {
            let mut request = self.client.post(&self.url).json(&payload(alert));
            if let Some(token) = &self.bearer_token {
                request = request.bearer_auth(token);
            }
            self.redactor.send(request).await.map(drop)
        })
    }
}
