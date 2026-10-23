// SPDX-License-Identifier: AGPL-3.0-only

//! Scalar value types shared across the configuration: identifiers,
//! durations, secrets and email addresses.

use std::borrow::Cow;
use std::fmt;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// An identifier: a slug matching `^[a-z0-9][a-z0-9-]*$`.
///
/// Identifiers name components, groups, checks and every other item, and
/// other items refer to them by it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct Id(String);

/// The pattern every [`Id`] matches, as published in the JSON Schema.
pub const ID_PATTERN: &str = "^[a-z0-9][a-z0-9-]*$";

impl Id {
    /// Parses an identifier, explaining what is wrong when it is not one.
    ///
    /// # Errors
    ///
    /// Returns a message when `text` is not a slug.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut characters = text.chars();
        let valid = characters
            .next()
            .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
            && characters.all(|character| {
                character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
            });
        if valid {
            Ok(Id(text.to_owned()))
        } else {
            Err(format!(
                "invalid id `{text}`: use lowercase letters, digits and hyphens, starting with a letter or digit"
            ))
        }
    }

    /// The identifier as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Id {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Id::parse(&text).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Id {
    fn schema_name() -> Cow<'static, str> {
        "Id".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": ID_PATTERN,
            "description": "A slug: lowercase letters, digits and hyphens, starting with a letter or digit."
        })
    }
}

/// A length of time, written as one or more number-and-unit pairs such as
/// `"500ms"`, `"30s"`, `"5m"`, `"1h30m"` or `"1d"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Duration(std::time::Duration);

/// The pattern every [`Duration`] matches, as published in the JSON Schema.
pub const DURATION_PATTERN: &str = "^([0-9]+(ms|s|m|h|d))+$";

impl Duration {
    /// A duration of whole seconds.
    #[must_use]
    pub const fn from_secs(seconds: u64) -> Self {
        Duration(std::time::Duration::from_secs(seconds))
    }

    /// Parses `"1h30m"` and the like.
    ///
    /// # Errors
    ///
    /// Returns a message when `text` is not a duration.
    pub fn parse(text: &str) -> Result<Self, String> {
        let invalid = || {
            format!(
                "invalid duration `{text}`: write a number followed by ms, s, m, h or d, such as \"30s\" or \"1h30m\""
            )
        };
        if text.is_empty() {
            return Err(invalid());
        }
        let mut total = std::time::Duration::ZERO;
        let mut rest = text;
        while !rest.is_empty() {
            let digits = rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len());
            if digits == 0 {
                return Err(invalid());
            }
            let amount: u64 = rest[..digits].parse().map_err(|_| invalid())?;
            rest = &rest[digits..];
            let unit = rest
                .find(|c: char| c.is_ascii_digit())
                .unwrap_or(rest.len());
            let part = match &rest[..unit] {
                "ms" => std::time::Duration::from_millis(amount),
                "s" => std::time::Duration::from_secs(amount),
                "m" => std::time::Duration::from_secs(amount.saturating_mul(60)),
                "h" => std::time::Duration::from_secs(amount.saturating_mul(3600)),
                "d" => std::time::Duration::from_secs(amount.saturating_mul(86_400)),
                _ => return Err(invalid()),
            };
            rest = &rest[unit..];
            total = total.checked_add(part).ok_or_else(invalid)?;
        }
        Ok(Duration(total))
    }

    /// The duration as a standard library value.
    #[must_use]
    pub const fn get(self) -> std::time::Duration {
        self.0
    }
}

impl fmt::Display for Duration {
    /// Writes the canonical form, largest unit first: `"1h30m"`, `"500ms"`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut milliseconds = self.0.as_millis();
        if milliseconds == 0 {
            return formatter.write_str("0s");
        }
        for (unit, size) in [
            ("d", 86_400_000),
            ("h", 3_600_000),
            ("m", 60_000),
            ("s", 1000),
            ("ms", 1),
        ] {
            if milliseconds >= size {
                write!(formatter, "{}{unit}", milliseconds / size)?;
                milliseconds %= size;
            }
        }
        Ok(())
    }
}

impl Serialize for Duration {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Duration {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Duration::parse(&text).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Duration {
    fn schema_name() -> Cow<'static, str> {
        "Duration".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": DURATION_PATTERN,
            "description": "A duration such as \"500ms\", \"30s\", \"5m\", \"1h30m\" or \"1d\"."
        })
    }
}

