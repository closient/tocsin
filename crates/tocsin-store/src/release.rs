// SPDX-License-Identifier: AGPL-3.0-only

//! Reading and writing the release log, and the timeline that interleaves
//! deploys with outages and incidents.
//!
//! Writes run on the writer thread, each in one IMMEDIATE transaction and
//! one upsert keyed by (project, version, environment). Every read that
//! returns releases goes through [`VISIBLE`], the one place the store
//! narrows releases to the projects a reader may see.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use tocsin_core::incident::{Actor, FieldError, Timestamp};
use tocsin_core::release::{DeployStamp, NewRelease, Release, Timeline, TimelineEvent};

use crate::error::{Error, Result};
use crate::incident::{actor, snapshot, text};
use crate::rollup::parse_column;

const RELEASE_COLUMNS: &str = "id, project_id, version, environment, title, notes, url, \
     released_at, deployed_at, created_at, created_by_kind, created_by, updated_at, \
     updated_by_kind, updated_by";

/// The filter every release read applies: `project_id` is in the JSON
/// array bound as `:visible`, or `:visible` is NULL for every project.
const VISIBLE: &str = "(:visible IS NULL OR project_id IN (SELECT value FROM json_each(:visible)))";

/// Which projects' releases a read returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Projects {
    /// Every project's, including ones no longer configured.
    All,
    /// Only these projects'.
    Only(Vec<String>),
}

impl Projects {
    /// The `:visible` parameter of [`VISIBLE`].
    fn bind(&self) -> Option<String> {
        match self {
            Projects::All => None,
            Projects::Only(ids) => Some(serde_json::Value::from(ids.clone()).to_string()),
        }
    }
}

/// What [`list`] returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseQuery {
    /// The projects the reader may see.
    pub visible: Projects,
    /// Only this project's releases.
    pub project: Option<String>,
    /// Only this version's releases, one per environment.
    pub version: Option<String>,
    /// Only releases published at or after this.
    pub since: Option<Timestamp>,
    /// At most this many.
    pub limit: u32,
}

/// What [`timeline`] returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineQuery {
    /// The projects whose deploys and releases the reader may see.
    pub visible: Projects,
    /// Events at or after this.
    pub since: Timestamp,
    /// At most this many events.
    pub limit: u32,
    /// Only events touching one component.
    pub component: Option<ComponentScope>,
}

/// What touching a component means, resolved from the configuration by
/// the caller: the store holds history, not configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentScope {
    /// The `[[component]]` id: outages recorded against it, incidents
    /// naming it.
    pub id: String,
    /// Its checks now: outages of these count even if recorded before the
    /// check joined the component.
    pub checks: Vec<String>,
    /// The `[[project]]`s listing it: their deploys and releases count.
    pub projects: Vec<String>,
}

/// Whether a write created the release or updated it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Written {
    /// A new row.
    Created,
    /// An existing row, changed in place.
    Updated,
}

/// Publishes `new`, or updates the release with its key. Title, notes and
/// link are replaced; `released_at` keeps its recorded value unless `new`
/// gives a later one (an earlier one is refused), or is set to `at` if the
/// row was only a deploy stamp; `deployed_at` is replaced only if `new`
/// gives one.
pub(crate) fn publish(
    transaction: &Transaction<'_>,
    new: &NewRelease,
    actor: &Actor,
    at: Timestamp,
) -> Result<(i64, Written)> {
    let environment = new.environment();
    let recorded: Option<Option<i64>> = transaction
        .prepare_cached(
            "SELECT released_at FROM release \
             WHERE project_id = ?1 AND version = ?2 AND environment = ?3",
        )?
        .query_row(params![new.project, new.version, environment], |row| {
            row.get(0)
        })
        .optional()?;
    if let (Some(Some(recorded)), Some(asked)) = (recorded, new.released_at)
        && asked.as_millis() < recorded
    {
        return Err(Error::Invalid(vec![FieldError::new(
            "released_at",
            format!(
                "is before the recorded {}; a release is never moved earlier",
                Timestamp::from_millis(recorded)
            ),
        )]));
    }
    let id = transaction
        .prepare_cached(
            "INSERT INTO release (project_id, version, environment, title, notes, url, \
             released_at, deployed_at, created_at, created_by_kind, created_by, updated_at, \
             updated_by_kind, updated_by) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, coalesce(?7, ?9), ?8, ?9, ?10, ?11, ?9, ?10, ?11) \
             ON CONFLICT (project_id, version, environment) DO UPDATE SET \
                 title = excluded.title, notes = excluded.notes, url = excluded.url, \
                 released_at = coalesce(?7, release.released_at, ?9), \
                 deployed_at = coalesce(?8, release.deployed_at), \
                 updated_at = ?9, updated_by_kind = ?10, updated_by = ?11 \
             RETURNING id",
        )?
        .query_row(
            params![
                new.project,
                new.version,
                environment,
                new.title(),
                new.notes,
                new.url,
                new.released_at.map(Timestamp::as_millis),
                new.deployed_at.map(Timestamp::as_millis),
                at.as_millis(),
                actor.kind.as_str(),
                actor.id,
            ],
            |row| row.get(0),
        )?;
    let written = if recorded.is_some() {
        Written::Updated
    } else {
        Written::Created
    };
    Ok((id, written))
}

