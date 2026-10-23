// SPDX-License-Identifier: AGPL-3.0-only

//! Incidents and maintenance windows as the engine sees them: which
//! components they name, and for maintenance, when.

use tocsin_core::Health;

/// An open incident or a maintenance window that has not completed, as read
/// through [`crate::ports::Notices`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// An unresolved incident.
    Incident {
        /// The components it names.
        components: Vec<String>,
        /// Its declared impact as a health: none is `Up`, minor is
        /// `Degraded`, major and critical are `Down`.
        impact: Health,
    },
    /// A maintenance window that has not completed.
    Maintenance {
        /// The components it covers.
        components: Vec<String>,
        /// When it starts (unix milliseconds).
        starts_at_ms: i64,
        /// When it ends, or `None` to run until it is marked completed.
        ends_at_ms: Option<i64>,
    },
}

impl Notice {
    fn names(&self, component: &str) -> bool {
        match self {
            Notice::Incident { components, .. } | Notice::Maintenance { components, .. } => {
                components.iter().any(|id| id == component)
            }
        }
    }

    /// Whether this is a maintenance window active at `now_ms`: started, and
    /// not yet at its end.
    #[must_use]
    pub fn maintenance_active(&self, now_ms: i64) -> bool {
        match self {
            Notice::Maintenance {
                starts_at_ms,
                ends_at_ms,
                ..
            } => *starts_at_ms <= now_ms && ends_at_ms.is_none_or(|end| now_ms < end),
            Notice::Incident { .. } => false,
        }
    }
}

/// The worst declared impact of the incidents naming `component`, or `None`
/// when none does.
pub(crate) fn incident_impact(notices: &[Notice], component: &str) -> Option<Health> {
    notices
        .iter()
        .filter(|notice| notice.names(component))
        .filter_map(|notice| match notice {
            Notice::Incident { impact, .. } => Some(*impact),
            Notice::Maintenance { .. } => None,
        })
        .max()
}

/// Whether a maintenance window covering `component` is active at `now_ms`.
pub(crate) fn in_maintenance(notices: &[Notice], component: &str, now_ms: i64) -> bool {
    notices
        .iter()
        .any(|notice| notice.names(component) && notice.maintenance_active(now_ms))
}

/// The next time after `now_ms` a maintenance window starts or ends.
pub(crate) fn next_boundary(notices: &[Notice], now_ms: i64) -> Option<i64> {
    notices
        .iter()
        .flat_map(|notice| match notice {
            Notice::Maintenance {
                starts_at_ms,
                ends_at_ms,
                ..
            } => [Some(*starts_at_ms), *ends_at_ms],
            Notice::Incident { .. } => [None, None],
        })
        .flatten()
        .filter(|&at| at > now_ms)
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(starts_at_ms: i64, ends_at_ms: Option<i64>) -> Notice {
        Notice::Maintenance {
            components: vec!["api".into()],
            starts_at_ms,
            ends_at_ms,
        }
    }

    #[test]
    fn a_window_is_active_from_its_start_until_its_end() {
        let notices = [window(100, Some(200))];
        assert!(!in_maintenance(&notices, "api", 99));
        assert!(in_maintenance(&notices, "api", 100));
        assert!(in_maintenance(&notices, "api", 199));
        assert!(!in_maintenance(&notices, "api", 200));
        assert!(!in_maintenance(&notices, "web", 150));
        assert!(in_maintenance(&[window(100, None)], "api", i64::MAX));
    }

    #[test]
    fn boundaries_are_the_next_start_or_end() {
        let notices = [window(100, Some(200)), window(150, None)];
        assert_eq!(next_boundary(&notices, 0), Some(100));
        assert_eq!(next_boundary(&notices, 100), Some(150));
        assert_eq!(next_boundary(&notices, 160), Some(200));
        assert_eq!(next_boundary(&notices, 200), None);
    }

    #[test]
    fn the_worst_incident_naming_a_component_counts() {
        let notices = [
            Notice::Incident {
                components: vec!["api".into()],
                impact: Health::Degraded,
            },
            Notice::Incident {
                components: vec!["api".into(), "web".into()],
                impact: Health::Down,
            },
            window(0, None),
        ];
        assert_eq!(incident_impact(&notices, "api"), Some(Health::Down));
        assert_eq!(
            incident_impact(&notices[..1], "api"),
            Some(Health::Degraded)
        );
        assert_eq!(incident_impact(&notices, "db"), None);
    }
}
