// SPDX-License-Identifier: AGPL-3.0-only

//! Which notifiers an alert goes to, from the `[[route]]`s.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use tocsin_core::Health;
use tocsin_core::config::{self, NotifierType};
use tocsin_store::{AlertRouting, AlertTarget, SubjectKind};

use crate::notifiers::alertmanager::REPOST_INTERVAL;

/// A check or heartbeat, as alerts name it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Subject {
    kind: SubjectKind,
    name: String,
}

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
    resolve: bool,
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

/// Routes alerts, and names their subjects and components.
#[derive(Debug, Clone, Default)]
pub struct Router {
    subjects: HashMap<String, Subject>,
    components: HashMap<String, (String, Option<String>)>,
    notifier_types: HashMap<String, NotifierType>,
    routes: Vec<CompiledRoute>,
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

impl Router {
    /// The router for a loaded configuration.
    #[must_use]
    pub fn new(config: &config::Config) -> Self {
        let mut subjects = HashMap::new();
        // Heartbeats first, so a check sharing an id wins.
        for heartbeat in &config.heartbeats {
            let id = heartbeat.id.as_str().to_owned();
            let name = heartbeat.name.clone().unwrap_or_else(|| id.clone());
            subjects.insert(
                id,
                Subject {
                    kind: SubjectKind::Heartbeat,
                    name,
                },
            );
        }
        for check in &config.checks {
            let id = check.id.as_str().to_owned();
            let name = check.name.clone().unwrap_or_else(|| id.clone());
            subjects.insert(
                id,
                Subject {
                    kind: SubjectKind::Check,
                    name,
                },
            );
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
                resolve: route.resolve,
            })
            .collect();
        Router {
            subjects,
            components,
            notifier_types,
            routes,
        }
    }

    /// Whether `subject_id` is a check or a heartbeat. An id the
    /// configuration no longer has is taken for a check.
    #[must_use]
    pub fn subject_kind(&self, subject_id: &str) -> SubjectKind {
        self.subjects
            .get(subject_id)
            .map_or(SubjectKind::Check, |subject| subject.kind)
    }

    /// The check or heartbeat's display name, or its id.
    #[must_use]
    pub fn subject_name(&self, subject_id: &str) -> String {
        self.subjects
            .get(subject_id)
            .map_or_else(|| subject_id.to_owned(), |subject| subject.name.clone())
    }

    /// The component's display name.
    #[must_use]
    pub fn component_name(&self, component_id: &str) -> Option<String> {
        self.components
            .get(component_id)
            .map(|(name, _)| name.clone())
    }

    /// Where an alert for `subject_id` at `health` goes: one target per
    /// notifier, however many routes name it. A notifier resolves if any
    /// matching route resolves, and repeats on the shortest interval among
    /// them; an Alertmanager notifier re-posts at least every
    /// [`REPOST_INTERVAL`] so Alertmanager never times the alert out.
    #[must_use]
    pub fn routing(
        &self,
        subject_id: &str,
        component_id: Option<&str>,
        health: Health,
    ) -> AlertRouting {
        let kind = self.subject_kind(subject_id);
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
                    target.resolve |= route.resolve;
                    target.repeat_ms = match (target.repeat_ms, route.repeat_ms) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (a, b) => a.or(b),
                    };
                } else {
                    targets.push(AlertTarget {
                        notifier_id: notifier.clone(),
                        route_id: route.id.clone(),
                        resolve: route.resolve,
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
        AlertRouting {
            subject_kind: kind,
            targets,
        }
    }
}
