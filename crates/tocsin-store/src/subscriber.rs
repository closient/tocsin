// SPDX-License-Identifier: AGPL-3.0-only

//! Email subscribers, what they subscribe to, and the delivery log that
//! sends each of them each announcement at most once. Slack subscriber
//! connections (`crate::slack`) share the announcements, through tables
//! of the same shape.
//!
//! The store holds no cryptography: callers hand it the SHA-256 of each
//! confirmation token and the random secret of each manage link, and
//! compare secrets themselves.

use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params};

use crate::error::{Error, Result};

/// A subscription target meaning every component, or every public
/// project, including ones added later.
pub const EVERY: &str = "*";

/// What a subscriber hears about. Each list holds ids, or [`EVERY`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Preferences {
    /// Components whose incidents they hear about.
    pub components: Vec<String>,
    /// Projects whose releases they hear about.
    pub projects: Vec<String>,
}

impl Preferences {
    /// Whether they hear about nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.components.is_empty() && self.projects.is_empty()
    }
}

/// One stored subscriber.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscriber {
    /// The row id.
    pub id: i64,
    /// The address, lowercased.
    pub email: String,
    /// When they first signed up, unix milliseconds UTC.
    pub created_at_ms: i64,
    /// When they confirmed, or `None` while unconfirmed.
    pub confirmed_at_ms: Option<i64>,
    /// When the confirmation link stops working, while unconfirmed.
    pub confirm_expires_at_ms: Option<i64>,
    /// The secret half of their manage link.
    pub manage_secret: String,
    /// What they hear about.
    pub preferences: Preferences,
}

/// A sign-up, as handed to [`Store::sign_up`](crate::Store::sign_up).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSignup {
    /// The address, lowercased.
    pub email: String,
    /// Hex SHA-256 of the confirmation token to send.
    pub confirm_hash: String,
    /// When that token stops working, unix milliseconds UTC.
    pub confirm_expires_at_ms: i64,
    /// The manage secret for a new subscriber.
    pub manage_secret: String,
    /// What they asked to hear about.
    pub preferences: Preferences,
    /// Now, unix milliseconds UTC.
    pub now_ms: i64,
}

/// What a sign-up found and did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signup {
    /// A new, unconfirmed subscriber: send the confirmation.
    New(Subscriber),
    /// The address had signed up but not confirmed: its token and
    /// preferences are replaced. Send the confirmation again.
    Unconfirmed(Subscriber),
    /// The address is already confirmed and nothing changed: send it its
    /// manage link, so only its owner can change anything.
    Confirmed(Subscriber),
}

/// How many subscribers there are.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubscriberCounts {
    /// Confirmed, and so receiving mail.
    pub confirmed: i64,
    /// Waiting for confirmation.
    pub unconfirmed: i64,
}

/// Which subscriptions an announcement matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Incidents on components.
    Component,
    /// Releases of projects.
    Project,
}

impl Scope {
    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Component => "component",
            Scope::Project => "project",
        }
    }
}

/// One announcement, as handed to
/// [`Store::queue_announcement`](crate::Store::queue_announcement).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAnnouncement {
    /// What it announces, unique: the same key is never queued twice.
    pub key: String,
    /// Which subscriptions it matches.
    pub scope: Scope,
    /// The components or project it is about. An incident naming none
    /// reaches every subscriber to incidents.
    pub targets: Vec<String>,
    /// The email subject.
    pub subject: String,
    /// The email body, before each recipient's manage link.
    pub body: String,
    /// Now, unix milliseconds UTC.
    pub created_at_ms: i64,
}

/// The deliveries due now, and when the next one is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mailbag {
    /// Due deliveries' ids, oldest first.
    pub due: Vec<i64>,
    /// When the earliest delivery not yet due is, if any.
    pub next_at_ms: Option<i64>,
}

/// A delivery claimed for sending: marked `sending` and committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedMail {
    /// The delivery's id.
    pub id: i64,
    /// The recipient's id.
    pub subscriber_id: i64,
    /// The recipient's address.
    pub email: String,
    /// The secret half of their manage link.
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

