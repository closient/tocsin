// SPDX-License-Identifier: AGPL-3.0-only

//! Reading and writing incidents. Writes run on the writer thread, each in
//! one IMMEDIATE transaction; reads assemble an [`Incident`] from its row,
//! its components and its updates.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use tocsin_core::incident::{
    Actor, ActorKind, Impact, Incident, IncidentEdit, IncidentKind, IncidentStatus, IncidentUpdate,
    NewIncident, NewUpdate, Timestamp,
};

use crate::error::{Error, Result};

const INCIDENT_COLUMNS: &str = "id, kind, title, impact, status, scheduled_start, scheduled_end, \
     resolved_at, created_at, created_by_kind, created_by, updated_at, updated_by_kind, updated_by";

/// Inserts a new incident with its components and first update; returns
/// its id.
pub(crate) fn insert(
    transaction: &Transaction<'_>,
    new: &NewIncident,
    actor: &Actor,
    at: Timestamp,
) -> Result<i64> {
    let status = new.first_status();
    transaction.execute(
        "INSERT INTO incident (kind, title, impact, status, scheduled_start, scheduled_end, \
         resolved_at, created_at, created_by_kind, created_by, updated_at, updated_by_kind, \
         updated_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?8, ?9, ?10)",
        params![
            new.kind.as_str(),
            new.title,
            new.impact.as_str(),
            status.as_str(),
            new.scheduled_start.map(Timestamp::as_millis),
            new.scheduled_end.map(Timestamp::as_millis),
            status.is_terminal().then_some(at.as_millis()),
            at.as_millis(),
            actor.kind.as_str(),
            actor.id,
        ],
    )?;
    let id = transaction.last_insert_rowid();
    replace_components(transaction, id, &new.components)?;
    append_update(transaction, id, status, &new.message, actor, at)?;
    Ok(id)
}

/// Appends `update` and moves the incident to its status. `None` if no
/// incident has that id.
pub(crate) fn add_update(
    transaction: &Transaction<'_>,
    id: i64,
    update: &NewUpdate,
    actor: &Actor,
    at: Timestamp,
) -> Result<Option<()>> {
    let Some(incident) = load(transaction, id)? else {
        return Ok(None);
    };
    incident.check_update(update).map_err(Error::Invalid)?;
    append_update(transaction, id, update.status, &update.message, actor, at)?;
    // `resolved_at` keeps the first resolution while the incident stays
    // over, and is cleared if an update reopens it.
    let resolved_at = if update.status.is_terminal() {
        Some(
            incident
                .resolved_at
                .map_or(at.as_millis(), Timestamp::as_millis),
        )
    } else {
        None
    };
    transaction.execute(
        "UPDATE incident SET status = ?2, resolved_at = ?3, updated_at = ?4, \
         updated_by_kind = ?5, updated_by = ?6 WHERE id = ?1",
        params![
            id,
            update.status.as_str(),
            resolved_at,
            at.as_millis(),
            actor.kind.as_str(),
            actor.id,
        ],
    )?;
    Ok(Some(()))
}

/// Applies `edit` to the incident row and logs it. `None` if no incident
/// has that id.
pub(crate) fn edit(
    transaction: &Transaction<'_>,
    id: i64,
    edit: &IncidentEdit,
    actor: &Actor,
    at: Timestamp,
) -> Result<Option<()>> {
    let Some(mut incident) = load(transaction, id)? else {
        return Ok(None);
    };
    incident.apply_edit(edit).map_err(Error::Invalid)?;
    transaction.execute(
        "UPDATE incident SET title = ?2, impact = ?3, scheduled_start = ?4, scheduled_end = ?5, \
         updated_at = ?6, updated_by_kind = ?7, updated_by = ?8 WHERE id = ?1",
        params![
            id,
            incident.title,
            incident.impact.as_str(),
            incident.scheduled_start.map(Timestamp::as_millis),
            incident.scheduled_end.map(Timestamp::as_millis),
            at.as_millis(),
            actor.kind.as_str(),
            actor.id,
        ],
    )?;
    if edit.components.is_some() {
        replace_components(transaction, id, &incident.components)?;
    }
    let changes = serde_json::to_string(edit).map_err(Error::Encode)?;
    transaction.execute(
        "INSERT INTO incident_edit (incident_id, changes, created_at, created_by_kind, \
         created_by) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, changes, at.as_millis(), actor.kind.as_str(), actor.id],
    )?;
    Ok(Some(()))
}

