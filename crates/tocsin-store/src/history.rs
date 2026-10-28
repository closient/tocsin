// SPDX-License-Identifier: AGPL-3.0-only

//! Reading history back for the API: one check's raw results and the
//! outages in a window, a page at a time, newest first.

use rusqlite::{Connection, OptionalExtension, named_params};
use tocsin_core::incident::{FieldError, Timestamp};
use tocsin_core::status::{CheckKind, CheckResult, OutageEnding, OutageRecord};

use crate::alert::SubjectKind;
use crate::error::{Error, Result};
use crate::incident::text;
use crate::rollup::parse_column;

/// Which checks' and heartbeats' outages a read returns. The caller
/// resolves it from the configuration: the store holds history, not
/// which checks are public.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checks {
    /// Every check's and heartbeat's, including ones no longer configured.
    All,
    /// What the public sees. A configured check's outages follow where it
    /// is now: shown if it is in `public`, hidden if it is in `internal`,
    /// whatever component they recorded. One no longer configured has
    /// only its outages to go by, so each is shown if it recorded a
    /// component not in `internal_components`: a check removed from the
    /// status page keeps its public history, and one removed while on no
    /// component or a non-public one stays hidden.
    Public {
        /// The configured checks and heartbeats on a public component.
        public: Vec<String>,
        /// The configured checks and heartbeats on none, or on a
        /// non-public one.
        internal: Vec<String>,
        /// The configured components that are not public.
        internal_components: Vec<String>,
        /// The configured quorum checks, whose outages are shown only
        /// where the quorum recorded them, under
        /// [`QUORUM_NODE`](tocsin_core::config::QUORUM_NODE): one node's
        /// view of them is internal.
        quorum: Vec<String>,
    },
}

impl Checks {
    /// The `:shown` parameter of [`SHOWN`].
    pub(crate) fn bind(&self) -> Option<String> {
        match self {
            Checks::All => None,
            Checks::Public {
                public,
                internal,
                internal_components,
                quorum,
            } => Some(
                serde_json::json!({
                    "public": public,
                    "internal": internal,
                    "internal_components": internal_components,
                    "quorum": quorum,
                })
                .to_string(),
            ),
        }
    }
}

/// The clause that narrows outages to the checks a reader may see, bound
/// by [`Checks::bind`]. [`OUTAGES`] spells it out in each arm, and the
/// timeline's outage read uses it too. `'*'` is
/// [`QUORUM_NODE`](tocsin_core::config::QUORUM_NODE).
pub(crate) const SHOWN: &str = "(:shown IS NULL \
     OR (check_id NOT IN (SELECT value FROM json_each(:shown, '$.internal')) \
         AND (node_id = '*' \
              OR check_id NOT IN (SELECT value FROM json_each(:shown, '$.quorum'))) \
         AND ((component_id IS NOT NULL \
               AND component_id NOT IN \
                   (SELECT value FROM json_each(:shown, '$.internal_components'))) \
              OR check_id IN (SELECT value FROM json_each(:shown, '$.public')))))";

/// Which page of a history read: newest first from `before`, back to
/// `since`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryPage {
    /// The start of the window.
    pub since: Timestamp,
    /// Only rows after this one in newest-first order: the previous page's
    /// last id. An id that names no row of this read (purged since, or
    /// from another check's page) is [`Error::Invalid`], so a lost cursor
    /// never reads as the last page.
    pub before: Option<i64>,
    /// At most this many rows.
    pub limit: u32,
}

/// A page resumes after the row at `(:at, :before)` in newest-first
/// order. `started_at <= :at` is the index bound that makes it a seek; the
/// rest breaks ties by id. Without a cursor both are `i64::MAX`, past
/// every row.
const RESULTS: &str =
    "SELECT id, node_id, started_at, duration_ms, health, error, status_code, detail
     FROM check_result
     WHERE check_id = :check AND started_at >= :since
       AND started_at <= :at AND (started_at < :at OR id < :before)
     ORDER BY started_at DESC, id DESC LIMIT :limit";

