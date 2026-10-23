// SPDX-License-Identifier: AGPL-3.0-only

//! The three health states a check result, outage or rollup can carry.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// How a check fared. Ordered from best to worst, so `max` is the worst.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Health {
    /// Every assertion passed.
    Up,
    /// An assertion whose failure maps to `degraded` failed; none mapping to `down` did.
    Degraded,
    /// The check could not run, or an assertion whose failure maps to `down` failed.
    Down,
}

impl Health {
    /// Every state, best first.
    pub const ALL: [Health; 3] = [Health::Up, Health::Degraded, Health::Down];

    /// The stable lowercase name used in storage, JSON and configuration.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Health::Up => "up",
            Health::Degraded => "degraded",
            Health::Down => "down",
        }
    }
}

impl fmt::Display for Health {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The text was not one of `up`, `degraded` or `down`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownHealth(pub String);

impl fmt::Display for UnknownHealth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown health {:?}; expected up, degraded or down",
            self.0
        )
    }
}

impl std::error::Error for UnknownHealth {}

impl FromStr for Health {
    type Err = UnknownHealth;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Health::ALL
            .into_iter()
            .find(|health| health.as_str() == text)
            .ok_or_else(|| UnknownHealth(text.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::Health;

    #[test]
    fn round_trips_through_text() {
        for health in Health::ALL {
            assert_eq!(health.as_str().parse::<Health>(), Ok(health));
        }
        assert!("sideways".parse::<Health>().is_err());
    }

    #[test]
    fn worst_is_the_maximum() {
        assert_eq!(Health::ALL.into_iter().max(), Some(Health::Down));
        assert!(Health::Up < Health::Degraded);
    }
}
