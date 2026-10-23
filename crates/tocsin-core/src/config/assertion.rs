// SPDX-License-Identifier: AGPL-3.0-only

//! `[[check.assert]]`: structured conditions on a probe result.
//!
//! Assertions are TOML tables, never a string DSL, so the JSON Schema can
//! validate every one. Each table holds exactly one condition plus an
//! optional `on_fail`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::check::CheckType;
use crate::Health;

/// One condition a probe result must meet, and the state a failure maps to.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Assertion {
    /// The HTTP status is one of these.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Vec<u16>>,
    /// The probe's latency, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<Comparison>,
    /// A value in the JSON response body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json: Option<JsonAssertion>,
    /// The response body text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<TextAssertion>,
    /// A response header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<HeaderAssertion>,
    /// Days until the server certificate expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub certificate_days_remaining: Option<Comparison>,
    /// Whether the server certificate is valid for the host name checked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname_valid: Option<bool>,
    /// Whether the server certificate chains to a trusted root and is
    /// within its validity period.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain_valid: Option<bool>,
    /// The records a DNS lookup returns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub records: Option<RecordsAssertion>,
    /// The text a TCP server sends after connecting (and after `send`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub banner: Option<TextAssertion>,
    /// The state the check takes when this condition fails.
    #[serde(default)]
    pub on_fail: FailureState,
}

/// The condition an [`Assertion`] holds, borrowed from it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Condition<'a> {
    /// See [`Assertion::status`].
    Status(&'a [u16]),
    /// See [`Assertion::latency_ms`].
    LatencyMilliseconds(&'a Comparison),
    /// See [`Assertion::json`].
    Json(&'a JsonAssertion),
    /// See [`Assertion::body`].
    Body(&'a TextAssertion),
    /// See [`Assertion::header`].
    Header(&'a HeaderAssertion),
    /// See [`Assertion::certificate_days_remaining`].
    CertificateDaysRemaining(&'a Comparison),
    /// See [`Assertion::hostname_valid`].
    HostnameValid(bool),
    /// See [`Assertion::chain_valid`].
    ChainValid(bool),
    /// See [`Assertion::records`].
    Records(&'a RecordsAssertion),
    /// See [`Assertion::banner`].
    Banner(&'a TextAssertion),
}

impl Condition<'_> {
    /// The key the condition is written under.
    #[must_use]
    pub fn key(&self) -> &'static str {
        match self {
            Condition::Status(_) => "status",
            Condition::LatencyMilliseconds(_) => "latency_ms",
            Condition::Json(_) => "json",
            Condition::Body(_) => "body",
            Condition::Header(_) => "header",
            Condition::CertificateDaysRemaining(_) => "certificate_days_remaining",
            Condition::HostnameValid(_) => "hostname_valid",
            Condition::ChainValid(_) => "chain_valid",
            Condition::Records(_) => "records",
            Condition::Banner(_) => "banner",
        }
    }

    /// Whether the condition can be evaluated on a result of this check type.
    #[must_use]
    pub fn applies_to(&self, check_type: CheckType) -> bool {
        match self {
            Condition::LatencyMilliseconds(_) => true,
            Condition::Status(_)
            | Condition::Json(_)
            | Condition::Body(_)
            | Condition::Header(_) => check_type == CheckType::Http,
            Condition::CertificateDaysRemaining(_)
            | Condition::HostnameValid(_)
            | Condition::ChainValid(_) => {
                matches!(check_type, CheckType::Http | CheckType::Tls)
            }
            Condition::Records(_) => check_type == CheckType::Dns,
            Condition::Banner(_) => check_type == CheckType::Tcp,
        }
    }

    /// Whether the condition reads the server certificate.
    #[must_use]
    pub fn needs_certificate(&self) -> bool {
        matches!(
            self,
            Condition::CertificateDaysRemaining(_)
                | Condition::HostnameValid(_)
                | Condition::ChainValid(_)
        )
    }
}

impl Assertion {
    /// Every condition set in this table; a valid assertion has exactly one.
    #[must_use]
    pub fn conditions(&self) -> Vec<Condition<'_>> {
        let mut conditions = Vec::new();
        if let Some(status) = &self.status {
            conditions.push(Condition::Status(status));
        }
        if let Some(latency) = &self.latency_ms {
            conditions.push(Condition::LatencyMilliseconds(latency));
        }
        if let Some(json) = &self.json {
            conditions.push(Condition::Json(json));
        }
        if let Some(body) = &self.body {
            conditions.push(Condition::Body(body));
        }
        if let Some(header) = &self.header {
            conditions.push(Condition::Header(header));
        }
        if let Some(days) = &self.certificate_days_remaining {
            conditions.push(Condition::CertificateDaysRemaining(days));
        }
        if let Some(valid) = self.hostname_valid {
            conditions.push(Condition::HostnameValid(valid));
        }
        if let Some(valid) = self.chain_valid {
            conditions.push(Condition::ChainValid(valid));
        }
        if let Some(records) = &self.records {
            conditions.push(Condition::Records(records));
        }
        if let Some(banner) = &self.banner {
            conditions.push(Condition::Banner(banner));
        }
        conditions
    }

    /// The assertion's single condition.
    ///
    /// # Panics
    ///
    /// Panics unless the table holds exactly one condition, which every
    /// assertion in a loaded configuration does.
    #[must_use]
    pub fn condition(&self) -> Condition<'_> {
        match self.conditions().as_slice() {
            [condition] => *condition,
            _ => panic!("config::load ensures each assertion holds exactly one condition"),
        }
    }
}

/// The keys an assertion table can hold a condition under.
pub const CONDITION_KEYS: &[&str] = &[
    "status",
    "latency_ms",
    "json",
    "body",
    "header",
    "certificate_days_remaining",
    "hostname_valid",
    "chain_valid",
    "records",
    "banner",
];

/// The state a failed assertion puts its check in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum FailureState {
    /// Working, but worse than it should be.
    Degraded,
    /// Not working.
    #[default]
    Down,
}

impl From<FailureState> for Health {
    fn from(state: FailureState) -> Self {
        match state {
            FailureState::Degraded => Health::Degraded,
            FailureState::Down => Health::Down,
        }
    }
}

/// Numeric bounds; every bound given must hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Comparison {
    /// Less than.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lt: Option<u64>,
    /// Less than or equal to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lte: Option<u64>,
    /// Greater than.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gt: Option<u64>,
    /// Greater than or equal to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gte: Option<u64>,
    /// Equal to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eq: Option<u64>,
}

impl Comparison {
    /// Whether any bound is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Comparison::default()
    }

    /// Whether `value` meets every bound. A negative value, such as the
    /// days left on an expired certificate, is below every bound.
    #[must_use]
    pub fn holds(&self, value: i64) -> bool {
        let value = i128::from(value);
        let bound = |bound: Option<u64>, test: fn(i128, i128) -> bool| {
            bound.is_none_or(|bound| test(value, i128::from(bound)))
        };
        bound(self.lt, |value, bound| value < bound)
            && bound(self.lte, |value, bound| value <= bound)
            && bound(self.gt, |value, bound| value > bound)
            && bound(self.gte, |value, bound| value >= bound)
            && bound(self.eq, |value, bound| value == bound)
    }
}