fn replace_components(transaction: &Transaction<'_>, id: i64, components: &[String]) -> Result<()> {
    transaction.execute(
        "DELETE FROM incident_component WHERE incident_id = ?1",
        [id],
    )?;
    let mut insert = transaction.prepare_cached(
        "INSERT INTO incident_component (incident_id, component_id) VALUES (?1, ?2)",
    )?;
    for component in components {
        insert.execute(params![id, component])?;
    }
    Ok(())
}

fn append_update(
    transaction: &Transaction<'_>,
    id: i64,
    status: IncidentStatus,
    message: &str,
    actor: &Actor,
    at: Timestamp,
) -> Result<()> {
    transaction.execute(
        "INSERT INTO incident_update (incident_id, status, message, created_at, \
         created_by_kind, created_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            id,
            status.as_str(),
            message,
            at.as_millis(),
            actor.kind.as_str(),
            actor.id
        ],
    )?;
    Ok(())
}

/// The incident with that id, with its components and updates.
pub(crate) fn load(conn: &Connection, id: i64) -> Result<Option<Incident>> {
    snapshot(conn, |conn| {
        let incident = conn
            .prepare_cached(&format!(
                "SELECT {INCIDENT_COLUMNS} FROM incident WHERE id = ?1"
            ))?
            .query_row([id], incident_row)
            .optional()?;
        Ok(complete(conn, incident.into_iter().collect())?.pop())
    })
}

/// Up to `limit` incidents, newest first, with ids below `before` when it
/// is given.
pub(crate) fn list(conn: &Connection, before: Option<i64>, limit: u32) -> Result<Vec<Incident>> {
    snapshot(conn, |conn| {
        let incidents = conn
            .prepare_cached(&format!(
                "SELECT {INCIDENT_COLUMNS} FROM incident WHERE id < ?1 ORDER BY id DESC LIMIT ?2"
            ))?
            .query_map(params![before.unwrap_or(i64::MAX), limit], incident_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        complete(conn, incidents)
    })
}

/// An incident not yet resolved or a maintenance not yet completed, with
/// only what deriving component status needs: no title, no updates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenIncident {
    /// Its id.
    pub id: i64,
    /// Incident or maintenance.
    pub kind: IncidentKind,
    /// Its declared impact.
    pub impact: Impact,
    /// The components it names, sorted.
    pub components: Vec<String>,
    /// A maintenance window's scheduled start.
    pub scheduled_start: Option<Timestamp>,
    /// A maintenance window's scheduled end.
    pub scheduled_end: Option<Timestamp>,
}

/// Every incident not yet resolved and every maintenance not yet
/// completed, oldest first, in one query.
pub(crate) fn open(conn: &Connection) -> Result<Vec<OpenIncident>> {
    let mut statement = conn.prepare_cached(
        "SELECT incident.id, kind, impact, scheduled_start, scheduled_end, component_id
         FROM incident
         LEFT JOIN incident_component ON incident_component.incident_id = incident.id
         WHERE resolved_at IS NULL
         ORDER BY incident.id, component_id",
    )?;
    let mut rows = statement.query([])?;
    let mut open: Vec<OpenIncident> = Vec::new();
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        if open.last().is_none_or(|incident| incident.id != id) {
            let instant = |index: usize| -> rusqlite::Result<Option<Timestamp>> {
                Ok(row
                    .get::<_, Option<i64>>(index)?
                    .map(Timestamp::from_millis))
            };
            open.push(OpenIncident {
                id,
                kind: text(row, 1)?,
                impact: text(row, 2)?,
                components: Vec::new(),
                scheduled_start: instant(3)?,
                scheduled_end: instant(4)?,
            });
        }
        if let (Some(incident), Some(component)) = (open.last_mut(), row.get(5)?) {
            incident.components.push(component);
        }
    }
    Ok(open)
}