/// Records that `version` of `project` was deployed. With no release yet,
/// creates a stub (empty title and notes, no `released_at`) that a later
/// publish fills in; otherwise replaces its `deployed_at`.
pub(crate) fn stamp(
    transaction: &Transaction<'_>,
    project: &str,
    version: &str,
    stamp: &DeployStamp,
    actor: &Actor,
    at: Timestamp,
) -> Result<(i64, Written)> {
    let environment = stamp.environment();
    let exists = transaction
        .prepare_cached(
            "SELECT 1 FROM release WHERE project_id = ?1 AND version = ?2 AND environment = ?3",
        )?
        .exists(params![project, version, environment])?;
    let id = transaction
        .prepare_cached(
            "INSERT INTO release (project_id, version, environment, title, notes, url, \
             released_at, deployed_at, created_at, created_by_kind, created_by, updated_at, \
             updated_by_kind, updated_by) \
             VALUES (?1, ?2, ?3, '', '', NULL, NULL, coalesce(?4, ?5), ?5, ?6, ?7, ?5, ?6, ?7) \
             ON CONFLICT (project_id, version, environment) DO UPDATE SET \
                 deployed_at = excluded.deployed_at, \
                 updated_at = ?5, updated_by_kind = ?6, updated_by = ?7 \
             RETURNING id",
        )?
        .query_row(
            params![
                project,
                version,
                environment,
                stamp.deployed_at.map(Timestamp::as_millis),
                at.as_millis(),
                actor.kind.as_str(),
                actor.id,
            ],
            |row| row.get(0),
        )?;
    let written = if exists {
        Written::Updated
    } else {
        Written::Created
    };
    Ok((id, written))
}

/// The release with that id, whatever its project.
pub(crate) fn load(conn: &Connection, id: i64) -> Result<Option<Release>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {RELEASE_COLUMNS} FROM release WHERE id = ?1"
        ))?
        .query_row([id], release_row)
        .optional()?)
}