/// How an attempt to send a claimed delivery ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailOutcome {
    /// The SMTP server accepted it.
    Sent,
    /// Worth trying again at this time, unix milliseconds UTC.
    Retry {
        /// When.
        at_ms: i64,
        /// Why this attempt failed.
        error: String,
    },
    /// Given up on.
    Failed {
        /// Why.
        error: String,
    },
}

/// What a prune removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubscriberPrune {
    /// Unconfirmed subscribers whose link expired.
    pub expired: usize,
    /// Finished deliveries, email and Slack, past retention.
    pub deliveries: usize,
}

/// Which kind of subscriber a query is about: an email address or a Slack
/// channel. Both hear about the same announcements, through tables of the
/// same shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lane {
    /// `subscriber`, `subscription`, `subscriber_delivery`.
    Mail,
    /// `slack_connection`, `slack_subscription`, `slack_delivery`.
    Slack,
}

impl Lane {
    fn subscriptions(self) -> &'static str {
        match self {
            Lane::Mail => "subscription",
            Lane::Slack => "slack_subscription",
        }
    }

    fn owner(self) -> &'static str {
        match self {
            Lane::Mail => "subscriber_id",
            Lane::Slack => "connection_id",
        }
    }

    fn deliveries(self) -> &'static str {
        match self {
            Lane::Mail => "subscriber_delivery",
            Lane::Slack => "slack_delivery",
        }
    }
}

const SUBSCRIBER_COLUMNS: &str =
    "id, email, created_at, confirmed_at, confirm_expires_at, manage_secret";

fn subscriber_row(row: &Row<'_>) -> rusqlite::Result<Subscriber> {
    Ok(Subscriber {
        id: row.get(0)?,
        email: row.get(1)?,
        created_at_ms: row.get(2)?,
        confirmed_at_ms: row.get(3)?,
        confirm_expires_at_ms: row.get(4)?,
        manage_secret: row.get(5)?,
        preferences: Preferences::default(),
    })
}

pub(crate) fn preferences(conn: &Connection, lane: Lane, id: i64) -> Result<Preferences> {
    let mut statement = conn.prepare_cached(&format!(
        "SELECT kind, target FROM {} WHERE {} = ?1 ORDER BY kind, target",
        lane.subscriptions(),
        lane.owner()
    ))?;
    let mut preferences = Preferences::default();
    let rows = statement.query_map([id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (kind, target) = row?;
        if kind == Scope::Component.as_str() {
            preferences.components.push(target);
        } else {
            preferences.projects.push(target);
        }
    }
    Ok(preferences)
}

fn with_preferences(
    conn: &Connection,
    subscriber: Option<Subscriber>,
) -> Result<Option<Subscriber>> {
    let Some(mut subscriber) = subscriber else {
        return Ok(None);
    };
    subscriber.preferences = preferences(conn, Lane::Mail, subscriber.id)?;
    Ok(Some(subscriber))
}

/// The subscriber with this id.
pub(crate) fn by_id(conn: &Connection, id: i64) -> Result<Option<Subscriber>> {
    let found = conn
        .query_row(
            &format!("SELECT {SUBSCRIBER_COLUMNS} FROM subscriber WHERE id = ?1"),
            [id],
            subscriber_row,
        )
        .optional()?;
    with_preferences(conn, found)
}

/// The unconfirmed subscriber whose confirmation token hashes to `hash`,
/// if that token has not expired.
pub(crate) fn by_confirmation(
    conn: &Connection,
    hash: &str,
    now_ms: i64,
) -> Result<Option<Subscriber>> {
    let found = conn
        .query_row(
            &format!(
                "SELECT {SUBSCRIBER_COLUMNS} FROM subscriber
                 WHERE confirm_hash = ?1 AND confirm_expires_at > ?2"
            ),
            params![hash, now_ms],
            subscriber_row,
        )
        .optional()?;
    with_preferences(conn, found)
}

pub(crate) fn replace_preferences(
    transaction: &Transaction<'_>,
    lane: Lane,
    id: i64,
    preferences: &Preferences,
) -> Result<()> {
    let (table, owner) = (lane.subscriptions(), lane.owner());
    transaction.execute(&format!("DELETE FROM {table} WHERE {owner} = ?1"), [id])?;
    let mut insert = transaction.prepare_cached(&format!(
        "INSERT OR IGNORE INTO {table} ({owner}, kind, target) VALUES (?1, ?2, ?3)"
    ))?;
    for (scope, targets) in [
        (Scope::Component, &preferences.components),
        (Scope::Project, &preferences.projects),
    ] {
        // `*` covers every id, so the ids beside it are dropped.
        let every = targets.iter().any(|target| target == EVERY);
        for target in targets {
            if !every || target == EVERY {
                insert.execute(params![id, scope.as_str(), target])?;
            }
        }
    }
    Ok(())
}

pub(crate) fn sign_up(conn: &mut Connection, signup: &NewSignup) -> Result<Signup> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let existing = transaction
        .query_row(
            "SELECT id, confirmed_at IS NOT NULL FROM subscriber WHERE email = ?1",
            [&signup.email],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?)),
        )
        .optional()?;
    let (id, outcome): (i64, fn(Subscriber) -> Signup) = match existing {
        Some((id, true)) => (id, Signup::Confirmed),
        Some((id, false)) => {
            transaction.execute(
                "UPDATE subscriber SET confirm_hash = ?2, confirm_expires_at = ?3 WHERE id = ?1",
                params![id, signup.confirm_hash, signup.confirm_expires_at_ms],
            )?;
            replace_preferences(&transaction, Lane::Mail, id, &signup.preferences)?;
            (id, Signup::Unconfirmed)
        }
        None => {
            transaction.execute(
                "INSERT INTO subscriber
                     (email, created_at, confirm_hash, confirm_expires_at, manage_secret)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    signup.email,
                    signup.now_ms,
                    signup.confirm_hash,
                    signup.confirm_expires_at_ms,
                    signup.manage_secret
                ],
            )?;
            let id = transaction.last_insert_rowid();
            replace_preferences(&transaction, Lane::Mail, id, &signup.preferences)?;
            (id, Signup::New)
        }
    };
    let subscriber = by_id(&transaction, id)?;
    transaction.commit()?;
    // Read in this transaction, so it is there.
    subscriber
        .map(outcome)
        .ok_or(Error::Sqlite(rusqlite::Error::QueryReturnedNoRows))
}

