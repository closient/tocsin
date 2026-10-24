// SPDX-License-Identifier: AGPL-3.0-only

//! What subscriber mail says. Every message is plain text and ends with
//! where to reply: From and Reply-To are `[server.email]`'s, which config
//! validation keeps answerable. A Slack channel gets the same text, ending
//! with links to manage or remove it and the address to write to.

use std::fmt::Write as _;
use std::time::Duration;

use serde_json::{Value, json};
use tocsin_core::config::Config;
use tocsin_core::incident::{Incident, IncidentKind, IncidentUpdate};
use tocsin_core::release::Release;
use tocsin_store::Preferences;

use super::paths;

/// A subject and a body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Text {
    /// The subject line.
    pub subject: String,
    /// The plain-text body.
    pub body: String,
}

/// The status page's name, as subjects start with it.
pub fn site(config: &Config) -> &str {
    &config.page.title
}

/// The one email an unconfirmed address can receive.
pub fn confirmation(config: &Config, base: &str, token: &str, within: Duration) -> Text {
    let site = site(config);
    Text {
        subject: format!("Confirm your subscription to {site}"),
        body: format!(
            "Someone, hopefully you, asked to get email from {site} when an \
             incident is opened or updated, or a release is published.\n\n\
             To start, confirm here:\n\n{base}{confirm}\n\n\
             The link works for {within}. If you did not ask, ignore this \
             email: nothing more will be sent, and the address is forgotten \
             when the link expires.\n\n\
             Questions? Reply to this email.\n",
            confirm = paths::confirm(token),
            within = in_words(within),
        ),
    }
}

/// What an already-confirmed address receives when someone signs it up
/// again: its own links, so only its owner can change anything.
pub fn already_subscribed(config: &Config, base: &str, manage: &str) -> Text {
    let site = site(config);
    Text {
        subject: format!("You are already subscribed to {site}"),
        body: format!(
            "Someone, hopefully you, asked to subscribe this address to \
             {site}. It already is, so nothing changed.\n\n\
             To choose what you hear about, or to unsubscribe:\n\n{base}{manage_path}\n\n\
             Questions? Reply to this email.\n",
            manage_path = paths::manage(manage),
        ),
    }
}

/// A new incident or maintenance window.
pub fn incident_opened(config: &Config, base: &str, incident: &Incident) -> Text {
    let site = site(config);
    let what = match incident.kind {
        IncidentKind::Incident => "Incident",
        IncidentKind::Maintenance => "Maintenance",
    };
    let mut body = format!("{what}: {}\n", incident.title);
    describe(&mut body, config, incident);
    if let Some(update) = incident.updates.first() {
        let _ = write!(body, "\n{}\n", update.message.trim());
    }
    let _ = write!(body, "\nFollow it at {base}/incidents/{}\n", incident.id);
    Text {
        subject: format!("[{site}] {what}: {}", incident.title),
        body,
    }
}

/// An update to an incident.
pub fn incident_updated(
    config: &Config,
    base: &str,
    incident: &Incident,
    update: &IncidentUpdate,
) -> Text {
    let site = site(config);
    let status = update.status.as_str().replace('_', " ");
    let mut body = format!("{}: {status}\n", incident.title);
    describe(&mut body, config, incident);
    let _ = write!(body, "\n{}\n", update.message.trim());
    let _ = write!(body, "\nFollow it at {base}/incidents/{}\n", incident.id);
    Text {
        subject: format!("[{site}] {status}: {}", incident.title),
        body,
    }
}

/// A release of a public project.
pub fn release(config: &Config, base: &str, release: &Release) -> Text {
    let site = site(config);
    let project = config
        .projects
        .iter()
        .find(|project| project.id.as_str() == release.project)
        .map_or(release.project.as_str(), |project| project.name.as_str());
    let mut body = format!(
        "{project} {}: {}\n",
        release.version,
        release.display_title()
    );
    let notes = release.notes.trim();
    if !notes.is_empty() {
        let _ = write!(body, "\n{notes}\n");
    }
    let _ = write!(
        body,
        "\nRead it at {base}/releases/{}/{}\n",
        release.project, release.version
    );
    Text {
        subject: format!("[{site}] {project} {} released", release.version),
        body,
    }
}