/// Runs `read` against one snapshot of the database, so an incident's row,
/// components and updates all come from the same moment even while the
/// writer commits. A write already runs inside its own transaction, which
/// is snapshot enough.
pub(crate) fn snapshot<T>(
    conn: &Connection,
    read: impl FnOnce(&Connection) -> Result<T>,
) -> Result<T> {
    if !conn.is_autocommit() {
        return read(conn);
    }
    let transaction = conn.unchecked_transaction()?;
    let value = read(&transaction)?;
    transaction.commit()?;
    Ok(value)
}

/// Fills in the components and updates of `incidents`, three queries
/// however many there are.
fn complete(conn: &Connection, mut incidents: Vec<Incident>) -> Result<Vec<Incident>> {
    if incidents.is_empty() {
        return Ok(incidents);
    }
    let ids = serde_json::Value::from(
        incidents
            .iter()
            .map(|incident| incident.id)
            .collect::<Vec<_>>(),
    )
    .to_string();
    let mut components: HashMap<i64, Vec<String>> = HashMap::new();
    let rows = conn
        .prepare_cached(
            "SELECT incident_id, component_id FROM incident_component \
             WHERE incident_id IN (SELECT value FROM json_each(?1)) \
             ORDER BY incident_id, component_id",
        )?
        .query_map([&ids], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<Vec<(i64, String)>>>()?;
    for (id, component) in rows {
        components.entry(id).or_default().push(component);
    }
    let mut updates: HashMap<i64, Vec<IncidentUpdate>> = HashMap::new();
    let rows = conn
        .prepare_cached(
            "SELECT incident_id, id, status, message, created_at, created_by_kind, created_by \
             FROM incident_update WHERE incident_id IN (SELECT value FROM json_each(?1)) \
             ORDER BY incident_id, id",
        )?
        .query_map([&ids], |row| {
            Ok((
                row.get(0)?,
                IncidentUpdate {
                    id: row.get(1)?,
                    status: text(row, 2)?,
                    message: row.get(3)?,
                    created_at: Timestamp::from_millis(row.get(4)?),
                    created_by: actor(row, 5)?,
                },
            ))
        })?
        .collect::<rusqlite::Result<Vec<(i64, IncidentUpdate)>>>()?;
    for (id, update) in rows {
        updates.entry(id).or_default().push(update);
    }
    for incident in &mut incidents {
        incident.components = components.remove(&incident.id).unwrap_or_default();
        incident.updates = updates.remove(&incident.id).unwrap_or_default();
    }
    Ok(incidents)
}

fn incident_row(row: &Row<'_>) -> rusqlite::Result<Incident> {
    let instant = |index: usize| -> rusqlite::Result<Option<Timestamp>> {
        Ok(row
            .get::<_, Option<i64>>(index)?
            .map(Timestamp::from_millis))
    };
    Ok(Incident {
        id: row.get(0)?,
        kind: text::<IncidentKind>(row, 1)?,
        title: row.get(2)?,
        impact: text::<Impact>(row, 3)?,
        status: text(row, 4)?,
        components: Vec::new(),
        scheduled_start: instant(5)?,
        scheduled_end: instant(6)?,
        resolved_at: instant(7)?,
        created_at: Timestamp::from_millis(row.get(8)?),
        created_by: actor(row, 9)?,
        updated_at: Timestamp::from_millis(row.get(11)?),
        updated_by: actor(row, 12)?,
        updates: Vec::new(),
    })
}

/// The `*_by_kind` column at `index` and the id after it.
pub(crate) fn actor(row: &Row<'_>, index: usize) -> rusqlite::Result<Actor> {
    Ok(Actor {
        kind: text::<ActorKind>(row, index)?,
        id: row.get(index + 1)?,
    })
}

/// A text column holding one of a fixed set of names.
pub(crate) fn text<T>(row: &Row<'_>, index: usize) -> rusqlite::Result<T>
where
    T: std::str::FromStr<Err = String>,
{
    let value: String = row.get(index)?;
    value.parse().map_err(|message: String| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            message.into(),
        )
    })
}
