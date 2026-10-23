// SPDX-License-Identifier: AGPL-3.0-only

//! The rules the engine applies to results, with no I/O and no clock:
//! when a check first runs, when its state changes, how a component's
//! checks combine, and what the status page shows for a component.

use std::time::Duration;

use serde::Serialize;

use crate::Health;
use crate::config::ComponentRule;

/// The jitter is at most the interval divided by this: a tenth of it.
pub const JITTER_DIVISOR: u32 = 10;

/// How far after the engine starts a check first runs: a value between zero
/// and a tenth of `interval`, at millisecond granularity, derived from the
/// check's ID alone.
///
/// The same ID always gets the same offset, on every node and in every
/// release, so restarts do not reshuffle the schedule and checks sharing an
/// interval spread across its first tenth instead of running together.
#[must_use]
pub fn jitter(id: &str, interval: Duration) -> Duration {
    let ceiling = interval.as_millis() / u128::from(JITTER_DIVISOR);
    let ceiling = u64::try_from(ceiling).unwrap_or(u64::MAX);
    if ceiling == 0 {
        return Duration::ZERO;
    }
    Duration::from_millis(stable_hash(id.as_bytes()) % (ceiling + 1))
}

/// FNV-1a over the bytes, then the `MurmurHash3` 64-bit finalizer so short IDs
/// differing only in their last byte still land far apart. Written out here
/// rather than using `std`'s hasher, whose output may change between Rust
/// releases.
fn stable_hash(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0100_0000_01b3;
    let mut hash = bytes.iter().fold(OFFSET, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(PRIME)
    });
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xff51_afd7_ed55_8ccd);
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    hash ^ (hash >> 33)
}

/// A check's confirmed state: what the status page, outages and alerts see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Confirmed {
    /// The confirmed health.
    pub health: Health,
    /// When the first result of the run that confirmed it started, in
    /// milliseconds since the Unix epoch.
    pub since_ms: i64,
}

/// A confirmed state change, returned once per change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Transition {
    /// The state before, or `None` for the first state the check reached.
    pub from: Option<Health>,
    /// The state after.
    pub to: Health,
    /// When the first result of the confirming run started.
    pub since_ms: i64,
    /// When the result that completed the run started.
    pub confirmed_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Run {
    count: u32,
    since_ms: i64,
}

/// Turns a check's raw results into confirmed state changes.
///
/// The state changes only after `confirm` consecutive results that disagree
/// with it; any result matching the current state starts the count over. The
/// new state is the health of the result that completes the run. A check
/// starts with no state, and its first `confirm` results establish one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirmation {
    confirm: u32,
    state: Option<Confirmed>,
    run: Option<Run>,
}

impl Confirmation {
    /// A check with no state yet that needs `confirm` agreeing results to
    /// change state. Zero is treated as one.
    #[must_use]
    pub fn new(confirm: u32) -> Self {
        Confirmation {
            confirm: confirm.max(1),
            state: None,
            run: None,
        }
    }

    /// A check already in `health` since `since_ms`, as recorded before a
    /// restart.
    #[must_use]
    pub fn resume(confirm: u32, health: Health, since_ms: i64) -> Self {
        Confirmation {
            state: Some(Confirmed { health, since_ms }),
            ..Confirmation::new(confirm)
        }
    }

    /// The confirmed state, if the check has one yet.
    #[must_use]
    pub fn state(&self) -> Option<Confirmed> {
        self.state
    }

    /// Records one result started at `at_ms` and returns the change it
    /// confirms, if any.
    pub fn observe(&mut self, health: Health, at_ms: i64) -> Option<Transition> {
        let from = self.state.map(|state| state.health);
        if from == Some(health) {
            self.run = None;
            return None;
        }
        let run = self.run.get_or_insert(Run {
            count: 0,
            since_ms: at_ms,
        });
        run.count += 1;
        if run.count < self.confirm {
            return None;
        }
        let since_ms = run.since_ms;
        self.run = None;
        self.state = Some(Confirmed { health, since_ms });
        Some(Transition {
            from,
            to: health,
            since_ms,
            confirmed_at_ms: at_ms,
        })
    }
}

/// A component's health from the confirmed health of its checks, `None` for
/// a check with no state yet.
///
/// A component with no checks is up: nothing reports it broken. A component
/// whose checks all lack a state has no health yet. Otherwise checks without
/// a state are left out and `rule` combines the rest:
///
/// - `worst`: the worst check.
/// - `majority`: the worst state that more than half of the checks are in
///   or beyond. A tie falls to the better state, so one of two checks down
///   leaves the component up.
#[must_use]
pub fn component_health(rule: ComponentRule, checks: &[Option<Health>]) -> Option<Health> {
    if checks.is_empty() {
        return Some(Health::Up);
    }
    let known: Vec<Health> = checks.iter().flatten().copied().collect();
    if known.is_empty() {
        return None;
    }
    match rule {
        ComponentRule::Worst => known.iter().copied().max(),
        ComponentRule::Majority => Health::ALL.into_iter().rev().find(|&level| {
            let at_or_beyond = known.iter().filter(|&&health| health >= level).count();
            at_or_beyond * 2 > known.len()
        }),
    }
}

