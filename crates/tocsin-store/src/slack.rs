// SPDX-License-Identifier: AGPL-3.0-only

//! Slack connections: incoming webhooks made by signing in to Slack, for a
//! notifier (an operator connection, found by name) or for a public
//! subscriber (a subscriber connection, with preferences and its own
//! delivery log beside the email one).
//!
//! The webhook URL is a secret. It is written once, read only to post,
//! and travels as a [`WebhookUrl`], whose `Debug` hides it; no listing
//! returns it.

use std::fmt;

use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};

use crate::error::{Error, Result};
use crate::subscriber::{Lane, Preferences, preferences, replace_preferences};

/// A Slack incoming webhook URL: anyone holding it can post to the
/// channel. Its `Debug` never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct WebhookUrl(String);

impl WebhookUrl {
    /// Wraps a URL Slack handed over.
    #[must_use]
    pub fn new(url: String) -> Self {
        WebhookUrl(url)
    }

    /// The URL itself, to post to and nothing else.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for WebhookUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WebhookUrl([redacted])")
    }
}

/// Who a connection is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlackPurpose {
    /// Operator alerts, through a `[[notifier]]` naming it.
    Operator,
    /// A public subscriber's announcements.
    Subscriber,
}

impl SlackPurpose {
    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SlackPurpose::Operator => "operator",
            SlackPurpose::Subscriber => "subscriber",
        }
    }

    fn parse(text: &str) -> rusqlite::Result<Self> {
        match text {
            "operator" => Ok(SlackPurpose::Operator),
            "subscriber" => Ok(SlackPurpose::Subscriber),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                format!("unknown Slack connection purpose `{other}`").into(),
            )),
        }
    }
}

/// A connection Slack just approved, as handed to
/// [`Store::connect_slack`](crate::Store::connect_slack).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSlackConnection {
    /// Who it is for.
    pub purpose: SlackPurpose,
    /// The name notifiers use, for an operator connection; `None` for a
    /// subscriber's.
    pub name: Option<String>,
    /// The webhook to post to.
    pub webhook_url: WebhookUrl,
    /// The channel's name, as Slack shows it (`#ops-alerts`).
    pub channel: String,
    /// The channel's id.
    pub channel_id: Option<String>,
    /// The workspace's name.
    pub team: String,
    /// The workspace's id.
    pub team_id: Option<String>,
    /// Where the workspace's admins manage the webhook on Slack.
    pub configuration_url: Option<String>,
    /// The admin who connected it, for an operator connection.
    pub created_by: Option<String>,
    /// The secret half of a subscriber connection's manage link; `None`
    /// for an operator's.
    pub manage_secret: Option<String>,
    /// What a subscriber connection hears about; empty for an operator's.
    pub preferences: Preferences,
    /// Now, unix milliseconds UTC.
    pub now_ms: i64,
}

/// One stored connection, without its webhook URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackConnection {
    /// The row id.
    pub id: i64,
    /// Who it is for.
    pub purpose: SlackPurpose,
    /// The name notifiers use, for an operator connection.
    pub name: Option<String>,
    /// The channel's name.
    pub channel: String,
    /// The channel's id.
    pub channel_id: Option<String>,
    /// The workspace's name.
    pub team: String,
    /// The workspace's id.
    pub team_id: Option<String>,
    /// Where the workspace's admins manage the webhook on Slack.
    pub configuration_url: Option<String>,
    /// The admin who connected it.
    pub created_by: Option<String>,
    /// When it was connected, unix milliseconds UTC.
    pub created_at_ms: i64,
    /// The secret half of a subscriber connection's manage link.
    pub manage_secret: Option<String>,
    /// When Slack revoked it, if it did.
    pub dead_at_ms: Option<i64>,
    /// What Slack said when it did.
    pub dead_reason: Option<String>,
    /// What a subscriber connection hears about.
    pub preferences: Preferences,
}

impl SlackConnection {
    /// Whether it can still be posted to.
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.dead_at_ms.is_none()
    }
}

/// What a notifier finds under a connection name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlackHook {
    /// A live connection: post here.
    Live {
        /// The connection's id, to mark it dead if Slack refuses.
        id: i64,
        /// The webhook.
        url: WebhookUrl,
    },
    /// Slack revoked it.
    Dead {
        /// What Slack said.
        reason: String,
    },
    /// There is no connection by that name.
    Missing,
}

/// How many connections there are, by purpose and liveness.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SlackCounts {
    /// Operator connections that work.
    pub operator_live: i64,
    /// Operator connections Slack revoked.
    pub operator_dead: i64,
    /// Subscriber connections that work.
    pub subscriber_live: i64,
    /// Subscriber connections Slack revoked.
    pub subscriber_dead: i64,
}