/// A value selected from the JSON response body by an RFC 9535 JSON path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JsonAssertion {
    /// An RFC 9535 JSON path, such as `$.checks[0].status` or
    /// `$.components[?@.name=='API'].status`. When it selects several
    /// values, the first in document order is compared.
    pub path: String,
    /// The value equals this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eq: Option<serde_json::Value>,
    /// The value does not equal this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ne: Option<serde_json::Value>,
    /// Whether the path selects any value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exists: Option<bool>,
    /// The value is a string containing this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,
}

impl JsonAssertion {
    /// How many matchers (`eq`, `ne`, `exists`, `contains`) are set; a valid
    /// assertion has exactly one.
    #[must_use]
    pub fn matcher_count(&self) -> usize {
        usize::from(self.eq.is_some())
            + usize::from(self.ne.is_some())
            + usize::from(self.exists.is_some())
            + usize::from(self.contains.is_some())
    }
}

/// A condition on text.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TextAssertion {
    /// The text contains this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,
    /// The text does not contain this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_contains: Option<String>,
    /// The text matches this regular expression (Rust `regex` syntax),
    /// anywhere in the text unless anchored with `^` or `$`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regex: Option<String>,
}

impl TextAssertion {
    /// How many matchers are set; a valid assertion has exactly one.
    #[must_use]
    pub fn matcher_count(&self) -> usize {
        usize::from(self.contains.is_some())
            + usize::from(self.not_contains.is_some())
            + usize::from(self.regex.is_some())
    }
}

/// A condition on one response header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HeaderAssertion {
    /// The header name, matched without regard to case.
    pub name: String,
    /// A value of the header equals this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eq: Option<String>,
    /// A value of the header contains this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,
    /// Whether the header is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exists: Option<bool>,
}

impl HeaderAssertion {
    /// How many matchers are set; a valid assertion has exactly one.
    #[must_use]
    pub fn matcher_count(&self) -> usize {
        usize::from(self.eq.is_some())
            + usize::from(self.contains.is_some())
            + usize::from(self.exists.is_some())
    }
}

/// A condition on the records a DNS lookup returns. Records are compared
/// as text, ignoring a trailing `.` on names.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecordsAssertion {
    /// One of the records equals this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,
    /// The records are exactly these, in any order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eq: Option<Vec<String>>,
}

impl RecordsAssertion {
    /// How many matchers are set; a valid assertion has exactly one.
    #[must_use]
    pub fn matcher_count(&self) -> usize {
        usize::from(self.contains.is_some()) + usize::from(self.eq.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn condition_keys_match_the_struct() {
        let every = Assertion {
            status: Some(vec![200]),
            latency_ms: Some(Comparison::default()),
            json: Some(JsonAssertion {
                path: "$".into(),
                eq: None,
                ne: None,
                exists: None,
                contains: None,
            }),
            body: Some(TextAssertion::default()),
            header: Some(HeaderAssertion {
                name: "a".into(),
                eq: None,
                contains: None,
                exists: None,
            }),
            certificate_days_remaining: Some(Comparison::default()),
            hostname_valid: Some(true),
            chain_valid: Some(true),
            records: Some(RecordsAssertion::default()),
            banner: Some(TextAssertion::default()),
            on_fail: FailureState::Down,
        };
        let keys: Vec<_> = every.conditions().iter().map(Condition::key).collect();
        assert_eq!(keys, CONDITION_KEYS);
    }

    #[test]
    fn comparisons() {
        let window = Comparison {
            gt: Some(10),
            lte: Some(20),
            ..Comparison::default()
        };
        assert!(!window.holds(10));
        assert!(window.holds(11));
        assert!(window.holds(20));
        assert!(!window.holds(21));
        let positive = Comparison {
            gte: Some(0),
            ..Comparison::default()
        };
        assert!(!positive.holds(-1), "a negative value is below every bound");
        assert!(Comparison::default().holds(-5));
    }
}
