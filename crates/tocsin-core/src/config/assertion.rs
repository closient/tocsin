// SPDX-License-Identifier: AGPL-3.0-only

//! `[[check.assert]]`: structured conditions on a probe result.
//!
//! Assertions are TOML tables, never a string DSL, so the JSON Schema can
//! validate every one. Each table holds exactly one condition plus an
//! optional `on_fail`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::check::CheckType;

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
    /// The records a DNS lookup returns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub records: Option<RecordsAssertion>,
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
    /// See [`Assertion::records`].
    Records(&'a RecordsAssertion),
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
            Condition::Records(_) => "records",
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
            Condition::CertificateDaysRemaining(_) => {
                matches!(check_type, CheckType::Http | CheckType::Tls)
            }
            Condition::Records(_) => check_type == CheckType::Dns,
        }
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
        if let Some(records) = &self.records {
            conditions.push(Condition::Records(records));
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
    "records",
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
}

/// A value at a path in a JSON response body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JsonAssertion {
    /// Where the value is: `$`, then `.key`, `[index]` or `["key"]` steps,
    /// such as `$.checks[0].status`.
    pub path: String,
    /// The value equals this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eq: Option<serde_json::Value>,
    /// The value does not equal this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ne: Option<serde_json::Value>,
    /// Whether a value exists at the path.
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
}

impl TextAssertion {
    /// How many matchers are set; a valid assertion has exactly one.
    #[must_use]
    pub fn matcher_count(&self) -> usize {
        usize::from(self.contains.is_some()) + usize::from(self.not_contains.is_some())
    }
}

/// A condition on one response header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HeaderAssertion {
    /// The header name, matched without regard to case.
    pub name: String,
    /// The header value equals this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eq: Option<String>,
    /// The header value contains this.
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

/// A condition on the records a DNS lookup returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecordsAssertion {
    /// One of the records equals this.
    pub contains: String,
}

/// One step of a [`JsonAssertion::path`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonPathStep {
    /// An object member.
    Key(String),
    /// An array element.
    Index(usize),
}

/// Parses a JSON path: `$`, then `.key`, `[index]`, `["key"]` or `['key']`.
///
/// # Errors
///
/// Returns a message saying where the path stops making sense.
pub fn parse_json_path(path: &str) -> Result<Vec<JsonPathStep>, String> {
    let invalid = |detail: &str| format!("invalid JSON path `{path}`: {detail}");
    let mut rest = path
        .strip_prefix('$')
        .ok_or_else(|| invalid("it must start with `$`"))?;
    let mut steps = Vec::new();
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('.') {
            let end = after
                .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'))
                .unwrap_or(after.len());
            if end == 0 {
                return Err(invalid("a `.` must be followed by a key"));
            }
            steps.push(JsonPathStep::Key(after[..end].to_owned()));
            rest = &after[end..];
        } else if let Some(after) = rest.strip_prefix('[') {
            let close = after
                .find(']')
                .ok_or_else(|| invalid("a `[` is never closed"))?;
            let inside = &after[..close];
            let quoted = inside
                .strip_prefix('"')
                .and_then(|key| key.strip_suffix('"'))
                .or_else(|| {
                    inside
                        .strip_prefix('\'')
                        .and_then(|key| key.strip_suffix('\''))
                });
            if let Some(key) = quoted {
                steps.push(JsonPathStep::Key(key.to_owned()));
            } else {
                let index = inside
                    .parse()
                    .map_err(|_| invalid("brackets hold an index or a quoted key"))?;
                steps.push(JsonPathStep::Index(index));
            }
            rest = &after[close + 1..];
        } else {
            return Err(invalid("expected `.key` or `[...]`"));
        }
    }
    Ok(steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_paths() {
        assert_eq!(parse_json_path("$").unwrap(), vec![]);
        assert_eq!(
            parse_json_path("$.checks[0][\"a b\"]['c'].status").unwrap(),
            vec![
                JsonPathStep::Key("checks".into()),
                JsonPathStep::Index(0),
                JsonPathStep::Key("a b".into()),
                JsonPathStep::Key("c".into()),
                JsonPathStep::Key("status".into()),
            ]
        );
        for bad in ["status", "$.", "$[", "$[x]", "$..a", "$a"] {
            assert!(parse_json_path(bad).is_err(), "{bad:?} should be rejected");
        }
    }

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
            records: Some(RecordsAssertion {
                contains: "a".into(),
            }),
            on_fail: FailureState::Down,
        };
        let keys: Vec<_> = every.conditions().iter().map(Condition::key).collect();
        assert_eq!(keys, CONDITION_KEYS);
    }
}