/// The lines every announcement ends with, for one recipient.
pub fn footer(base: &str, manage: &str) -> String {
    format!(
        "\n--\nYou get this because you subscribed to updates. Questions? Reply \
         to this email.\nChoose what you hear about: {base}{manage_path}\n\
         Unsubscribe: {base}{unsubscribe_path}\n",
        manage_path = paths::manage(manage),
        unsubscribe_path = paths::unsubscribe(manage),
    )
}

/// The first message a Slack channel gets once added: what it will hear
/// about, from where.
pub fn slack_welcome(config: &Config, base: &str, preferences: &Preferences) -> Text {
    let site = site(config);
    let what = match (
        preferences.components.is_empty(),
        preferences.projects.is_empty(),
    ) {
        (false, false) => "an incident is opened or updated, or a release is published",
        (false, true) => "an incident is opened or updated",
        (true, _) => "a release is published",
    };
    Text {
        subject: format!("This channel now hears from {site}"),
        body: format!(
            "Someone added {site} to this channel. It gets a message when {what}.\n\n\
             The status page: {base}/\n"
        ),
    }
}

/// The most of a message's text one Slack section holds (Slack allows
/// 3000 characters), leaving room for the subject.
const SLACK_SECTION: usize = 2800;

/// `text` as a Slack message, ending with links to manage or remove the
/// channel (`manage` is its token) and the address to write to.
pub fn slack(base: &str, manage: &str, contact: &str, text: &Text) -> Value {
    use crate::slack::escape;
    let mut body = text.body.trim().to_owned();
    if body.chars().count() > SLACK_SECTION {
        body = body.chars().take(SLACK_SECTION).collect::<String>() + "…";
    }
    let footer = format!(
        "<{base}{manage_path}|Choose what this channel hears> · \
         <{base}{remove_path}|Remove this channel> · \
         Questions? <mailto:{contact}|{contact}>",
        manage_path = paths::slack_manage(manage),
        remove_path = paths::slack_unsubscribe(manage),
        contact = escape(contact),
    );
    json!({
        "text": escape(&text.subject),
        "unfurl_links": false,
        "unfurl_media": false,
        "blocks": [
            {
                "type": "section",
                "text": {
                    "type": "mrkdwn",
                    "text": format!("*{}*\n{}", escape(&text.subject), escape(&body)),
                },
            },
            {
                "type": "context",
                "elements": [{ "type": "mrkdwn", "text": footer }],
            },
        ],
    })
}

/// The impact and components, one line each.
fn describe(body: &mut String, config: &Config, incident: &Incident) {
    if incident.kind == IncidentKind::Incident {
        let _ = writeln!(body, "Impact: {}", incident.impact.as_str());
    }
    if !incident.components.is_empty() {
        let names = incident
            .components
            .iter()
            .map(|id| {
                config
                    .components
                    .iter()
                    .find(|component| component.id.as_str() == id)
                    .map_or(id.as_str(), |component| component.name.as_str())
            })
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(body, "Affects: {names}");
    }
}

/// `2d` as "2 days", `90m` as "90 minutes".
fn in_words(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let (count, unit) = if seconds.is_multiple_of(86_400) {
        (seconds / 86_400, "day")
    } else if seconds.is_multiple_of(3600) {
        (seconds / 3600, "hour")
    } else {
        (seconds.div_ceil(60), "minute")
    };
    if count == 1 {
        format!("1 {unit}")
    } else {
        format!("{count} {unit}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_as_words() {
        assert_eq!(in_words(Duration::from_hours(48)), "2 days");
        assert_eq!(in_words(Duration::from_hours(24)), "1 day");
        assert_eq!(in_words(Duration::from_hours(3)), "3 hours");
        assert_eq!(in_words(Duration::from_mins(90)), "90 minutes");
    }
}
