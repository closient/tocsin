// SPDX-License-Identifier: AGPL-3.0-only

//! `[alerting]`: the labels and annotations every check and heartbeat must
//! carry, so an alert a receiver routes on them is never routed by default.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The labels tocsin sets on every alert it posts to Alertmanager: its
/// identity. A check or heartbeat may not set them.
pub const IDENTITY_LABELS: &[&str] = &["alertname", "node", "subject_kind", "subject_id"];

/// The annotations tocsin sets on alerts it posts to Alertmanager (and, for
/// `ended_by`, to a webhook). A check or heartbeat may not set them.
pub const OWN_ANNOTATIONS: &[&str] = &[
    "summary",
    "description",
    "health",
    "name",
    "component",
    "component_name",
    "tocsin_test",
    "ended_by",
];

/// `[alerting]`: the contract every check's and heartbeat's `labels` and
/// `annotations` are held to. Without it, none is required and any value
/// goes; the name and non-empty rules apply either way.
///
/// A receiver such as Alertmanager routes on labels, and a label that is
/// missing or misspelt routes by default without anyone noticing. Declared
/// here, the same mistake fails `tocsin config check`, and a reload that
/// makes it keeps the last good configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Alerting {
    /// The labels the contract covers, by name, each a
    /// `[alerting.labels.<name>]` table. Labels it does not name are
    /// allowed and unchecked.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[schemars(extend("propertyNames" = { "pattern": LABEL_NAME_PATTERN }))]
    pub labels: BTreeMap<String, AlertingRule>,
    /// The annotations the contract covers, by name, each a
    /// `[alerting.annotations.<name>]` table. Annotations it does not name
    /// are allowed and unchecked.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[schemars(extend("propertyNames" = { "pattern": LABEL_NAME_PATTERN }))]
    pub annotations: BTreeMap<String, AlertingRule>,
}

/// What `[alerting]` asks of one label or annotation.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AlertingRule {
    /// Whether every check and heartbeat must set it.
    #[serde(default)]
    pub required: bool,
    /// The values it may take, matched exactly. Empty allows any value.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(inner(length(min = 1)))]
    pub values: Vec<String>,
}

/// [`is_label_name`] as a JSON Schema `pattern`, for the schema's
/// `propertyNames`: without lookahead, which not every validator has.
pub const LABEL_NAME_PATTERN: &str = "^([A-Za-z][A-Za-z0-9_]*|_([A-Za-z0-9][A-Za-z0-9_]*)?)$";

/// Whether `name` can be a label: letters, digits and underscores, not
/// starting with a digit, as Prometheus and Alertmanager require. Names
/// starting `__` are reserved for their own use.
#[must_use]
pub fn is_label_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
        && !name.starts_with("__")
}

#[cfg(test)]
mod tests {
    use super::{LABEL_NAME_PATTERN, is_label_name};

    #[test]
    fn label_names_are_prometheus_names() {
        for name in ["needs_human", "_private", "a1", "PageClass"] {
            assert!(is_label_name(name), "{name}");
        }
        for name in ["", "1st", "needs-human", "needs human", "__name__", "é"] {
            assert!(!is_label_name(name), "{name}");
        }
    }

    #[test]
    fn the_schema_pattern_takes_the_names_the_loader_does() {
        let pattern = regex::Regex::new(LABEL_NAME_PATTERN).unwrap();
        for name in [
            "needs_human",
            "_private",
            "a1",
            "PageClass",
            "_",
            "_1",
            "a__b",
            "a_",
            "",
            "1st",
            "needs-human",
            "needs human",
            "__name__",
            "__",
            "é",
            "a\n",
        ] {
            assert_eq!(pattern.is_match(name), is_label_name(name), "{name:?}");
        }
    }
}