pub(crate) fn confirm(
    conn: &mut Connection,
    hash: &str,
    now_ms: i64,
) -> Result<Option<Subscriber>> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let id: Option<i64> = transaction
        .query_row(
            "SELECT id FROM subscriber WHERE confirm_hash = ?1 AND confirm_expires_at > ?2",
            params![hash, now_ms],
            |row| row.get(0),
        )
        .optional()?;
    let Some(id) = id else {
        return Ok(None);
    };
    transaction.execute(
        "UPDATE subscriber
         SET confirmed_at = max(?2, created_at), confirm_hash = NULL, confirm_expires_at = NULL
         WHERE id = ?1",
        params![id, now_ms],
    )?;
    let subscriber = by_id(&transaction, id)?;
    transaction.commit()?;
    Ok(subscriber)
}

pub(crate) fn set_preferences(
    conn: &mut Connection,
    id: i64,
    preferences: &Preferences,
) -> Result<bool> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let exists = transaction
        .query_row("SELECT 1 FROM subscriber WHERE id = ?1", [id], |_| Ok(()))
        .optional()?
        .is_some();
    if exists {
        replace_preferences(&transaction, Lane::Mail, id, preferences)?;
    }
    transaction.commit()?;
    Ok(exists)
}

pub(crate) fn remove(conn: &mut Connection, id: i64) -> Result<bool> {
    // Subscriptions and deliveries go with it (ON DELETE CASCADE).
    Ok(conn.execute("DELETE FROM subscriber WHERE id = ?1", [id])? > 0)
}

pub(crate) fn list(conn: &Connection, after: Option<i64>, limit: u32) -> Result<Vec<Subscriber>> {
    let mut statement = conn.prepare_cached(&format!(
        "SELECT {SUBSCRIBER_COLUMNS} FROM subscriber
         WHERE ?1 IS NULL OR id > ?1 ORDER BY id LIMIT ?2"
    ))?;
    let rows = statement
        .query_map(params![after, limit], subscriber_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|mut subscriber| {
            subscriber.preferences = preferences(conn, Lane::Mail, subscriber.id)?;
            Ok(subscriber)
        })
        .collect()
}