/// Where a results cursor points, within the same check only.
const RESULT_CURSOR: &str =
    "SELECT started_at FROM check_result WHERE id = :before AND check_id = :check";

/// `check_id`'s results that started at or after `page.since`, newest
/// first, ties by id. Rides `check_result_by_check` for a check and a
/// heartbeat alike: the two share one id space.
///
/// # Errors
///
/// [`Error::Invalid`] when `page.before` names no result of `check_id`.
pub(crate) fn results(
    conn: &Connection,
    check_id: &str,
    page: HistoryPage,
) -> Result<Vec<CheckResult>> {
    let (at, before) = match page.before {
        None => (i64::MAX, i64::MAX),
        Some(before) => {
            let at = conn
                .prepare_cached(RESULT_CURSOR)?
                .query_row(
                    named_params! { ":before": before, ":check": check_id },
                    |row| row.get(0),
                )
                .optional()?
                .ok_or_else(|| lost_cursor(before, "result of this check"))?;
            (at, before)
        }
    };
    let mut statement = conn.prepare_cached(RESULTS)?;
    let rows = statement.query_map(
        named_params! {
            ":check": check_id,
            ":since": page.since.as_millis(),
            ":at": at,
            ":before": before,
            ":limit": page.limit,
        },
        result_row,
    )?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// A row of [`RESULTS`] or [`NEWEST`] as a result.
fn result_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CheckResult> {
    let detail = row
        .get::<_, Option<String>>(7)?
        .map(|text| {
            serde_json::from_str(&text).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    7,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })
        })
        .transpose()?;
    Ok(CheckResult {
        id: row.get(0)?,
        node_id: row.get(1)?,
        started_at: Timestamp::from_millis(row.get(2)?),
        duration_ms: row.get(3)?,
        health: parse_column(4, &row.get::<_, String>(4)?)?,
        error: row.get(5)?,
        status_code: row.get(6)?,
        detail,
    })
}

/// Which of a check's results [`newest`] looks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResultFilter {
    /// Any result.
    Any,
    /// A result that was degraded or down.
    NotUp,
    /// A result whose error, if it has one, does not begin with this:
    /// a heartbeat's latest ping rather than a missed deadline.
    ErrorNotStartingWith(String),
}

/// One check's newest result of a kind, from `since` on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Newest {
    /// The check or heartbeat.
    pub check_id: String,
    /// The start of the window searched.
    pub since: Timestamp,
    /// Which results count.
    pub filter: ResultFilter,
}

/// The newest result of `:check` from `:since` that the filter keeps.
/// Rides `check_result_by_check` backwards and stops at the first match.
const NEWEST: &str =
    "SELECT id, node_id, started_at, duration_ms, health, error, status_code, detail
     FROM check_result
     WHERE check_id = :check AND started_at >= :since
       AND (:not_up = 0 OR health != 'up')
       AND (:prefix IS NULL OR error IS NULL OR substr(error, 1, length(:prefix)) != :prefix)
     ORDER BY started_at DESC, id DESC LIMIT 1";

/// For each of `queries`, in order, the newest result it asks for, if any.
pub(crate) fn newest(conn: &Connection, queries: &[Newest]) -> Result<Vec<Option<CheckResult>>> {
    let mut statement = conn.prepare_cached(NEWEST)?;
    let mut found = Vec::with_capacity(queries.len());
    for query in queries {
        let (not_up, prefix) = match &query.filter {
            ResultFilter::Any => (false, None),
            ResultFilter::NotUp => (true, None),
            ResultFilter::ErrorNotStartingWith(prefix) => (false, Some(prefix.as_str())),
        };
        found.push(
            statement
                .query_row(
                    named_params! {
                        ":check": query.check_id,
                        ":since": query.since.as_millis(),
                        ":not_up": not_up,
                        ":prefix": prefix,
                    },
                    result_row,
                )
                .optional()?,
        );
    }
    Ok(found)
}