/// What the status page shows for a component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentStatus {
    /// Working.
    Operational,
    /// Working with problems.
    Degraded,
    /// Not working.
    Down,
    /// In a maintenance window.
    Maintenance,
    /// No check has reported yet and no incident says otherwise.
    Unknown,
}

impl ComponentStatus {
    /// The status that shows `health`.
    #[must_use]
    pub const fn from_health(health: Health) -> Self {
        match health {
            Health::Up => ComponentStatus::Operational,
            Health::Degraded => ComponentStatus::Degraded,
            Health::Down => ComponentStatus::Down,
        }
    }

    /// The stable lowercase name used in JSON.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ComponentStatus::Operational => "operational",
            ComponentStatus::Degraded => "degraded",
            ComponentStatus::Down => "down",
            ComponentStatus::Maintenance => "maintenance",
            ComponentStatus::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for ComponentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The status shown for a component.
///
/// - `health` is the component's health from its checks.
/// - `incident` is the worst impact, as a health, of the open incidents
///   naming the component, or `None` when there are none.
/// - `maintenance` is whether a maintenance window covering it is active.
///
/// An incident overrides health when it is worse. During maintenance the
/// checks' health is not shown and the component is in maintenance, unless
/// an incident declares an impact.
#[must_use]
pub fn component_status(
    health: Option<Health>,
    incident: Option<Health>,
    maintenance: bool,
) -> ComponentStatus {
    let declared = incident.filter(|&impact| impact > Health::Up);
    if maintenance {
        return declared.map_or(ComponentStatus::Maintenance, ComponentStatus::from_health);
    }
    match (health, declared) {
        (None, None) => ComponentStatus::Unknown,
        (Some(health), None) => ComponentStatus::from_health(health),
        (None, Some(impact)) => ComponentStatus::from_health(impact),
        (Some(health), Some(impact)) => ComponentStatus::from_health(health.max(impact)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Health::{Degraded, Down, Up};

    #[test]
    fn jitter_stays_within_a_tenth_of_the_interval() {
        for interval in [
            Duration::from_millis(1),
            Duration::from_millis(9),
            Duration::from_millis(10),
            Duration::from_secs(1),
            Duration::from_secs(60),
            Duration::from_hours(24),
        ] {
            for index in 0..2_000 {
                let id = format!("check-{index}");
                let offset = jitter(&id, interval);
                assert!(
                    offset * JITTER_DIVISOR <= interval,
                    "{id} got {offset:?} for {interval:?}"
                );
            }
        }
        assert_eq!(jitter("api", Duration::from_millis(9)), Duration::ZERO);
    }

    #[test]
    fn jitter_spreads_ids_across_the_window() {
        let interval = Duration::from_secs(60);
        let window = interval / JITTER_DIVISOR;
        let mut buckets = [0_u32; 10];
        for index in 0..200 {
            let offset = jitter(&format!("check-{index}"), interval);
            let bucket = offset.as_millis() * 10 / (window.as_millis() + 1);
            buckets[usize::try_from(bucket).expect("a small bucket")] += 1;
        }
        assert!(
            buckets.iter().all(|&count| (5..=40).contains(&count)),
            "uneven spread {buckets:?}"
        );
    }

    #[test]
    fn jitter_is_stable_across_releases() {
        // Pinned: changing the hash reshuffles every deployed schedule.
        let interval = Duration::from_secs(60);
        assert_eq!(jitter("api", interval), Duration::from_millis(1495));
        assert_eq!(jitter("db", interval), Duration::from_millis(586));
        assert_eq!(jitter("web", interval), Duration::from_millis(1078));
    }

    #[test]
    fn first_results_establish_a_state_after_confirm() {
        let mut confirmation = Confirmation::new(3);
        assert_eq!(confirmation.observe(Up, 10), None);
        assert_eq!(confirmation.observe(Up, 20), None);
        assert_eq!(confirmation.state(), None);
        assert_eq!(
            confirmation.observe(Up, 30),
            Some(Transition {
                from: None,
                to: Up,
                since_ms: 10,
                confirmed_at_ms: 30,
            })
        );
        assert_eq!(
            confirmation.state(),
            Some(Confirmed {
                health: Up,
                since_ms: 10
            })
        );
    }

    #[test]
    fn confirm_of_one_changes_on_every_differing_result() {
        let mut confirmation = Confirmation::new(1);
        assert_eq!(confirmation.observe(Up, 1).map(|t| t.to), Some(Up));
        assert_eq!(confirmation.observe(Up, 2), None);
        assert_eq!(confirmation.observe(Down, 3).map(|t| t.to), Some(Down));
        assert_eq!(
            confirmation.observe(Degraded, 4).map(|t| t.to),
            Some(Degraded)
        );
    }

    #[test]
    fn a_change_needs_confirm_consecutive_differing_results() {
        let mut confirmation = Confirmation::resume(3, Up, 0);
        assert_eq!(confirmation.observe(Down, 100), None);
        assert_eq!(confirmation.observe(Down, 200), None);
        assert_eq!(
            confirmation.observe(Down, 300),
            Some(Transition {
                from: Some(Up),
                to: Down,
                since_ms: 100,
                confirmed_at_ms: 300,
            })
        );
        assert_eq!(confirmation.observe(Down, 400), None);
    }

    #[test]
    fn flapping_never_confirms() {
        let mut confirmation = Confirmation::resume(2, Up, 0);
        for step in 0..50 {
            let health = if step % 2 == 0 { Down } else { Up };
            assert_eq!(confirmation.observe(health, step), None, "step {step}");
        }
        assert_eq!(confirmation.state().map(|state| state.health), Some(Up));
    }

    #[test]
    fn a_matching_result_starts_the_count_over() {
        let mut confirmation = Confirmation::resume(3, Up, 0);
        assert_eq!(confirmation.observe(Down, 1), None);
        assert_eq!(confirmation.observe(Down, 2), None);
        assert_eq!(confirmation.observe(Up, 3), None);
        assert_eq!(confirmation.observe(Down, 4), None);
        assert_eq!(confirmation.observe(Down, 5), None);
        let change = confirmation.observe(Down, 6).expect("a change");
        assert_eq!(change.since_ms, 4);
    }

    #[test]
    fn mixed_failures_confirm_as_the_last_one() {
        let mut confirmation = Confirmation::resume(3, Up, 0);
        assert_eq!(confirmation.observe(Down, 1), None);
        assert_eq!(confirmation.observe(Degraded, 2), None);
        assert_eq!(confirmation.observe(Down, 3).map(|t| t.to), Some(Down));
    }

    #[test]
    fn worst_rule_takes_the_worst_check() {
        let rule = ComponentRule::Worst;
        assert_eq!(component_health(rule, &[Some(Up), Some(Up)]), Some(Up));
        assert_eq!(
            component_health(rule, &[Some(Up), Some(Degraded), Some(Up)]),
            Some(Degraded)
        );
        assert_eq!(
            component_health(rule, &[Some(Up), Some(Down), Some(Degraded)]),
            Some(Down)
        );
        assert_eq!(component_health(rule, &[None, Some(Down)]), Some(Down));
    }

    #[test]
    fn majority_rule_takes_what_more_than_half_share() {
        let rule = ComponentRule::Majority;
        assert_eq!(
            component_health(rule, &[Some(Up), Some(Up), Some(Down)]),
            Some(Up)
        );
        assert_eq!(
            component_health(rule, &[Some(Up), Some(Down), Some(Down)]),
            Some(Down)
        );
        // Two of three are degraded or worse, but only one is down.
        assert_eq!(
            component_health(rule, &[Some(Up), Some(Degraded), Some(Down)]),
            Some(Degraded)
        );
        // A tie falls to the better state.
        assert_eq!(component_health(rule, &[Some(Up), Some(Down)]), Some(Up));
        // Checks without a state do not count.
        assert_eq!(
            component_health(rule, &[None, None, Some(Down)]),
            Some(Down)
        );
    }

    #[test]
    fn components_without_known_checks() {
        for rule in [ComponentRule::Worst, ComponentRule::Majority] {
            assert_eq!(component_health(rule, &[]), Some(Up));
            assert_eq!(component_health(rule, &[None, None]), None);
        }
    }

    #[test]
    fn an_incident_overrides_health_only_when_worse() {
        use ComponentStatus as S;
        assert_eq!(component_status(Some(Up), None, false), S::Operational);
        assert_eq!(component_status(Some(Up), Some(Down), false), S::Down);
        assert_eq!(component_status(Some(Down), Some(Degraded), false), S::Down);
        assert_eq!(
            component_status(Some(Degraded), Some(Up), false),
            S::Degraded
        );
        assert_eq!(component_status(None, None, false), S::Unknown);
        assert_eq!(component_status(None, Some(Up), false), S::Unknown);
        assert_eq!(component_status(None, Some(Degraded), false), S::Degraded);
    }

    #[test]
    fn maintenance_hides_health_but_not_a_declared_incident() {
        use ComponentStatus as S;
        assert_eq!(component_status(Some(Down), None, true), S::Maintenance);
        assert_eq!(component_status(None, None, true), S::Maintenance);
        assert_eq!(component_status(Some(Down), Some(Up), true), S::Maintenance);
        assert_eq!(
            component_status(Some(Up), Some(Degraded), true),
            S::Degraded
        );
    }
}