/// A Slack delivery claimed for posting: marked `sending` and committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedSlack {
    /// The delivery's id.
    pub id: i64,
    /// The connection's id.
    pub connection_id: i64,
    /// Where to post.
    pub webhook_url: WebhookUrl,
    /// The secret half of the connection's manage link.
    pub manage_secret: String,
    /// The announcement's key.
    pub key: String,
    /// The announcement's subject.
    pub subject: String,
    /// The announcement's body.
    pub body: String,
    /// Attempts so far, this one included.
    pub attempts: u32,
}

const COLUMNS: &str = "id, purpose, name, channel, channel_id, team, team_id, configuration_url, \
                       created_by, created_at, manage_secret, dead_at, dead_reason";

fn connection_row(row: &Row<'_>) -> rusqlite::Result<SlackConnection> {
    Ok(SlackConnection {
        id: row.get(0)?,
        purpose: SlackPurpose::parse(&row.get::<_, String>(1)?)?,
        name: row.get(2)?,
        channel: row.get(3)?,
        channel_id: row.get(4)?,
        team: row.get(5)?,
        team_id: row.get(6)?,
        configuration_url: row.get(7)?,
        created_by: row.get(8)?,
        created_at_ms: row.get(9)?,
        manage_secret: row.get(10)?,
        dead_at_ms: row.get(11)?,
        dead_reason: row.get(12)?,
        preferences: Preferences::default(),
    })
}

/// The connection with this id.
pub(crate) fn by_id(conn: &Connection, id: i64) -> Result<Option<SlackConnection>> {
    let found = conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM slack_connection WHERE id = ?1"),
            [id],
            connection_row,
        )
        .optional()?;
    let Some(mut connection) = found else {
        return Ok(None);
    };
    connection.preferences = preferences(conn, Lane::Slack, connection.id)?;
    Ok(Some(connection))
}

/// Stores a connection Slack approved. An operator connection replaces the
/// one of the same name, dead or alive, which is how a revoked one is
/// mended. A subscriber connection replaces any other subscriber
/// connection to the same channel, so a channel added twice hears each
/// announcement once.
pub(crate) fn connect(
    conn: &mut Connection,
    connection: &NewSlackConnection,
) -> Result<SlackConnection> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if connection.purpose == SlackPurpose::Subscriber
        && let (Some(team_id), Some(channel_id)) = (&connection.team_id, &connection.channel_id)
    {
        transaction.execute(
            "DELETE FROM slack_connection
             WHERE purpose = 'subscriber' AND team_id = ?1 AND channel_id = ?2",
            params![team_id, channel_id],
        )?;
    }
    transaction.execute(
        "INSERT INTO slack_connection
             (purpose, name, webhook_url, channel, channel_id, team, team_id,
              configuration_url, created_by, created_at, manage_secret)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT (name) DO UPDATE SET
             webhook_url = excluded.webhook_url, channel = excluded.channel,
             channel_id = excluded.channel_id, team = excluded.team,
             team_id = excluded.team_id, configuration_url = excluded.configuration_url,
             created_by = excluded.created_by, created_at = excluded.created_at,
             dead_at = NULL, dead_reason = NULL",
        params![
            connection.purpose.as_str(),
            connection.name,
            connection.webhook_url.expose(),
            connection.channel,
            connection.channel_id,
            connection.team,
            connection.team_id,
            connection.configuration_url,
            connection.created_by,
            connection.now_ms,
            connection.manage_secret,
        ],
    )?;
    let id: i64 = match &connection.name {
        Some(name) => transaction.query_row(
            "SELECT id FROM slack_connection WHERE name = ?1",
            [name],
            |row| row.get(0),
        )?,
        None => transaction.last_insert_rowid(),
    };
    replace_preferences(&transaction, Lane::Slack, id, &connection.preferences)?;
    let stored = by_id(&transaction, id)?;
    transaction.commit()?;
    stored.ok_or(Error::Sqlite(rusqlite::Error::QueryReturnedNoRows))
}

/// Every connection for `purpose`, operator ones by name, subscriber ones
/// newest first.
pub(crate) fn list(conn: &Connection, purpose: SlackPurpose) -> Result<Vec<SlackConnection>> {
    let order = match purpose {
        SlackPurpose::Operator => "name",
        SlackPurpose::Subscriber => "id DESC",
    };
    let mut statement = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM slack_connection WHERE purpose = ?1 ORDER BY {order}"
    ))?;
    let rows = statement
        .query_map([purpose.as_str()], connection_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|mut connection| {
            connection.preferences = preferences(conn, Lane::Slack, connection.id)?;
            Ok(connection)
        })
        .collect()
}