/// Published releases matching `query`, newest first, and whether more
/// matched than its limit. A deploy stamp awaiting its publish is not a
/// release yet and is left out; it shows on the timeline as a deploy.
pub(crate) fn list(conn: &Connection, query: &ReleaseQuery) -> Result<(Vec<Release>, bool)> {
    let mut releases = conn
        .prepare_cached(&format!(
            "SELECT {RELEASE_COLUMNS} FROM release \
             WHERE released_at IS NOT NULL AND {VISIBLE} \
             AND (:project IS NULL OR project_id = :project) \
             AND (:version IS NULL OR version = :version) \
             AND released_at >= :since \
             ORDER BY released_at DESC, id DESC LIMIT :limit"
        ))?
        .query_map(
            rusqlite::named_params! {
                ":visible": query.visible.bind(),
                ":project": query.project,
                ":version": query.version,
                ":since": query.since.map_or(0, Timestamp::as_millis),
                ":limit": i64::from(query.limit) + 1,
            },
            release_row,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let truncated = releases.len() > query.limit as usize;
    releases.truncate(query.limit as usize);
    Ok((releases, truncated))
}

/// Outages, incidents, deploys and releases from `query.since` on, oldest
/// first, ties broken by kind and then id; at most `query.limit` of them.
///
/// Each kind is read in order with one more row than the limit, so the
/// first `limit` of the merge are exact, and every read comes from the
/// same snapshot.
pub(crate) fn timeline(conn: &Connection, query: &TimelineQuery) -> Result<Timeline> {
    snapshot(conn, |conn| {
        let limit = i64::from(query.limit) + 1;
        let since = query.since.as_millis();
        let scope = query.component.as_ref();
        let component = scope.map(|scope| scope.id.as_str());
        let checks = scope.map(|scope| serde_json::Value::from(scope.checks.clone()).to_string());
        let projects =
            scope.map(|scope| serde_json::Value::from(scope.projects.clone()).to_string());

        let mut events = conn
            .prepare_cached(
                "SELECT id, node_id, check_id, component_id, health, started_at, ended_at \
                 FROM outage WHERE started_at >= :since \
                 AND (:component IS NULL OR component_id = :component \
                      OR check_id IN (SELECT value FROM json_each(:checks))) \
                 ORDER BY started_at, id LIMIT :limit",
            )?
            .query_map(
                rusqlite::named_params! {
                    ":since": since,
                    ":component": component,
                    ":checks": checks,
                    ":limit": limit,
                },
                |row| {
                    Ok(TimelineEvent::Outage {
                        at: Timestamp::from_millis(row.get(5)?),
                        id: row.get(0)?,
                        node_id: row.get(1)?,
                        check_id: row.get(2)?,
                        component_id: row.get(3)?,
                        health: parse_column(4, &row.get::<_, String>(4)?)?,
                        ended_at: row.get::<_, Option<i64>>(6)?.map(Timestamp::from_millis),
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let incidents = conn
            .prepare_cached(
                "SELECT id, created_at, kind, title, impact, status FROM incident \
                 WHERE created_at >= :since \
                 AND (:component IS NULL OR EXISTS (SELECT 1 FROM incident_component \
                      WHERE incident_id = incident.id AND component_id = :component)) \
                 ORDER BY created_at, id LIMIT :limit",
            )?
            .query_map(
                rusqlite::named_params! {
                    ":since": since,
                    ":component": component,
                    ":limit": limit,
                },
                |row| {
                    Ok(TimelineEvent::Incident {
                        id: row.get(0)?,
                        at: Timestamp::from_millis(row.get(1)?),
                        incident_kind: text(row, 2)?,
                        title: row.get(3)?,
                        impact: text(row, 4)?,
                        status: text(row, 5)?,
                        components: Vec::new(),
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        events.extend(with_components(conn, incidents)?);

        let releases = conn
            .prepare_cached(&format!(
                "SELECT {RELEASE_COLUMNS} FROM release \
                 WHERE coalesce(deployed_at, released_at) >= :since AND {VISIBLE} \
                 AND (:projects IS NULL OR project_id IN (SELECT value FROM json_each(:projects))) \
                 ORDER BY coalesce(deployed_at, released_at), id LIMIT :limit"
            ))?
            .query_map(
                rusqlite::named_params! {
                    ":since": since,
                    ":visible": query.visible.bind(),
                    ":projects": projects,
                    ":limit": limit,
                },
                release_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        events.extend(releases.into_iter().filter_map(release_event));

        events.sort_by_key(TimelineEvent::sort_key);
        let truncated = events.len() > query.limit as usize;
        events.truncate(query.limit as usize);
        Ok(Timeline {
            since: query.since,
            events,
            truncated,
        })
    })
}

/// A release on the timeline: a deploy at `deployed_at` once it has one,
/// else a release at `released_at`.
fn release_event(release: Release) -> Option<TimelineEvent> {
    let id = release.id;
    match (release.deployed_at, release.released_at) {
        (Some(at), _) => Some(TimelineEvent::Deploy { at, id, release }),
        (None, Some(at)) => Some(TimelineEvent::Release { at, id, release }),
        // The schema requires one of the two.
        (None, None) => None,
    }
}

/// Fills in each incident event's components, one query however many.
fn with_components(
    conn: &Connection,
    mut incidents: Vec<TimelineEvent>,
) -> Result<Vec<TimelineEvent>> {
    if incidents.is_empty() {
        return Ok(incidents);
    }
    let ids = serde_json::Value::from(incidents.iter().map(TimelineEvent::id).collect::<Vec<_>>())
        .to_string();
    let mut found: HashMap<i64, Vec<String>> = HashMap::new();
    let rows = conn
        .prepare_cached(
            "SELECT incident_id, component_id FROM incident_component \
             WHERE incident_id IN (SELECT value FROM json_each(?1)) \
             ORDER BY incident_id, component_id",
        )?
        .query_map([&ids], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<Vec<(i64, String)>>>()?;
    for (id, component) in rows {
        found.entry(id).or_default().push(component);
    }
    for event in &mut incidents {
        if let TimelineEvent::Incident { id, components, .. } = event {
            *components = found.remove(id).unwrap_or_default();
        }
    }
    Ok(incidents)
}

fn release_row(row: &Row<'_>) -> rusqlite::Result<Release> {
    let instant = |index: usize| -> rusqlite::Result<Option<Timestamp>> {
        Ok(row
            .get::<_, Option<i64>>(index)?
            .map(Timestamp::from_millis))
    };
    Ok(Release {
        id: row.get(0)?,
        project: row.get(1)?,
        version: row.get(2)?,
        environment: row.get(3)?,
        title: row.get(4)?,
        notes: row.get(5)?,
        url: row.get(6)?,
        released_at: instant(7)?,
        deployed_at: instant(8)?,
        created_at: Timestamp::from_millis(row.get(9)?),
        created_by: actor(row, 10)?,
        updated_at: Timestamp::from_millis(row.get(12)?),
        updated_by: actor(row, 13)?,
    })
}