/// A secret read from an environment variable.
///
/// In a file it is always written `${ENV:NAME}`; a secret never sits in a
/// file. Loading resolves it, and a missing variable is a validation
/// error. Its value never appears in `Debug`, `Display`, serialized output
/// or error messages.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret {
    variable: String,
    value: Option<String>,
}

/// The pattern every [`Secret`] matches, as published in the JSON Schema.
pub const SECRET_PATTERN: &str = r"^\$\{ENV:[A-Za-z_][A-Za-z0-9_]*\}$";

impl Secret {
    /// Parses an `${ENV:NAME}` reference. The value is resolved later.
    ///
    /// # Errors
    ///
    /// Returns a message, which never repeats `text`, when `text` is not an
    /// environment reference.
    pub fn parse(text: &str) -> Result<Self, String> {
        let variable = text
            .strip_prefix("${ENV:")
            .and_then(|rest| rest.strip_suffix('}'))
            .filter(|name| is_variable_name(name));
        match variable {
            Some(variable) => Ok(Secret {
                variable: variable.to_owned(),
                value: None,
            }),
            // The text may be a secret pasted into the file, so it is not echoed.
            None => Err(
                "a secret must be an environment reference such as \"${ENV:SLACK_WEBHOOK_URL}\"; secrets never sit in files"
                    .to_owned(),
            ),
        }
    }

    /// The environment variable the secret is read from.
    #[must_use]
    pub fn variable(&self) -> &str {
        &self.variable
    }

    /// The secret's value.
    ///
    /// # Panics
    ///
    /// Panics if the secret was not resolved, which cannot happen to a
    /// secret inside a [`Config`](super::Config) returned by
    /// [`load`](super::load).
    #[must_use]
    pub fn expose(&self) -> &str {
        self.value
            .as_deref()
            .expect("config::load resolves every secret before returning")
    }

    pub(crate) fn resolve(&mut self, value: String) {
        self.value = Some(value);
    }

    /// The value, once resolved.
    pub(crate) fn resolved(&self) -> Option<&str> {
        self.value.as_deref()
    }

    /// What serialized output and `Display` show in place of the value.
    fn redacted(&self) -> String {
        format!("<redacted from ${{ENV:{}}}>", self.variable)
    }
}

fn is_variable_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.redacted())
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.redacted())
    }
}

impl Serialize for Secret {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.redacted())
    }
}

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Secret::parse(&text).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Secret {
    fn schema_name() -> Cow<'static, str> {
        "Secret".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": SECRET_PATTERN,
            "description": "A secret, read from an environment variable: \"${ENV:NAME}\"."
        })
    }
}

/// A string written either as itself or, when it is sensitive, as an
/// `${ENV:NAME}` reference to a [`Secret`].
///
/// A string containing `${ENV:` must be exactly one reference: the
/// variable holds the whole value, so `"Bearer ${ENV:TOKEN}"` is rejected
/// and `TOKEN` holds `Bearer ...` instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Text {
    /// Written in the file; shown as is.
    Literal(String),
    /// Read from the environment; redacted wherever it is shown.
    Secret(Secret),
}

impl Text {
    /// The value: the literal, or the secret's.
    ///
    /// # Panics
    ///
    /// Panics if a secret was not resolved, which cannot happen inside a
    /// [`Config`](super::Config) returned by [`load`](super::load).
    #[must_use]
    pub fn expose(&self) -> &str {
        match self {
            Text::Literal(text) => text,
            Text::Secret(secret) => secret.expose(),
        }
    }
}

impl Serialize for Text {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Text::Literal(text) => serializer.serialize_str(text),
            Text::Secret(secret) => secret.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Text {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        if text.contains("${ENV:") {
            Secret::parse(&text)
                .map(Text::Secret)
                .map_err(serde::de::Error::custom)
        } else {
            Ok(Text::Literal(text))
        }
    }
}

impl JsonSchema for Text {
    fn schema_name() -> Cow<'static, str> {
        "Text".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "description": "A string, or a secret read from an environment variable: \"${ENV:NAME}\"."
        })
    }
}

