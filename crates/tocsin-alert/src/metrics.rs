// SPDX-License-Identifier: AGPL-3.0-only

//! Alerting's Prometheus metrics.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};

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
    failures: Family<NotifierLabel, Counter>,
    abandoned: Family<NotifierLabel, Counter>,
    /// Unlabelled: its usual cause is a notifier the configuration no
    /// longer has, whose labelled series [`AlertMetrics::retain`] drops.
    alerts_abandoned: Counter,
    /// Every notifier either family has a series for, which a family
    /// cannot list, so [`AlertMetrics::retain`] can find the stale ones.
    notifiers: Arc<Mutex<BTreeSet<String>>>,
}

impl AlertMetrics {
    /// Registers the counters in `registry`:
    ///
    /// - `tocsin_notification_failures_total{notifier}`, every failed
    ///   attempt to deliver an alert message, retried or not;
    /// - `tocsin_notifications_abandoned_total{notifier}`, every message
    ///   given up on: refused outright, or still failing after the last
    ///   retry. An operator alerts on this one.
    /// - `tocsin_alerts_abandoned_total`, every alert that ended with no
    ///   resolve delivered: its resolve was given up on, most often because
    ///   its notifier was removed while tocsin was stopped. Not labelled by
    ///   notifier, since a removed notifier's series is not exported.
    #[must_use]
    pub fn register(registry: &mut Registry) -> Self {
        let metrics = AlertMetrics::default();
        registry.register(
            "tocsin_notification_failures",
            "Failed attempts to deliver an alert message, retried or not, by notifier",
            metrics.failures.clone(),
        );
        registry.register(
            "tocsin_notifications_abandoned",
            "Alert messages given up on after their last attempt failed, by notifier",
            metrics.abandoned.clone(),
        );
        registry.register(
            "tocsin_alerts_abandoned",
            "Alerts that ended without their resolve being delivered",
            metrics.alerts_abandoned.clone(),
        );
        metrics
    }

    /// Counts one failed attempt on `notifier`.
    pub fn attempt_failed(&self, notifier: &str) {
        let _known = self.known(notifier);
        self.failures.get_or_create(&label(notifier)).inc();
    }

    /// Counts one message `notifier` will not be sent.
    pub fn abandoned(&self, notifier: &str) {
        let _known = self.known(notifier);
        self.abandoned.get_or_create(&label(notifier)).inc();
    }

    /// Counts one alert that ended with no resolve delivered.
    pub fn alert_abandoned(&self) {
        self.alerts_abandoned.inc();
    }

    /// Alerts that ended with no resolve delivered so far.
    #[must_use]
    pub fn alerts_abandoned(&self) -> u64 {
        self.alerts_abandoned.get()
    }

    /// Failed attempts on `notifier` so far.
    #[must_use]
    pub fn attempt_failures(&self, notifier: &str) -> u64 {
        self.failures
            .get(&label(notifier))
            .map_or(0, |count| count.get())
    }

    /// Messages given up on for `notifier` so far.
    #[must_use]
    pub fn abandonments(&self, notifier: &str) -> u64 {
        self.abandoned
            .get(&label(notifier))
            .map_or(0, |count| count.get())
    }

    /// Drops the series of every notifier `configured` says no, so a
    /// notifier a reload removed stops being exported. Its counts are
    /// gone; one configured again starts from zero, which Prometheus reads
    /// as a counter reset.
    pub fn retain(&self, configured: impl Fn(&str) -> bool) {
        let mut notifiers = self.lock();
        notifiers.retain(|notifier| {
            let keep = configured(notifier);
            if !keep {
                self.failures.remove(&label(notifier));
                self.abandoned.remove(&label(notifier));
            }
            keep
        });
    }

    /// Notes that `notifier` has series, holding the lock while the caller
    /// creates or bumps one so [`AlertMetrics::retain`] cannot drop it from
    /// the set but leave the series.
    fn known(&self, notifier: &str) -> std::sync::MutexGuard<'_, BTreeSet<String>> {
        let mut notifiers = self.lock();
        if !notifiers.contains(notifier) {
            notifiers.insert(notifier.to_owned());
        }
        notifiers
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeSet<String>> {
        // Nothing done under the lock panics.
        self.notifiers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use prometheus_client::registry::Registry;

    use super::AlertMetrics;

    #[test]
    fn retain_drops_only_the_notifiers_no_longer_configured() {
        let metrics = AlertMetrics::register(&mut Registry::default());
        metrics.attempt_failed("pager");
        metrics.abandoned("pager");
        metrics.attempt_failed("slack");
        metrics.alert_abandoned();
        metrics.retain(|notifier| notifier == "slack");
        assert_eq!(metrics.alerts_abandoned(), 1, "not a notifier's series");
        assert_eq!(metrics.attempt_failures("pager"), 0);
        assert_eq!(metrics.abandonments("pager"), 0);
        assert!(metrics.failures.get(&super::label("pager")).is_none());
        assert!(metrics.abandoned.get(&super::label("pager")).is_none());
        assert_eq!(metrics.attempt_failures("slack"), 1);
    }
}
