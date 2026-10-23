// SPDX-License-Identifier: AGPL-3.0-only

//! Incidents and maintenance windows as the engine sees them: which
//! components they name, and for maintenance, when.

use tocsin_core::Health;
use tocsin_core::incident::{Impact, Incident, IncidentKind};

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
        /// Its scheduled start (unix milliseconds).
        starts_at_ms: i64,
        /// Its scheduled end (unix milliseconds).
        ends_at_ms: i64,
    },
}

impl Notice {
    /// What `incident` means to the engine, or `None` once it is resolved
    /// or completed.
    ///
    /// A maintenance window is its schedule: active from `scheduled_start`
    /// until `scheduled_end`, whatever its status says, except that marking
    /// it completed ends it at once. Maintenance without a schedule (the
    /// API refuses one) is ignored.
    #[must_use]
    pub fn from_incident(incident: &Incident) -> Option<Notice> {
        if incident.status.is_terminal() {
            return None;
        }
        let components = incident.components.clone();
        match incident.kind {
            IncidentKind::Incident => Some(Notice::Incident {
                components,
                impact: impact_health(incident.impact),
            }),
            IncidentKind::Maintenance => Some(Notice::Maintenance {
                components,
                starts_at_ms: incident.scheduled_start?.as_millis(),
                ends_at_ms: incident.scheduled_end?.as_millis(),
            }),
        }
    }

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
            } => (*starts_at_ms..*ends_at_ms).contains(&now_ms),
            Notice::Incident { .. } => false,
        }
    }
}

/// A declared impact as a health. Four levels become three: none is `Up`,
/// minor is `Degraded`, and major and critical are both `Down`.
#[must_use]
pub fn impact_health(impact: Impact) -> Health {
    match impact {
        Impact::None => Health::Up,
        Impact::Minor => Health::Degraded,
        Impact::Major | Impact::Critical => Health::Down,
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
            } => vec![*starts_at_ms, *ends_at_ms],
            Notice::Incident { .. } => Vec::new(),
        })
        .filter(|&at| at > now_ms)
        .min()
}

#[cfg(test)]
mod tests {
    use tocsin_core::incident::{Actor, IncidentStatus, Timestamp};

    use super::*;

    fn window(starts_at_ms: i64, ends_at_ms: i64) -> Notice {
        Notice::Maintenance {
            components: vec!["api".into()],
            starts_at_ms,
            ends_at_ms,
        }
    }

    #[test]
    fn a_window_is_active_from_its_start_until_its_end() {
        let notices = [window(100, 200)];
        assert!(!in_maintenance(&notices, "api", 99));
        assert!(in_maintenance(&notices, "api", 100));
        assert!(in_maintenance(&notices, "api", 199));
        assert!(!in_maintenance(&notices, "api", 200));
        assert!(!in_maintenance(&notices, "web", 150));
    }

    #[test]
    fn boundaries_are_the_next_start_or_end() {
        let notices = [window(100, 200), window(150, 300)];
        assert_eq!(next_boundary(&notices, 0), Some(100));
        assert_eq!(next_boundary(&notices, 100), Some(150));
        assert_eq!(next_boundary(&notices, 160), Some(200));
        assert_eq!(next_boundary(&notices, 200), Some(300));
        assert_eq!(next_boundary(&notices, 300), None);
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
            window(0, i64::MAX),
        ];
        assert_eq!(incident_impact(&notices, "api"), Some(Health::Down));
        assert_eq!(
            incident_impact(&notices[..1], "api"),
            Some(Health::Degraded)
        );
        assert_eq!(incident_impact(&notices, "db"), None);
    }

    fn incident(kind: IncidentKind, impact: Impact, status: IncidentStatus) -> Incident {
        let at = Timestamp::from_millis;
        let maintenance = kind == IncidentKind::Maintenance;
        Incident {
            id: 1,
            kind,
            title: "Title".into(),
            impact,
            status,
            components: vec!["api".into()],
            scheduled_start: maintenance.then(|| at(100)),
            scheduled_end: maintenance.then(|| at(200)),
            resolved_at: status.is_terminal().then(|| at(150)),
            created_at: at(0),
            created_by: Actor::token("ci"),
            updated_at: at(0),
            updated_by: Actor::token("ci"),
            updates: Vec::new(),
        }
    }

    #[test]
    fn impact_maps_onto_health() {
        let mapped: Vec<_> = Impact::ALL.iter().copied().map(impact_health).collect();
        assert_eq!(
            mapped,
            [Health::Up, Health::Degraded, Health::Down, Health::Down]
        );
    }

    #[test]
    fn open_incidents_and_maintenance_become_notices() {
        let open = incident(
            IncidentKind::Incident,
            Impact::Minor,
            IncidentStatus::Monitoring,
        );
        assert_eq!(
            Notice::from_incident(&open),
            Some(Notice::Incident {
                components: vec!["api".into()],
                impact: Health::Degraded,
            })
        );
        // Started early by its status or not, a window is its schedule.
        for status in [IncidentStatus::Scheduled, IncidentStatus::InProgress] {
            let planned = incident(IncidentKind::Maintenance, Impact::None, status);
            assert_eq!(Notice::from_incident(&planned), Some(window(100, 200)));
        }
    }

    #[test]
    fn resolved_and_completed_ones_do_not() {
        for (kind, status) in [
            (IncidentKind::Incident, IncidentStatus::Resolved),
            (IncidentKind::Maintenance, IncidentStatus::Completed),
        ] {
            let over = incident(kind, Impact::Critical, status);
            assert_eq!(Notice::from_incident(&over), None);
        }
    }
}
