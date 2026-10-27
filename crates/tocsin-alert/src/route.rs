// SPDX-License-Identifier: AGPL-3.0-only

//! Which notifiers an alert goes to, from the `[[route]]`s.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use tocsin_core::Health;
use tocsin_core::config::{self, NotifierType};
use tocsin_store::{AlertRouting, AlertTarget, SubjectKind};

use crate::notifiers::alertmanager::REPOST_INTERVAL;
use crate::notifiers::pushover::Priorities;

#[derive(Debug, Clone)]
struct CompiledRoute {
    id: String,
    notifiers: Vec<String>,
    components: HashSet<String>,
    groups: HashSet<String>,
    checks: HashSet<String>,
    heartbeats: HashSet<String>,
    notify_on: Health,
    repeat_ms: Option<i64>,
}

impl CompiledRoute {
    fn matches(
        &self,
        kind: SubjectKind,
        subject_id: &str,
        group: Option<&str>,
        component: Option<&str>,
        health: Health,
    ) -> bool {
        if health < self.notify_on {
            return false;
        }
        let subjects = if self.checks.is_empty() && self.heartbeats.is_empty() {
            true
        } else {
            match kind {
                SubjectKind::Check => self.checks.contains(subject_id),
                SubjectKind::Heartbeat => self.heartbeats.contains(subject_id),
            }
        };
        let components = if self.components.is_empty() && self.groups.is_empty() {
            true
        } else {
            component.is_some_and(|component| self.components.contains(component))
                || group.is_some_and(|group| self.groups.contains(group))
        };
        subjects && components
    }
}

/// What the configuration says about a check or heartbeat that its alerts
/// carry.
#[derive(Debug, Clone, Default)]
struct Subject {
    name: String,
    labels: BTreeMap<String, String>,
    annotations: BTreeMap<String, String>,
}

/// Routes alerts, and names their subjects and components.
#[derive(Debug, Clone, Default)]
pub struct Router {
    /// Each check and heartbeat by kind and id, as an alert's identity
    /// names its subject.
    subjects: HashMap<(SubjectKind, String), Subject>,
    components: HashMap<String, (String, Option<String>)>,
    notifier_types: HashMap<String, NotifierType>,
    /// Each Pushover notifier's priorities, which say what keeps ringing.
    pushover: HashMap<String, Priorities>,
    /// Each notifier's `max_in_flight`.
    max_in_flight: HashMap<String, usize>,
    routes: Vec<CompiledRoute>,
}

