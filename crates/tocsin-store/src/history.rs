// SPDX-License-Identifier: AGPL-3.0-only

//! Reading history back for the API: one check's raw results and the
//! outages in a window, a page at a time, newest first.

use rusqlite::{Connection, OptionalExtension, named_params};
use tocsin_core::incident::{FieldError, Timestamp};
use tocsin_core::status::{CheckKind, CheckResult, OutageRecord};

use crate::alert::SubjectKind;
use crate::error::{Error, Result};
use crate::rollup::parse_column;

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
        |row| {
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
        },
    )?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The error for a `before` that names no row this read can resume from.
fn lost_cursor(before: i64, what: &str) -> Error {
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
    Ok(match parse_column(index, &row.get::<_, String>(index)?)? {
        SubjectKind::Check => CheckKind::Check,
        SubjectKind::Heartbeat => CheckKind::Heartbeat,
    })
}

/// Outages open at any moment at or after `page.since`, in three arms
/// that each ride an index: started since (`outage_by_start`, in order, so
/// a page reads only its rows); started before and still open
/// (`outage_open`); started before and ended since (`outage_ended`). The
/// last two are what a window catches mid-way, few enough to sort. One
/// arm with `ended_at IS NULL OR ended_at >= :since` would walk every
/// older outage through `outage_by_start` instead, hence `INDEXED BY`.
/// The cursor bound is the same as [`RESULTS`]'s, in every arm.
const OUTAGES: &str =
    "SELECT id, node_id, check_id, component_id, health, started_at, ended_at, subject_kind
     FROM outage INDEXED BY outage_by_start
     WHERE started_at >= :since
       AND started_at <= :at AND (started_at < :at OR id < :before)
     UNION ALL
     SELECT id, node_id, check_id, component_id, health, started_at, ended_at, subject_kind
     FROM outage INDEXED BY outage_open
     WHERE ended_at IS NULL AND started_at < :since
       AND started_at <= :at AND (started_at < :at OR id < :before)
     UNION ALL
     SELECT id, node_id, check_id, component_id, health, started_at, ended_at, subject_kind
     FROM outage INDEXED BY outage_ended
     WHERE ended_at >= :since AND started_at < :since
       AND started_at <= :at AND (started_at < :at OR id < :before)
     ORDER BY started_at DESC, id DESC LIMIT :limit";

/// Outages open at any moment at or after `page.since`: started since,
/// or started before and ended since or not at all. Newest start first,
/// ties by id.
///
/// # Errors
///
/// [`Error::Invalid`] when `page.before` names no outage.
pub(crate) fn outages(conn: &Connection, page: HistoryPage) -> Result<Vec<OutageRecord>> {
    let (at, before) = match page.before {
        None => (i64::MAX, i64::MAX),
        Some(before) => {
            let at = conn
                .prepare_cached("SELECT started_at FROM outage WHERE id = ?1")?
                .query_row([before], |row| row.get(0))
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
        },
        |row| {
            Ok(OutageRecord {
                id: row.get(0)?,
                node_id: row.get(1)?,
                check_id: row.get(2)?,
                component_id: row.get(3)?,
                health: parse_column(4, &row.get::<_, String>(4)?)?,
                started_at: Timestamp::from_millis(row.get(5)?),
                ended_at: row.get::<_, Option<i64>>(6)?.map(Timestamp::from_millis),
                subject_kind: subject_kind(7, row)?,
            })
        },
    )?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[cfg(test)]
mod tests {
    use rusqlite::{Connection, named_params};

    use super::{OUTAGES, RESULTS};

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
