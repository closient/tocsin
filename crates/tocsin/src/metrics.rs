// SPDX-License-Identifier: AGPL-3.0-only

//! The process's Prometheus registry. Each part of tocsin registers its
//! metrics here; the `/metrics` endpoint (TOC-50) will serve [`Metrics::encode`].

use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use tocsin_alert::AlertMetrics;
use tocsin_core::incident::Timestamp;

/// Every metric the process exports.
#[derive(Debug)]
pub struct Metrics {
    registry: Registry,
    /// Alerting's counters.
    pub alerting: AlertMetrics,
    /// Whether the configuration last reloaded.
    pub config: ConfigMetrics,
}

impl Metrics {
    /// A registry holding every metric, all at zero.
    pub fn new() -> Self {
        let mut registry = Registry::default();
        let alerting = AlertMetrics::register(&mut registry);
        let config = ConfigMetrics::register(&mut registry);
        Metrics {
            registry,
            alerting,
            config,
        }
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

/// How configuration reloads went:
///
/// - `tocsin_config_reload_success`, 1 while the configuration last read
///   is the one running, 0 after a reload failed and the last good one was
///   kept. It starts at 1: serve does not start on a bad configuration.
/// - `tocsin_config_last_error_timestamp`, when a reload last failed, in
///   unix seconds; 0 if none has.
#[derive(Debug, Clone)]
pub struct ConfigMetrics {
    reload_success: Gauge,
    last_error: Gauge,
}

impl ConfigMetrics {
    fn register(registry: &mut Registry) -> Self {
        let metrics = ConfigMetrics {
            reload_success: Gauge::default(),
            last_error: Gauge::default(),
        };
        metrics.reload_success.set(1);
        registry.register(
            "tocsin_config_reload_success",
            "1 if the last configuration reload succeeded, 0 if it failed and the last good configuration is still running",
            metrics.reload_success.clone(),
        );
        registry.register(
            "tocsin_config_last_error_timestamp",
            "When a configuration reload last failed, in unix seconds; 0 if none has",
            metrics.last_error.clone(),
        );
        metrics
    }

    /// A reload put its configuration in place.
    pub fn succeeded(&self) {
        self.reload_success.set(1);
    }

    /// A reload at `at` failed.
    pub fn failed(&self, at: Timestamp) {
        self.reload_success.set(0);
        self.last_error.set(at.as_millis().div_euclid(1000));
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
