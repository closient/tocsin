// SPDX-License-Identifier: AGPL-3.0-only

//! Silences, and the messages they held back.
//!
//! A silence never stops an alert being written: the delivery worker marks
//! each message it holds back `silenced` with the silence's id, so what
//! happened while a silence held is read back from the outbox.

use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use tocsin_core::incident::{Actor, FieldError, Timestamp};
use tocsin_core::silence::{HeldMessage, Matchers, NewSilence, Silence, SilenceState};

use crate::error::{Error, Result};
use crate::history::subject_kind;
use crate::incident::{actor, snapshot, text};
use crate::rollup::parse_column;

/// The most held-back messages read with each silence, newest first;
/// `held_total` counts them all.
pub(crate) const HELD_LIMIT: u32 = 100;

const SILENCE_COLUMNS: &str = "s.id, s.node_id, s.subject_kind, s.subject_id, s.component_id,
    s.notifier_id, s.episode, s.reason, s.created_at, s.created_by_kind, s.created_by,
    s.expires_at, s.ended_at, s.ended_by_kind, s.ended_by, o.ended_at,
    (SELECT count(*) FROM alert_delivery d WHERE d.silence_id = s.id)";

const SILENCE_FROM: &str = "silence s LEFT JOIN outage o ON o.id = s.episode";

/// Whether silence `s` (joined to its episode as `o`) applies at `:now`.
const ACTIVE: &str =
    "s.ended_at IS NULL AND s.expires_at > :now AND (s.episode IS NULL OR o.ended_at IS NULL)";

fn silence_row(row: &Row<'_>, now: Timestamp) -> rusqlite::Result<Silence> {
    let optional_actor = |index: usize| -> rusqlite::Result<Option<Actor>> {
        match row.get::<_, Option<String>>(index)? {
            Some(_) => Ok(Some(actor(row, index)?)),
            None => Ok(None),
        }
    };
    let instant = |index: usize| -> rusqlite::Result<Option<Timestamp>> {
        Ok(row
            .get::<_, Option<i64>>(index)?
            .map(Timestamp::from_millis))
    };
    let mut silence = Silence {
        id: row.get(0)?,
        matchers: Matchers {
            node: row.get(1)?,
            subject_kind: match row.get::<_, Option<String>>(2)? {
                Some(_) => Some(subject_kind(2, row)?),
                None => None,
            },
            subject: row.get(3)?,
            component: row.get(4)?,
            notifier: row.get(5)?,
        },
        episode: row.get(6)?,
        reason: row.get(7)?,
        created_at: Timestamp::from_millis(row.get(8)?),
        created_by: actor(row, 9)?,
        expires_at: Timestamp::from_millis(row.get(11)?),
        ended_at: instant(12)?,
        ended_by: optional_actor(13)?,
        episode_ended_at: instant(15)?,
        state: SilenceState::Active,
        held: Vec::new(),
        held_total: row.get(16)?,
    };
    silence.state = silence.state_at(now);
    Ok(silence)
}