pub(crate) fn counts(conn: &Connection) -> Result<SubscriberCounts> {
    Ok(conn.query_row(
        "SELECT count(confirmed_at), count(*) - count(confirmed_at) FROM subscriber",
        [],
        |row| {
            Ok(SubscriberCounts {
                confirmed: row.get(0)?,
                unconfirmed: row.get(1)?,
            })
        },
    )?)
}

pub(crate) fn queue_announcement(
    conn: &mut Connection,
    announcement: &NewAnnouncement,
) -> Result<bool> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let inserted = transaction.execute(
        "INSERT OR IGNORE INTO subscriber_event (key, scope, subject, body, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            announcement.key,
            announcement.scope.as_str(),
            announcement.subject,
            announcement.body,
            announcement.created_at_ms
        ],
    )?;
    if inserted > 0 {
        let id = transaction.last_insert_rowid();
        let mut insert = transaction.prepare_cached(
            "INSERT OR IGNORE INTO subscriber_event_target (event_id, target) VALUES (?1, ?2)",
        )?;
        for target in &announcement.targets {
            insert.execute(params![id, target])?;
        }
    }
    transaction.commit()?;
    Ok(inserted > 0)
}

/// Queues a delivery to every confirmed subscriber and every live Slack
/// subscriber connection each unexpanded announcement matches, and marks
/// it expanded, one announcement per transaction, so neither lane can be
/// queued without the other. Resolves with how many deliveries it queued.
pub(crate) fn expand(conn: &mut Connection, now_ms: i64) -> Result<usize> {
    let pending: Vec<i64> = {
        let mut statement = conn.prepare_cached(
            "SELECT id FROM subscriber_event WHERE expanded_at IS NULL ORDER BY id",
        )?;
        statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    let mut queued = 0;
    for event in pending {
        let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // A subscription matches when it is `*`, names one of the event's
        // targets, or the event names none (an incident on no component).
        queued += transaction.execute(
            "INSERT OR IGNORE INTO subscriber_delivery (subscriber_id, event_id, next_attempt_at)
             SELECT s.id, e.id, ?2
             FROM subscriber_event e
             JOIN subscriber s ON s.confirmed_at IS NOT NULL
             WHERE e.id = ?1 AND e.expanded_at IS NULL AND EXISTS (
                 SELECT 1 FROM subscription p
                 WHERE p.subscriber_id = s.id AND p.kind = e.scope AND (
                     p.target = '*'
                     OR NOT EXISTS (SELECT 1 FROM subscriber_event_target t WHERE t.event_id = e.id)
                     OR p.target IN (SELECT t.target FROM subscriber_event_target t WHERE t.event_id = e.id)
                 )
             )
             ORDER BY s.id",
            params![event, now_ms],
        )?;
        queued += transaction.execute(
            "INSERT OR IGNORE INTO slack_delivery (connection_id, event_id, next_attempt_at)
             SELECT c.id, e.id, ?2
             FROM subscriber_event e
             JOIN slack_connection c ON c.purpose = 'subscriber' AND c.dead_at IS NULL
             WHERE e.id = ?1 AND e.expanded_at IS NULL AND EXISTS (
                 SELECT 1 FROM slack_subscription p
                 WHERE p.connection_id = c.id AND p.kind = e.scope AND (
                     p.target = '*'
                     OR NOT EXISTS (SELECT 1 FROM subscriber_event_target t WHERE t.event_id = e.id)
                     OR p.target IN (SELECT t.target FROM subscriber_event_target t WHERE t.event_id = e.id)
                 )
             )
             ORDER BY c.id",
            params![event, now_ms],
        )?;
        transaction.execute(
            "UPDATE subscriber_event SET expanded_at = max(?2, created_at)
             WHERE id = ?1 AND expanded_at IS NULL",
            params![event, now_ms],
        )?;
        transaction.commit()?;
    }
    Ok(queued)
}

pub(crate) fn mailbag(conn: &Connection, lane: Lane, now_ms: i64, limit: u32) -> Result<Mailbag> {
    let table = lane.deliveries();
    let mut statement = conn.prepare_cached(&format!(
        "SELECT id FROM {table}
         WHERE status = 'pending' AND next_attempt_at <= ?1
         ORDER BY next_attempt_at, id LIMIT ?2"
    ))?;
    let due = statement
        .query_map(params![now_ms, limit], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let next_at_ms = conn.query_row(
        &format!(
            "SELECT min(next_attempt_at) FROM {table}
             WHERE status = 'pending' AND next_attempt_at > ?1"
        ),
        [now_ms],
        |row| row.get(0),
    )?;
    Ok(Mailbag { due, next_at_ms })
}

/// Marks a due delivery `sending` and returns what to send, if it is
/// still pending and its subscriber is still confirmed.
pub(crate) fn claim(conn: &mut Connection, id: i64) -> Result<Option<ClaimedMail>> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let claimed = transaction.execute(
        "UPDATE subscriber_delivery SET status = 'sending', attempts = attempts + 1
         WHERE id = ?1 AND status = 'pending' AND EXISTS (
             SELECT 1 FROM subscriber s
             WHERE s.id = subscriber_delivery.subscriber_id AND s.confirmed_at IS NOT NULL
         )",
        [id],
    )?;
    let mail = if claimed == 0 {
        None
    } else {
        Some(transaction.query_row(
            "SELECT d.id, d.subscriber_id, s.email, s.manage_secret, e.key, e.subject, e.body,
                    d.attempts
             FROM subscriber_delivery d
             JOIN subscriber s ON s.id = d.subscriber_id
             JOIN subscriber_event e ON e.id = d.event_id
             WHERE d.id = ?1",
            [id],
            |row| {
                Ok(ClaimedMail {
                    id: row.get(0)?,
                    subscriber_id: row.get(1)?,
                    email: row.get(2)?,
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
    Ok(mail)
}

pub(crate) fn finish(
    conn: &mut Connection,
    lane: Lane,
    id: i64,
    outcome: &MailOutcome,
    now_ms: i64,
) -> Result<()> {
    let table = lane.deliveries();
    match outcome {
        MailOutcome::Sent => conn.execute(
            &format!(
                "UPDATE {table} SET status = 'sent', last_error = NULL, finished_at = ?2
                 WHERE id = ?1 AND status = 'sending'"
            ),
            params![id, now_ms],
        )?,
        MailOutcome::Retry { at_ms, error } => conn.execute(
            &format!(
                "UPDATE {table} SET status = 'pending', last_error = ?2, next_attempt_at = ?3
                 WHERE id = ?1 AND status = 'sending'"
            ),
            params![id, error, at_ms],
        )?,
        MailOutcome::Failed { error } => conn.execute(
            &format!(
                "UPDATE {table} SET status = 'failed', last_error = ?2, finished_at = ?3
                 WHERE id = ?1 AND status = 'sending'"
            ),
            params![id, error, now_ms],
        )?,
    };
    Ok(())
}

/// Fails every delivery, email or Slack, left `sending`: the SMTP server
/// or Slack may have taken it before the process stopped, so it is never
/// sent again.
pub(crate) fn interrupt(conn: &mut Connection, now_ms: i64) -> Result<usize> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut interrupted = 0;
    for lane in [Lane::Mail, Lane::Slack] {
        interrupted += transaction.execute(
            &format!(
                "UPDATE {}
                 SET status = 'failed', finished_at = ?1,
                     last_error = 'interrupted while sending; not sent again, to avoid a duplicate'
                 WHERE status = 'sending'",
                lane.deliveries()
            ),
            [now_ms],
        )?;
    }
    transaction.commit()?;
    Ok(interrupted)
}

pub(crate) fn prune(
    conn: &mut Connection,
    now_ms: i64,
    finished_before_ms: i64,
) -> Result<SubscriberPrune> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let expired = transaction.execute(
        "DELETE FROM subscriber WHERE confirmed_at IS NULL AND confirm_expires_at <= ?1",
        [now_ms],
    )?;
    // An announcement keeps its row, and so its key, after its deliveries
    // go: it is expanded once, so a pruned delivery is never queued again.
    let mut deliveries = 0;
    for lane in [Lane::Mail, Lane::Slack] {
        deliveries += transaction.execute(
            &format!(
                "DELETE FROM {} WHERE finished_at IS NOT NULL AND finished_at < ?1",
                lane.deliveries()
            ),
            [finished_before_ms],
        )?;
    }
    transaction.commit()?;
    Ok(SubscriberPrune {
        expired,
        deliveries,
    })
}