/// The operator connection named `name`, for a notifier to post through.
pub(crate) fn hook(conn: &Connection, name: &str) -> Result<SlackHook> {
    let found = conn
        .query_row(
            "SELECT id, webhook_url, dead_reason FROM slack_connection
             WHERE purpose = 'operator' AND name = ?1",
            [name],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?;
    Ok(match found {
        None => SlackHook::Missing,
        Some((_, _, Some(reason))) => SlackHook::Dead { reason },
        Some((id, url, None)) => SlackHook::Live {
            id,
            url: WebhookUrl::new(url),
        },
    })
}

/// Every operator connection's name and whether it is live.
pub(crate) fn operator_states(conn: &Connection) -> Result<Vec<(String, bool)>> {
    let mut statement = conn.prepare_cached(
        "SELECT name, dead_at IS NULL FROM slack_connection
         WHERE purpose = 'operator' ORDER BY name",
    )?;
    Ok(statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn counts(conn: &Connection) -> Result<SlackCounts> {
    let mut statement = conn.prepare_cached(
        "SELECT purpose, dead_at IS NULL, count(*) FROM slack_connection GROUP BY 1, 2",
    )?;
    let mut counts = SlackCounts::default();
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, bool>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    for row in rows {
        let (purpose, live, count) = row?;
        let slot = match (SlackPurpose::parse(&purpose)?, live) {
            (SlackPurpose::Operator, true) => &mut counts.operator_live,
            (SlackPurpose::Operator, false) => &mut counts.operator_dead,
            (SlackPurpose::Subscriber, true) => &mut counts.subscriber_live,
            (SlackPurpose::Subscriber, false) => &mut counts.subscriber_dead,
        };
        *slot = count;
    }
    Ok(counts)
}

/// Marks a connection dead with Slack's reason, and fails every delivery
/// still waiting for it in the same transaction, so none waits forever.
/// Resolves with whether it was live.
pub(crate) fn kill(conn: &mut Connection, id: i64, reason: &str, now_ms: i64) -> Result<bool> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let killed = transaction.execute(
        "UPDATE slack_connection SET dead_at = max(?2, created_at), dead_reason = ?3
         WHERE id = ?1 AND dead_at IS NULL",
        params![id, now_ms, reason],
    )?;
    transaction.execute(
        "UPDATE slack_delivery
         SET status = 'failed', finished_at = ?2,
             last_error = 'the Slack connection is dead: ' || ?3
         WHERE connection_id = ?1 AND status = 'pending'",
        params![id, now_ms, reason],
    )?;
    transaction.commit()?;
    Ok(killed > 0)
}

pub(crate) fn remove(conn: &mut Connection, id: i64) -> Result<bool> {
    // Subscriptions and deliveries go with it (ON DELETE CASCADE).
    Ok(conn.execute("DELETE FROM slack_connection WHERE id = ?1", [id])? > 0)
}

pub(crate) fn set_preferences(
    conn: &mut Connection,
    id: i64,
    preferences: &Preferences,
) -> Result<bool> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let exists = transaction
        .query_row(
            "SELECT 1 FROM slack_connection WHERE id = ?1 AND purpose = 'subscriber'",
            [id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if exists {
        replace_preferences(&transaction, Lane::Slack, id, preferences)?;
    }
    transaction.commit()?;
    Ok(exists)
}

/// Marks a due Slack delivery `sending` and returns what to post, if it is
/// still pending and its connection is still live.
pub(crate) fn claim(conn: &mut Connection, id: i64) -> Result<Option<ClaimedSlack>> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let claimed = transaction.execute(
        "UPDATE slack_delivery SET status = 'sending', attempts = attempts + 1
         WHERE id = ?1 AND status = 'pending' AND EXISTS (
             SELECT 1 FROM slack_connection c
             WHERE c.id = slack_delivery.connection_id AND c.dead_at IS NULL
         )",
        [id],
    )?;
    let post = if claimed == 0 {
        None
    } else {
        Some(transaction.query_row(
            "SELECT d.id, d.connection_id, c.webhook_url, c.manage_secret, e.key, e.subject,
                    e.body, d.attempts
             FROM slack_delivery d
             JOIN slack_connection c ON c.id = d.connection_id
             JOIN subscriber_event e ON e.id = d.event_id
             WHERE d.id = ?1",
            [id],
            |row| {
                Ok(ClaimedSlack {
                    id: row.get(0)?,
                    connection_id: row.get(1)?,
                    webhook_url: WebhookUrl::new(row.get(2)?),
                    manage_secret: row.get(3)?,
                    key: row.get(4)?,
                    subject: row.get(5)?,
                    body: row.get(6)?,
                    attempts: row.get(7)?,
                })
            },
        )?)
    };
    transaction.commit()?;
    Ok(post)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_webhook_url_never_shows_in_debug() {
        let url = WebhookUrl::new("https://hooks.slack.com/services/T0/B0/xyz".to_owned());
        assert_eq!(format!("{url:?}"), "WebhookUrl([redacted])");
        let hook = SlackHook::Live { id: 1, url };
        assert!(!format!("{hook:?}").contains("hooks.slack.com"));
    }
}