/// A subject's configured labels with its `class` and, when it has one, its
/// `topology_node` among them, which every alert of it carries.
fn pinned_labels(
    labels: &BTreeMap<String, String>,
    class: config::ProbeClass,
    topology_node: Option<&config::TopologyNode>,
) -> BTreeMap<String, String> {
    let mut labels = labels.clone();
    labels.insert(config::CLASS_LABEL.to_owned(), class.as_str().to_owned());
    if let Some(node) = topology_node {
        labels.insert(
            config::TOPOLOGY_NODE_LABEL.to_owned(),
            node.as_str().to_owned(),
        );
    }
    labels
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

impl Router {
    /// The router for a loaded configuration.
    #[must_use]
    pub fn new(config: &config::Config) -> Self {
        let mut subjects = HashMap::new();
        for heartbeat in &config.heartbeats {
            let id = heartbeat.id.as_str().to_owned();
            let subject = Subject {
                name: heartbeat.name.clone().unwrap_or_else(|| id.clone()),
                labels: pinned_labels(
                    &heartbeat.labels,
                    heartbeat.class,
                    heartbeat.topology_node.as_ref(),
                ),
                annotations: heartbeat.annotations.clone(),
            };
            subjects.insert((SubjectKind::Heartbeat, id), subject);
        }
        for check in &config.checks {
            let id = check.id.as_str().to_owned();
            let subject = Subject {
                name: check.name.clone().unwrap_or_else(|| id.clone()),
                labels: pinned_labels(&check.labels, check.class, check.topology_node.as_ref()),
                annotations: check.annotations.clone(),
            };
            subjects.insert((SubjectKind::Check, id), subject);
        }
        let components = config
            .components
            .iter()
            .map(|component| {
                (
                    component.id.as_str().to_owned(),
                    (
                        component.name.clone(),
                        component
                            .group
                            .as_ref()
                            .map(|group| group.as_str().to_owned()),
                    ),
                )
            })
            .collect();
        let notifier_types = config
            .notifiers
            .iter()
            .map(|notifier| (notifier.id.as_str().to_owned(), notifier.notifier_type))
            .collect();
        let pushover = config
            .notifiers
            .iter()
            .filter_map(|notifier| {
                let settings = notifier.pushover.as_ref()?;
                (notifier.notifier_type == NotifierType::Pushover)
                    .then(|| (notifier.id.as_str().to_owned(), Priorities::from(settings)))
            })
            .collect();
        let max_in_flight = config
            .notifiers
            .iter()
            .map(|notifier| {
                (
                    notifier.id.as_str().to_owned(),
                    usize::try_from(notifier.max_in_flight)
                        .unwrap_or(usize::MAX)
                        .max(1),
                )
            })
            .collect();
        let set = |ids: &[config::Id]| ids.iter().map(|id| id.as_str().to_owned()).collect();
        let routes = config
            .routes
            .iter()
            .map(|route| CompiledRoute {
                id: route.id.as_str().to_owned(),
                notifiers: route
                    .notifiers
                    .iter()
                    .map(|id| id.as_str().to_owned())
                    .collect(),
                components: set(&route.components),
                groups: set(&route.groups),
                checks: set(&route.checks),
                heartbeats: set(&route.heartbeats),
                notify_on: Health::from(route.notify_on),
                repeat_ms: Some(millis(route.repeat.get())).filter(|ms| *ms > 0),
            })
            .collect();
        Router {
            subjects,
            components,
            notifier_types,
            pushover,
            max_in_flight,
            routes,
        }
    }

    /// The check or heartbeat's display name, or its id.
    #[must_use]
    pub fn subject_name(&self, kind: SubjectKind, subject_id: &str) -> String {
        self.subjects
            .get(&(kind, subject_id.to_owned()))
            .map_or_else(|| subject_id.to_owned(), |subject| subject.name.clone())
    }

    /// The most deliveries `notifier_id` may send at once; one for a
    /// notifier the configuration no longer has.
    #[must_use]
    pub fn max_in_flight(&self, notifier_id: &str) -> usize {
        self.max_in_flight.get(notifier_id).copied().unwrap_or(1)
    }

    /// The component's display name.
    #[must_use]
    pub fn component_name(&self, component_id: &str) -> Option<String> {
        self.components
            .get(component_id)
            .map(|(name, _)| name.clone())
    }

    /// Where an alert for the `kind` `subject_id` at `health` goes: one
    /// target per notifier, however many routes name it, repeating on the
    /// shortest interval among them; an Alertmanager notifier re-posts at
    /// least every [`REPOST_INTERVAL`] so Alertmanager never times the
    /// alert out. It also lists every notifier whose message at `health`
    /// keeps ringing until acknowledged, so an alert that rang is cancelled
    /// once it no longer does.
    #[must_use]
    pub fn routing(
        &self,
        kind: SubjectKind,
        subject_id: &str,
        component_id: Option<&str>,
        health: Health,
    ) -> AlertRouting {
        let group = component_id
            .and_then(|component| self.components.get(component))
            .and_then(|(_, group)| group.as_deref());
        let mut targets: Vec<AlertTarget> = Vec::new();
        for route in &self.routes {
            if !route.matches(kind, subject_id, group, component_id, health) {
                continue;
            }
            for notifier in &route.notifiers {
                if let Some(target) = targets
                    .iter_mut()
                    .find(|target| &target.notifier_id == notifier)
                {
                    target.repeat_ms = match (target.repeat_ms, route.repeat_ms) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (a, b) => a.or(b),
                    };
                } else {
                    targets.push(AlertTarget {
                        notifier_id: notifier.clone(),
                        route_id: route.id.clone(),
                        repeat_ms: route.repeat_ms,
                    });
                }
            }
        }
        for target in &mut targets {
            if self.notifier_types.get(&target.notifier_id) == Some(&NotifierType::Alertmanager) {
                let repost = millis(REPOST_INTERVAL);
                target.repeat_ms = Some(target.repeat_ms.map_or(repost, |ms| ms.min(repost)));
            }
        }
        let mut ringing: Vec<String> = self
            .pushover
            .iter()
            .filter(|(_, priorities)| priorities.rings(health))
            .map(|(id, _)| id.clone())
            .collect();
        ringing.sort();
        let subject = self.subjects.get(&(kind, subject_id.to_owned()));
        AlertRouting {
            targets,
            ringing,
            labels: subject
                .map(|subject| subject.labels.clone())
                .unwrap_or_default(),
            annotations: subject
                .map(|subject| subject.annotations.clone())
                .unwrap_or_default(),
        }
    }
}