/// Fills in up to [`HELD_LIMIT`] held-back messages of each silence.
fn complete(conn: &Connection, silences: &mut [Silence]) -> Result<()> {
    let mut statement = conn.prepare_cached(
        "SELECT d.id, d.kind, d.health, d.finished_at, a.id, a.node_id, a.subject_kind,
             a.subject_id, a.notifier_id
         FROM alert_delivery d INDEXED BY alert_delivery_by_silence
         JOIN alert a ON a.id = d.alert_id
         WHERE d.silence_id = ?1
         ORDER BY d.id DESC LIMIT ?2",
    )?;
    for silence in silences {
        silence.held = statement
            .query_map(params![silence.id, HELD_LIMIT], |row| {
                Ok(HeldMessage {
                    delivery_id: row.get(0)?,
                    kind: text(row, 1)?,
                    health: parse_column(2, &row.get::<_, String>(2)?)?,
                    at: Timestamp::from_millis(row.get(3)?),
                    alert_id: row.get(4)?,
                    node: row.get(5)?,
                    subject_kind: subject_kind(6, row)?,
                    subject: row.get(7)?,
                    notifier: row.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
    }
    Ok(())
}

/// Inserts `new`, made by `actor` at `at` to last until `expires_at`, and
/// reads it back. The caller has validated `new`, resolved its expiry
/// against `at`, and scoped it to its episode; the episode is
/// checked again here, in the same transaction, so an outage that closed
/// since is refused rather than silenced after the fact.
pub(crate) fn insert(
    conn: &mut Connection,
    new: &NewSilence,
    expires_at: Timestamp,
    actor: &Actor,
    at: Timestamp,
) -> Result<Silence> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(episode) = new.episode {
        let open = transaction
            .prepare_cached("SELECT ended_at IS NULL FROM outage WHERE id = ?1")?
            .query_row([episode], |row| row.get::<_, bool>(0))
            .optional()?;
        match open {
            Some(true) => {}
            Some(false) => {
                return Err(Error::Invalid(vec![FieldError::new(
                    "episode",
                    format!("outage {episode} has ended; silence the alert instead"),
                )]));
            }
            None => {
                return Err(Error::Invalid(vec![FieldError::new(
                    "episode",
                    format!("no outage has id {episode}"),
                )]));
            }
        }
    }
    let matchers = &new.matchers;
    transaction
        .prepare_cached(
            "INSERT INTO silence (node_id, subject_kind, subject_id, component_id, notifier_id,
                 episode, reason, created_at, created_by_kind, created_by, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        )?
        .execute(params![
            matchers.node,
            matchers
                .subject_kind
                .map(tocsin_core::status::CheckKind::as_str),
            matchers.subject,
            matchers.component,
            matchers.notifier,
            new.episode,
            new.reason.trim(),
            at.as_millis(),
            actor.kind.as_str(),
            actor.id,
            expires_at.as_millis(),
        ])?;
    let id = transaction.last_insert_rowid();
    let silence = load(&transaction, id, at)?;
    transaction.commit()?;
    silence.ok_or_else(|| Error::Batch(format!("silence {id} was not read back")))
}

/// Silence `id` as of `now`, with what it held back.
pub(crate) fn load(conn: &Connection, id: i64, now: Timestamp) -> Result<Option<Silence>> {
    snapshot(conn, |conn| {
        let silence = conn
            .prepare_cached(&format!(
                "SELECT {SILENCE_COLUMNS} FROM {SILENCE_FROM} WHERE s.id = ?1"
            ))?
            .query_row([id], |row| silence_row(row, now))
            .optional()?;
        let Some(silence) = silence else {
            return Ok(None);
        };
        let mut silences = [silence];
        complete(conn, &mut silences)?;
        let [silence] = silences;
        Ok(Some(silence))
    })
}

/// Up to `limit` silences as of `now`, newest first; with `before`, only
/// those with a lower id.
pub(crate) fn list(
    conn: &Connection,
    before: Option<i64>,
    limit: u32,
    now: Timestamp,
) -> Result<Vec<Silence>> {
    snapshot(conn, |conn| {
        let mut silences = conn
            .prepare_cached(&format!(
                "SELECT {SILENCE_COLUMNS} FROM {SILENCE_FROM} WHERE s.id < ?1
                 ORDER BY s.id DESC LIMIT ?2"
            ))?
            .query_map(params![before.unwrap_or(i64::MAX), limit], |row| {
                silence_row(row, now)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        complete(conn, &mut silences)?;
        Ok(silences)
    })
}

/// Ends silence `id` early, by `actor` at `at`. One already over (ended,
/// expired or past its episode) is left as it is. `None` if no silence
/// has that id.
pub(crate) fn end(
    conn: &mut Connection,
    id: i64,
    actor: &Actor,
    at: Timestamp,
) -> Result<Option<Silence>> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction
        .prepare_cached(&format!(
            "UPDATE silence SET ended_at = :now, ended_by_kind = :kind, ended_by = :by
             WHERE id = :id AND id IN (SELECT s.id FROM {SILENCE_FROM} WHERE {ACTIVE})"
        ))?
        .execute(rusqlite::named_params! {
            ":id": id,
            ":now": at.as_millis(),
            ":kind": actor.kind.as_str(),
            ":by": actor.id,
        })?;
    let silence = load(&transaction, id, at)?;
    transaction.commit()?;
    Ok(silence)
}

/// A silence that applies now, as the delivery worker needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveSilence {
    /// Its id.
    pub id: i64,
    /// What it matches.
    pub matchers: Matchers,
    /// The outage it is scoped to, if any: it then holds only that
    /// outage's messages.
    pub episode: Option<i64>,
    /// When it stops applying, unless ended first.
    pub expires_at: Timestamp,
}

/// Every silence that applies at `now`.
pub(crate) fn active(conn: &Connection, now: Timestamp) -> Result<Vec<ActiveSilence>> {
    let mut statement = conn.prepare_cached(&format!(
        "SELECT {SILENCE_COLUMNS} FROM {SILENCE_FROM} WHERE {ACTIVE} ORDER BY s.id"
    ))?;
    let rows = statement.query_map(rusqlite::named_params! { ":now": now.as_millis() }, |row| {
        let silence = silence_row(row, now)?;
        Ok(ActiveSilence {
            id: silence.id,
            matchers: silence.matchers,
            episode: silence.episode,
            expires_at: silence.expires_at,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Marks pending fire or repeat `delivery_id` held back by silence
/// `silence_id` at `now`, instead of sending it. Refused (`false`) unless
/// the delivery is still pending, the silence still applies at `now`, and
/// a silence scoped to an episode is scoped to the outage the alert fired
/// for: the worker read all three a moment ago, and any may have moved
/// since.
pub(crate) fn hold(
    conn: &Connection,
    delivery_id: i64,
    silence_id: i64,
    now: Timestamp,
) -> Result<bool> {
    let changed = conn
        .prepare_cached(&format!(
            "UPDATE alert_delivery SET status = 'silenced', silence_id = :silence,
                 finished_at = :now
             WHERE id = :delivery AND status = 'pending' AND kind IN ('fire', 'repeat')
               AND EXISTS (SELECT 1 FROM {SILENCE_FROM}
                           WHERE s.id = :silence AND {ACTIVE}
                             AND (s.episode IS NULL OR s.episode =
                                  (SELECT a.outage_id FROM alert a
                                   WHERE a.id = alert_delivery.alert_id)))"
        ))?
        .execute(rusqlite::named_params! {
            ":delivery": delivery_id,
            ":silence": silence_id,
            ":now": now.as_millis(),
        })?;
    Ok(changed == 1)
}

/// Marks pending resolve `delivery_id` held back at `now` if the receiver
/// cannot have heard of its alert: no send of a fire or repeat for it was
/// ever attempted, and the latest one was held back by a silence. It is
/// held under that silence, whether or not the silence still applies;
/// anything else (a send attempted, a fire that is not silenced, a row
/// pruned) sends it. The silence it was held under, or `None` if it goes.
pub(crate) fn hold_resolve(
    conn: &Connection,
    delivery_id: i64,
    now: Timestamp,
) -> Result<Option<i64>> {
    const LATEST_FIRE: &str = "FROM alert_delivery f
        WHERE f.alert_id = alert_delivery.alert_id AND f.kind IN ('fire', 'repeat')
          AND f.id < alert_delivery.id
        ORDER BY f.id DESC LIMIT 1";
    Ok(conn
        .prepare_cached(&format!(
            "UPDATE alert_delivery SET status = 'silenced', finished_at = :now,
                 silence_id = (SELECT f.silence_id {LATEST_FIRE})
             WHERE id = :delivery AND status = 'pending' AND kind = 'resolve'
               AND NOT (SELECT a.attempted FROM alert a WHERE a.id = alert_delivery.alert_id)
               AND (SELECT f.status {LATEST_FIRE}) = 'silenced'
             RETURNING silence_id"
        ))?
        .query_row(
            rusqlite::named_params! { ":delivery": delivery_id, ":now": now.as_millis() },
            |row| row.get(0),
        )
        .optional()?)
}
