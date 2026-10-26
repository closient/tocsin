// SPDX-License-Identifier: AGPL-3.0-only

//! What a notifier is handed: one message about one alert, and the wording
//! every chat-style notifier shares.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use tocsin_core::Health;
use tocsin_store::SubjectKind;

/// Which message this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AlertEvent {
    /// The alert started firing, or its health changed.
    Fire,
    /// The alert is still firing.
    Repeat,
    /// The alert ended.
    Resolve,
}

impl AlertEvent {
    /// The spelling used in payloads.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            AlertEvent::Fire => "fire",
            AlertEvent::Repeat => "repeat",
            AlertEvent::Resolve => "resolve",
        }
    }
}

/// One message about one alert.
///
/// Its identity, (`node_id`, `subject_kind`, `subject_id`), is the same on
/// the fire, every repeat and the resolve, so a receiver can pair them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    /// Fire, repeat or resolve.
    pub event: AlertEvent,
    /// The node that saw it.
    pub node_id: String,
    /// Check or heartbeat.
    pub subject_kind: SubjectKind,
    /// The check or heartbeat id.
    pub subject_id: String,
    /// The check or heartbeat's display name.
    pub subject_name: String,
    /// The check or heartbeat's configured `labels`, as the configuration
    /// has them when the message is sent.
    pub labels: BTreeMap<String, String>,
    /// The check or heartbeat's configured `annotations`, as the
    /// configuration has them when the message is sent.
    pub annotations: BTreeMap<String, String>,
    /// Its component, if it has one.
    pub component_id: Option<String>,
    /// The component's display name.
    pub component_name: Option<String>,
    /// Degraded or down; on a resolve, the health it recovered from.
    pub health: Health,
    /// When the alert started (unix milliseconds).
    pub started_at_ms: i64,
    /// When it ended, on a resolve (unix milliseconds).
    pub ended_at_ms: Option<i64>,
    /// When this message was handed to the notifier (unix milliseconds).
    pub sent_at_ms: i64,
    /// The outbox row this message came from; `None` for a test message.
    pub delivery_id: Option<i64>,
    /// Whether `tocsin notify test` sent it.
    pub test: bool,
}

impl Alert {
    /// The identity as one string: `node/kind/id`.
    #[must_use]
    pub fn identity(&self) -> String {
        format!(
            "{}/{}/{}",
            self.node_id,
            self.subject_kind.as_str(),
            self.subject_id
        )
    }

    /// How long the alert lasted, on a resolve (milliseconds).
    #[must_use]
    pub fn duration_ms(&self) -> Option<i64> {
        self.ended_at_ms
            .map(|ended| ended.saturating_sub(self.started_at_ms).max(0))
    }

    /// Whether this message says the alert is over.
    #[must_use]
    pub fn is_resolve(&self) -> bool {
        self.event == AlertEvent::Resolve
    }

    /// A one-line title: `[DOWN] API (check api on node-a)`.
    #[must_use]
    pub fn title(&self) -> String {
        let state = match (self.event, self.health) {
            (AlertEvent::Resolve, _) => "RESOLVED",
            (AlertEvent::Repeat, Health::Down) => "STILL DOWN",
            (AlertEvent::Repeat, _) => "STILL DEGRADED",
            (_, Health::Down) => "DOWN",
            (_, _) => "DEGRADED",
        };
        let test = if self.test { "[TEST] " } else { "" };
        format!(
            "{test}[{state}] {} ({} {} on {})",
            one_line(&self.subject_name),
            self.subject_kind.as_str(),
            self.subject_id,
            self.node_id
        )
    }

