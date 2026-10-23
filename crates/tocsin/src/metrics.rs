// SPDX-License-Identifier: AGPL-3.0-only

//! The process's Prometheus registry. Each part of tocsin registers its
//! metrics here; the `/metrics` endpoint (TOC-50) will serve [`Metrics::encode`].

use prometheus_client::registry::Registry;
use tocsin_alert::AlertMetrics;

/// Every metric the process exports.
#[derive(Debug)]
pub struct Metrics {
    registry: Registry,
    /// Alerting's counters.
    pub alerting: AlertMetrics,
}

impl Metrics {
    /// A registry holding every metric, all at zero.
    pub fn new() -> Self {
        let mut registry = Registry::default();
        let alerting = AlertMetrics::register(&mut registry);
        Metrics { registry, alerting }
    }

    /// The metrics in the `OpenMetrics` text format.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "served at /metrics once TOC-50 lands")
    )]
    pub fn encode(&self) -> String {
        let mut text = String::new();
        prometheus_client::encoding::text::encode(&mut text, &self.registry)
            .expect("writing to a String cannot fail");
        text
    }
}

#[cfg(test)]
mod tests {
    use super::Metrics;

    #[test]
    fn failed_attempts_and_abandoned_notifications_are_exported_per_notifier() {
        let metrics = Metrics::new();
        metrics.alerting.attempt_failed("pager");
        metrics.alerting.attempt_failed("pager");
        metrics.alerting.abandoned("pager");
        let text = metrics.encode();
        for line in [
            "tocsin_notification_attempt_failures_total{notifier=\"pager\"} 2",
            "tocsin_notifications_abandoned_total{notifier=\"pager\"} 1",
        ] {
            assert!(text.contains(line), "{line} missing from {text}");
        }
    }
}