/// Splits `"Name <address>"` or a bare address into the address, or says
/// why it is not one.
///
/// # Errors
///
/// Returns a message when `text` is not an email address.
pub fn email_address(text: &str) -> Result<&str, String> {
    let address = match (text.rfind('<'), text.strip_suffix('>')) {
        (Some(open), Some(inner)) => &inner[open + 1..],
        _ => text,
    }
    .trim();
    let valid = address.split_once('@').is_some_and(|(local, domain)| {
        !local.is_empty()
            && !domain.is_empty()
            && !domain.contains('@')
            && domain.contains('.')
            && !address.contains(char::is_whitespace)
    });
    if valid {
        Ok(address)
    } else {
        Err(format!("`{text}` is not an email address"))
    }
}

/// Whether an address is one nobody reads: a local part containing
/// `noreply` or `donotreply` once case, `+tags` and the `-`, `_`, `.`
/// separators are ignored, so `no-reply-status@` and `alerts-noreply@`
/// count too.
#[must_use]
pub fn is_unanswerable(address: &str) -> bool {
    let local = address.split('@').next().unwrap_or_default();
    let local = local.split('+').next().unwrap_or_default();
    let normalized: String = local
        .chars()
        .filter(|character| !matches!(character, '-' | '_' | '.'))
        .flat_map(char::to_lowercase)
        .collect();
    normalized.contains("noreply") || normalized.contains("donotreply")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_slugs() {
        assert!(Id::parse("web-1").is_ok());
        assert!(Id::parse("0").is_ok());
        for bad in ["", "-web", "Web", "web_1", "web 1", "wéb"] {
            assert!(Id::parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn durations_parse_and_print_canonically() {
        let cases = [
            ("500ms", 500),
            ("30s", 30_000),
            ("5m", 300_000),
            ("1h30m", 5_400_000),
            ("1d", 86_400_000),
            ("90s", 90_000),
        ];
        for (text, milliseconds) in cases {
            let duration = Duration::parse(text).unwrap();
            assert_eq!(duration.get().as_millis(), milliseconds, "{text}");
        }
        assert_eq!(Duration::parse("90s").unwrap().to_string(), "1m30s");
        assert_eq!(Duration::parse("1500ms").unwrap().to_string(), "1s500ms");
        for bad in ["", "60", "s", "1x", "1.5s", "-1s", "1 s", "1s2"] {
            assert!(Duration::parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn secrets_never_echo_their_text() {
        let error = Secret::parse("hunter2-literal").unwrap_err();
        assert!(!error.contains("hunter2"));
        let mut secret = Secret::parse("${ENV:TOKEN}").unwrap();
        assert_eq!(secret.variable(), "TOKEN");
        secret.resolve("hunter2".into());
        assert_eq!(secret.expose(), "hunter2");
        for shown in [
            format!("{secret:?}"),
            secret.to_string(),
            serde_json::to_string(&secret).unwrap(),
        ] {
            assert!(!shown.contains("hunter2"), "{shown}");
            assert!(shown.contains("TOKEN"), "{shown}");
        }
        for bad in [
            "${ENV:}",
            "${ENV:1A}",
            "${env:A}",
            "x${ENV:A}",
            "${ENV:A-B}",
        ] {
            assert!(Secret::parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn email_addresses() {
        assert_eq!(email_address("ops@example.com"), Ok("ops@example.com"));
        assert_eq!(
            email_address("Ops Team <ops@example.com>"),
            Ok("ops@example.com")
        );
        for bad in [
            "ops",
            "@example.com",
            "ops@",
            "ops@localhost",
            "a b@example.com",
        ] {
            assert!(email_address(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn unanswerable_addresses() {
        for bad in [
            "noreply@example.com",
            "no-reply@example.com",
            "No_Reply@example.com",
            "donotreply@example.com",
            "do-not-reply@example.com",
            "noreply+alerts@example.com",
            "no-reply-status@example.com",
            "alerts-noreply@example.com",
            "Alerts.Do.Not.Reply@example.com",
        ] {
            assert!(is_unanswerable(bad), "{bad}");
        }
        for good in [
            "support@example.com",
            "reply@example.com",
            "norepl@example.com",
            "reply-now@example.com",
            "support@noreply.example.com",
        ] {
            assert!(!is_unanswerable(good), "{good}");
        }
    }
}
