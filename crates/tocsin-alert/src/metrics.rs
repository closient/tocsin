// SPDX-License-Identifier: AGPL-3.0-only

//! Alerting's Prometheus metrics.

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::registry::Registry;

/// The label set: which notifier.
type NotifierLabel = Vec<(&'static str, String)>;

fn label(notifier: &str) -> NotifierLabel {
    vec![("notifier", notifier.to_owned())]
}

/// Alerting's counters, cheap to clone.
#[derive(Debug, Clone, Default)]
pub struct AlertMetrics {
    attempt_failures: Family<NotifierLabel, Counter>,
    abandoned: Family<NotifierLabel, Counter>,
}

impl AlertMetrics {
    /// Registers the counters in `registry`:
    ///
    /// - `tocsin_notification_attempt_failures_total{notifier}`, every
    ///   failed attempt to deliver an alert message, retried or not;
    /// - `tocsin_notifications_abandoned_total{notifier}`, every message
    ///   given up on: refused outright, or still failing after the last
    ///   retry. An operator alerts on this one.
    #[must_use]
    pub fn register(registry: &mut Registry) -> Self {
        let metrics = AlertMetrics::default();
        registry.register(
            "tocsin_notification_attempt_failures",
            "Failed attempts to deliver an alert message, retried or not, by notifier",
            metrics.attempt_failures.clone(),
        );
        registry.register(
            "tocsin_notifications_abandoned",
            "Alert messages given up on after their last attempt failed, by notifier",
            metrics.abandoned.clone(),
        );
        metrics
    }

    /// Counts one failed attempt on `notifier`.
    pub fn attempt_failed(&self, notifier: &str) {
        self.attempt_failures.get_or_create(&label(notifier)).inc();
    }

    /// Counts one message `notifier` will not be sent.
    pub fn abandoned(&self, notifier: &str) {
        self.abandoned.get_or_create(&label(notifier)).inc();
    }

    /// Failed attempts on `notifier` so far.
    #[must_use]
    pub fn attempt_failures(&self, notifier: &str) -> u64 {
        self.attempt_failures.get_or_create(&label(notifier)).get()
    }

    /// Messages given up on for `notifier` so far.
    #[must_use]
    pub fn abandonments(&self, notifier: &str) -> u64 {
        self.abandoned.get_or_create(&label(notifier)).get()
    }
}