/// One check's results in one slice of a window: how many, how many
/// failed, and the latency of those that were not down.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultSlice {
    /// The check or heartbeat.
    pub check_id: String,
    /// Which slice, counting from 0 at the start of the window.
    pub slice: i64,
    /// Results in the slice.
    pub results: u32,
    /// Of those, results that were degraded.
    pub degraded: u32,
    /// Of those, results that were down.
    pub down: u32,
    /// The mean duration of the results that were not down, in
    /// milliseconds; `None` when every one was down.
    pub mean_ms: Option<f64>,
    /// The shortest of them.
    pub min_ms: Option<i64>,
    /// The longest of them.
    pub max_ms: Option<i64>,
}

/// What one slice of results sums up. Down results are left out of the
/// latency: a timeout's duration is the timeout, and a refused
/// connection's says nothing about the service.
macro_rules! slice_columns {
    () => {
        "SELECT check_id, (started_at - :since) / :width AS slice, COUNT(*),
         SUM(health = 'degraded'), SUM(health = 'down'),
         AVG(CASE WHEN health != 'down' THEN duration_ms END),
         MIN(CASE WHEN health != 'down' THEN duration_ms END),
         MAX(CASE WHEN health != 'down' THEN duration_ms END)"
    };
}

/// Every check's results from `:since` on, grouped into slices `:width`
/// milliseconds wide. Pinned to `check_result_by_time`: left to itself
/// the planner walks all of `check_result_by_check` to save the sort,
/// which reads every retained result rather than the window's.
const SLICES: &str = concat!(
    slice_columns!(),
    "
     FROM check_result INDEXED BY check_result_by_time WHERE started_at >= :since
     GROUP BY check_id, slice ORDER BY check_id, slice"
);

/// One check's results from `:since` on, in the same slices. Rides
/// `check_result_by_check`.
const CHECK_SLICES: &str = concat!(
    slice_columns!(),
    "
     FROM check_result WHERE check_id = :check AND started_at >= :since
     GROUP BY slice ORDER BY slice"
);

/// The results since `since` of `check`, or of every check, in slices
/// `width_ms` wide, ordered by check and slice. A slice with no results
/// is absent.
pub(crate) fn slices(
    conn: &Connection,
    since: Timestamp,
    width_ms: i64,
    check: Option<&str>,
) -> Result<Vec<ResultSlice>> {
    let row = |row: &rusqlite::Row<'_>| {
        Ok(ResultSlice {
            check_id: row.get(0)?,
            slice: row.get(1)?,
            results: row.get(2)?,
            degraded: row.get(3)?,
            down: row.get(4)?,
            mean_ms: row.get(5)?,
            min_ms: row.get(6)?,
            max_ms: row.get(7)?,
        })
    };
    let (since, width) = (since.as_millis(), width_ms.max(1));
    let rows = match check {
        Some(check) => conn
            .prepare_cached(CHECK_SLICES)?
            .query_map(
                named_params! { ":since": since, ":width": width, ":check": check },
                row,
            )?
            .collect::<rusqlite::Result<_>>()?,
        None => conn
            .prepare_cached(SLICES)?
            .query_map(named_params! { ":since": since, ":width": width }, row)?
            .collect::<rusqlite::Result<_>>()?,
    };
    Ok(rows)
}

/// The error for a `before` that names no row this read can resume from.
pub(crate) fn lost_cursor(before: i64, what: &str) -> Error {
    Error::Invalid(vec![FieldError::new(
        "before",
        format!(
            "{before} is no {what}: it was purged since, or came from another read; \
             start again without before"
        ),
    )])
}

/// Column `index`, an outage's `subject_kind`, as the API spells it.
pub(crate) fn subject_kind(index: usize, row: &rusqlite::Row<'_>) -> rusqlite::Result<CheckKind> {
    Ok(parse_column::<SubjectKind>(index, &row.get::<_, String>(index)?)?.into())
}