    /// A few sentences of detail, without the title.
    #[must_use]
    pub fn body(&self) -> String {
        let mut body = String::new();
        let name = one_line(&self.subject_name);
        let since = timestamp(self.started_at_ms);
        match (self.event, self.ended_at_ms) {
            (AlertEvent::Resolve, Some(ended)) => {
                let lasted = human_duration(self.duration_ms().unwrap_or_default());
                let _ = write!(
                    body,
                    "{name} recovered at {} after {lasted} ({} since {since}).",
                    timestamp(ended),
                    self.health
                );
            }
            _ => {
                let _ = write!(body, "{name} is {} since {since}.", self.health);
            }
        }
        if let Some(component) = &self.component_id {
            let label = self.component_name.as_deref().unwrap_or(component);
            let _ = write!(body, "\nComponent: {}", one_line(label));
        }
        let _ = write!(body, "\nAlert: {}", self.identity());
        if self.test {
            body.push_str("\nThis is a test sent by `tocsin notify test`.");
        }
        body
    }

    /// The title and body together.
    #[must_use]
    pub fn text(&self) -> String {
        format!("{}\n{}", self.title(), self.body())
    }
}

/// Collapses line breaks and other control characters so a configured name
/// cannot add lines or headers to a message.
#[must_use]
pub fn one_line(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

/// `2026-10-23 14:05:00 UTC`.
#[must_use]
pub fn timestamp(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms).map_or_else(
        |_| format!("{ms} ms"),
        |at| at.strftime("%Y-%m-%d %H:%M:%S UTC").to_string(),
    )
}

/// RFC 3339 with milliseconds, as Alertmanager and the webhook take it.
#[must_use]
pub fn rfc3339(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms).map_or_else(
        |_| jiff::Timestamp::UNIX_EPOCH.to_string(),
        |at| at.strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
    )
}

/// `1h 2m 3s`, to the second; under a second is `0s`.
#[must_use]
pub fn human_duration(ms: i64) -> String {
    let total = ms.max(0) / 1000;
    let (days, hours, minutes, seconds) = (
        total / 86_400,
        total / 3600 % 24,
        total / 60 % 60,
        total % 60,
    );
    let mut parts = Vec::new();
    for (value, unit) in [(days, "d"), (hours, "h"), (minutes, "m")] {
        if value > 0 {
            parts.push(format!("{value}{unit}"));
        }
    }
    if seconds > 0 || parts.is_empty() {
        parts.push(format!("{seconds}s"));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alert(event: AlertEvent) -> Alert {
        Alert {
            event,
            node_id: "node-a".into(),
            subject_kind: SubjectKind::Check,
            subject_id: "api".into(),
            subject_name: "API\r\nBcc: x@example.com".into(),
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
            component_id: Some("web".into()),
            component_name: Some("Website".into()),
            health: Health::Down,
            started_at_ms: 1_800_000_000_000,
            ended_at_ms: (event == AlertEvent::Resolve).then_some(1_800_000_090_000),
            sent_at_ms: 1_800_000_090_000,
            delivery_id: Some(7),
            test: false,
        }
    }

    #[test]
    fn titles_say_what_happened_on_one_line() {
        assert_eq!(
            alert(AlertEvent::Fire).title(),
            "[DOWN] API  Bcc: x@example.com (check api on node-a)"
        );
        assert!(
            alert(AlertEvent::Repeat)
                .title()
                .starts_with("[STILL DOWN] ")
        );
        assert!(
            alert(AlertEvent::Resolve)
                .title()
                .starts_with("[RESOLVED] ")
        );
    }

    #[test]
    fn a_resolve_says_how_long_it_lasted() {
        let body = alert(AlertEvent::Resolve).body();
        assert!(body.contains("after 1m 30s"), "{body}");
        assert!(body.contains("Alert: node-a/check/api"), "{body}");
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(human_duration(0), "0s");
        assert_eq!(human_duration(59_999), "59s");
        assert_eq!(human_duration(3_600_000), "1h");
        assert_eq!(human_duration(90_061_000), "1d 1h 1m 1s");
        assert_eq!(rfc3339(1_800_000_000_123), "2027-01-15T08:00:00.123Z");
    }
}
