// SPDX-License-Identifier: AGPL-3.0-only

//! An incident as markdown: YAML front matter, the title, then every
//! update oldest first. The format is described in `docs/incidents.md`.

use std::fmt::Write as _;

use serde_json::json;
use tocsin_core::incident::{Incident, Timestamp};

/// Renders `incident` as a markdown document.
///
/// Front-matter strings are written as JSON strings, which YAML reads as
/// the same strings whatever they contain.
pub fn markdown(incident: &Incident) -> String {
    let mut text = String::from("---\n");
    let mut field = |name: &str, value: serde_json::Value| {
        let _ = writeln!(text, "{name}: {value}");
    };
    field("id", json!(incident.id));
    field("kind", json!(incident.kind));
    field("title", json!(incident.title));
    field("impact", json!(incident.impact));
    field("status", json!(incident.status));
    field("components", json!(incident.components));
    let mut instant = |name: &str, value: Option<Timestamp>| {
        if let Some(value) = value {
            field(name, json!(value));
        }
    };
    instant("scheduled_start", incident.scheduled_start);
    instant("scheduled_end", incident.scheduled_end);
    instant("resolved_at", incident.resolved_at);
    field("created_at", json!(incident.created_at));
    field("created_by", json!(incident.created_by.to_string()));
    field("updated_at", json!(incident.updated_at));
    field("updated_by", json!(incident.updated_by.to_string()));
    text.push_str("---\n\n");
    let _ = writeln!(text, "# {}", incident.title.trim());
    for update in &incident.updates {
        let _ = write!(
            text,
            "\n## {} — {}\n\n_by {}_\n\n{}\n",
            update.created_at,
            update.status,
            update.created_by,
            update.message.trim_end()
        );
    }
    text
}

/// The file name `export --all` and `--out` use.
pub fn file_name(incident: &Incident) -> String {
    format!("incident-{}.md", incident.id)
}

#[cfg(test)]
mod tests {
    use tocsin_core::incident::{
        Actor, Impact, Incident, IncidentKind, IncidentStatus, IncidentUpdate, Timestamp,
    };

    use super::markdown;

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    #[test]
    fn an_incident_renders_as_front_matter_and_its_updates() {
        let oncall = Actor::token("oncall");
        let incident = Incident {
            id: 7,
            kind: IncidentKind::Incident,
            title: "API errors: \"500\" on /v1".into(),
            impact: Impact::Major,
            status: IncidentStatus::Resolved,
            components: vec!["api".into(), "web".into()],
            scheduled_start: None,
            scheduled_end: None,
            resolved_at: Some(at("2026-10-23T15:00:00Z")),
            created_at: at("2026-10-23T14:05:00Z"),
            created_by: Actor::token("ci"),
            updated_at: at("2026-10-23T15:00:00Z"),
            updated_by: oncall.clone(),
            updates: vec![
                IncidentUpdate {
                    id: 1,
                    status: IncidentStatus::Investigating,
                    message: "Looking into **elevated** errors.\n".into(),
                    created_at: at("2026-10-23T14:05:00Z"),
                    created_by: Actor::token("ci"),
                },
                IncidentUpdate {
                    id: 2,
                    status: IncidentStatus::Resolved,
                    message: "Rolled back.\n\n- web fixed\n- api fixed".into(),
                    created_at: at("2026-10-23T15:00:00.250Z"),
                    created_by: oncall,
                },
            ],
        };
        let expected = r#"---
id: 7
kind: "incident"
title: "API errors: \"500\" on /v1"
impact: "major"
status: "resolved"
components: ["api","web"]
resolved_at: "2026-10-23T15:00:00.000Z"
created_at: "2026-10-23T14:05:00.000Z"
created_by: "token:ci"
updated_at: "2026-10-23T15:00:00.000Z"
updated_by: "token:oncall"
---

# API errors: "500" on /v1

## 2026-10-23T14:05:00.000Z — investigating

_by token:ci_

Looking into **elevated** errors.

## 2026-10-23T15:00:00.250Z — resolved

_by token:oncall_

Rolled back.

- web fixed
- api fixed
"#;
        assert_eq!(markdown(&incident), expected);
    }

    #[test]
    fn maintenance_carries_its_schedule() {
        let incident = Incident {
            id: 8,
            kind: IncidentKind::Maintenance,
            title: "Database upgrade".into(),
            impact: Impact::None,
            status: IncidentStatus::Scheduled,
            components: vec![],
            scheduled_start: Some(at("2026-11-01T02:00:00Z")),
            scheduled_end: Some(at("2026-11-01T03:00:00Z")),
            resolved_at: None,
            created_at: at("2026-10-23T14:05:00Z"),
            created_by: Actor::token("ci"),
            updated_at: at("2026-10-23T14:05:00Z"),
            updated_by: Actor::token("ci"),
            updates: vec![],
        };
        let text = markdown(&incident);
        assert!(text.contains("components: []\nscheduled_start: \"2026-11-01T02:00:00.000Z\"\nscheduled_end: \"2026-11-01T03:00:00.000Z\"\ncreated_at"), "{text}");
        assert!(!text.contains("resolved_at"), "{text}");
    }
}