/// Column `index`, how an outage ended; `None` while it is open.
pub(crate) fn ending(
    index: usize,
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Option<OutageEnding>> {
    match row.get_ref(index)? {
        rusqlite::types::ValueRef::Null => Ok(None),
        _ => text(row, index).map(Some),
    }
}

/// Outages open at any moment at or after `page.since`, in three arms
/// that each ride an index: started since (`outage_by_start`, in order, so
/// a page reads only its rows); started before and still open
/// (`outage_open`); started before and ended since (`outage_ended`). The
/// last two are what a window catches mid-way, few enough to sort. One
/// arm with `ended_at IS NULL OR ended_at >= :since` would walk every
/// older outage through `outage_by_start` instead, hence `INDEXED BY`.
/// The cursor bound is the same as [`RESULTS`]'s, and [`SHOWN`] narrows
/// every arm to the checks the reader may see.
const OUTAGES: &str =
    "SELECT id, node_id, check_id, component_id, health, started_at, ended_at, subject_kind,
         ending
     FROM outage INDEXED BY outage_by_start
     WHERE started_at >= :since
       AND started_at <= :at AND (started_at < :at OR id < :before)
       AND (:shown IS NULL
            OR (check_id NOT IN (SELECT value FROM json_each(:shown, '$.internal'))
                AND (node_id = '*'
                     OR check_id NOT IN (SELECT value FROM json_each(:shown, '$.quorum')))
                AND ((component_id IS NOT NULL
                      AND component_id NOT IN
                          (SELECT value FROM json_each(:shown, '$.internal_components')))
                     OR check_id IN (SELECT value FROM json_each(:shown, '$.public')))))
     UNION ALL
     SELECT id, node_id, check_id, component_id, health, started_at, ended_at, subject_kind,
         ending
     FROM outage INDEXED BY outage_open
     WHERE ended_at IS NULL AND started_at < :since
       AND started_at <= :at AND (started_at < :at OR id < :before)
       AND (:shown IS NULL
            OR (check_id NOT IN (SELECT value FROM json_each(:shown, '$.internal'))
                AND (node_id = '*'
                     OR check_id NOT IN (SELECT value FROM json_each(:shown, '$.quorum')))
                AND ((component_id IS NOT NULL
                      AND component_id NOT IN
                          (SELECT value FROM json_each(:shown, '$.internal_components')))
                     OR check_id IN (SELECT value FROM json_each(:shown, '$.public')))))
     UNION ALL
     SELECT id, node_id, check_id, component_id, health, started_at, ended_at, subject_kind,
         ending
     FROM outage INDEXED BY outage_ended
     WHERE ended_at >= :since AND started_at < :since
       AND started_at <= :at AND (started_at < :at OR id < :before)
       AND (:shown IS NULL
            OR (check_id NOT IN (SELECT value FROM json_each(:shown, '$.internal'))
                AND (node_id = '*'
                     OR check_id NOT IN (SELECT value FROM json_each(:shown, '$.quorum')))
                AND ((component_id IS NOT NULL
                      AND component_id NOT IN
                          (SELECT value FROM json_each(:shown, '$.internal_components')))
                     OR check_id IN (SELECT value FROM json_each(:shown, '$.public')))))
     ORDER BY started_at DESC, id DESC LIMIT :limit";

/// Outages open at any moment at or after `page.since`: started since,
/// or started before and ended since or not at all. Newest start first,
/// ties by id.
///
/// # Errors
///
/// [`Error::Invalid`] when `page.before` names no outage `shown` holds,
/// so a cursor cannot tell a reader whether a hidden outage exists.
pub(crate) fn outages(
    conn: &Connection,
    page: HistoryPage,
    shown: &Checks,
) -> Result<Vec<OutageRecord>> {
    let shown = shown.bind();
    let (at, before) = match page.before {
        None => (i64::MAX, i64::MAX),
        Some(before) => {
            let at = conn
                .prepare_cached(&format!(
                    "SELECT started_at FROM outage WHERE id = :before AND {SHOWN}"
                ))?
                .query_row(
                    named_params! { ":before": before, ":shown": shown },
                    |row| row.get(0),
                )
                .optional()?
                .ok_or_else(|| lost_cursor(before, "outage"))?;
            (at, before)
        }
    };
    let mut statement = conn.prepare_cached(OUTAGES)?;
    let rows = statement.query_map(
        named_params! {
            ":since": page.since.as_millis(),
            ":at": at,
            ":before": before,
            ":limit": page.limit,
            ":shown": shown,
        },
        outage_record,
    )?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Outage `id`, if there is one, whatever its check.
pub(crate) fn outage(conn: &Connection, id: i64) -> Result<Option<OutageRecord>> {
    Ok(conn
        .prepare_cached(
            "SELECT id, node_id, check_id, component_id, health, started_at, ended_at,
                 subject_kind, ending
             FROM outage WHERE id = ?1",
        )?
        .query_row([id], outage_record)
        .optional()?)
}

/// An outage from the nine columns [`OUTAGES`] selects.
fn outage_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<OutageRecord> {
    Ok(OutageRecord {
        id: row.get(0)?,
        node_id: row.get(1)?,
        check_id: row.get(2)?,
        component_id: row.get(3)?,
        health: parse_column(4, &row.get::<_, String>(4)?)?,
        started_at: Timestamp::from_millis(row.get(5)?),
        ended_at: row.get::<_, Option<i64>>(6)?.map(Timestamp::from_millis),
        subject_kind: subject_kind(7, row)?,
        ending: ending(8, row)?,
    })
}

#[cfg(test)]
mod tests {
    use rusqlite::{Connection, named_params};

    use super::{OUTAGES, RESULTS, SHOWN};

    /// A migrated, empty database.
    async fn migrated() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(crate::DB_FILE);
        crate::Store::open(&path).unwrap().close().await.unwrap();
        let conn = Connection::open(&path).unwrap();
        (dir, conn)
    }

    /// `sql`'s query plan, a line per step.
    fn plan(conn: &Connection, sql: &str, params: &[(&str, &dyn rusqlite::ToSql)]) -> String {
        let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let plan: Vec<String> = statement
            .query_map(params, |row| row.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        plan.join("\n")
    }

    /// `OUTAGES` is a const, so it spells [`SHOWN`] out in each arm; an
    /// arm that drifted would show the public what the others hide.
    #[test]
    fn every_outage_arm_narrows_to_what_is_shown() {
        let words = |sql: &str| sql.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(words(OUTAGES).matches(&words(SHOWN)).count(), 3);
    }

    #[tokio::test]
    async fn a_results_page_seeks_the_index_in_order() {
        let (_dir, conn) = migrated().await;
        // A check and a heartbeat: one statement, one id space, one index.
        for check in ["api", "backup"] {
            let plan = plan(
                &conn,
                RESULTS,
                named_params! {
                    ":check": check, ":since": 0, ":at": 9, ":before": 7, ":limit": 101,
                },
            );
            assert!(
                plan.contains(
                    "USING INDEX check_result_by_check (check_id=? AND started_at>? AND started_at<?)"
                ),
                "the cursor bounds the index on both sides: {plan}"
            );
            assert!(
                !plan.contains("TEMP B-TREE"),
                "a page sorts no rows: {plan}"
            );
        }
    }

    #[tokio::test]
    async fn an_outages_page_reads_three_indexed_arms() {
        let (_dir, conn) = migrated().await;
        let plan = plan(
            &conn,
            OUTAGES,
            named_params! { ":since": 0, ":at": 9, ":before": 7, ":limit": 101 },
        );
        for arm in [
            "SEARCH outage USING INDEX outage_by_start (started_at>? AND started_at<?)",
            "SCAN outage USING INDEX outage_open",
            "SEARCH outage USING INDEX outage_ended (ended_at>?)",
        ] {
            assert!(plan.contains(arm), "{arm}: {plan}");
        }
        assert!(plan.contains("MERGE (UNION ALL)"), "{plan}");
        // Only the two mid-way arms sort; the started-since arm is read in
        // order, so a page stops after its rows.
        assert_eq!(plan.matches("TEMP B-TREE").count(), 2, "{plan}");
    }

    #[tokio::test]
    async fn open_incidents_read_their_own_index() {
        let (_dir, conn) = migrated().await;
        let plan = plan(
            &conn,
            "SELECT id FROM incident WHERE resolved_at IS NULL ORDER BY id DESC",
            &[],
        );
        assert!(plan.contains("incident_unresolved"), "{plan}");
        assert!(!plan.contains("TEMP B-TREE"), "{plan}");
    }
}
