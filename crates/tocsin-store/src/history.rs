// SPDX-License-Identifier: AGPL-3.0-only

//! Reading history back for the API: one check's raw results and the
//! outages in a window, a page at a time, newest first.

use rusqlite::{Connection, named_params};
use tocsin_core::incident::Timestamp;
use tocsin_core::status::{CheckKind, CheckResult, OutageRecord};

use crate::alert::SubjectKind;
use crate::error::Result;
use crate::rollup::parse_column;

/// Which page of a history read: newest first from `before`, back to
/// `since`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryPage {
    /// The start of the window.
    pub since: Timestamp,
    /// Only rows after this one in newest-first order: the previous page's
    /// last id. An id no page handed out matches nothing.
    pub before: Option<i64>,
    /// At most this many rows.
    pub limit: u32,
}

const RESULTS: &str =
    "SELECT id, node_id, started_at, duration_ms, health, error, status_code, detail
     FROM check_result
     WHERE check_id = :check AND started_at >= :since
       AND (:before IS NULL OR (started_at, id) <
            (SELECT started_at, id FROM check_result WHERE id = :before))
     ORDER BY started_at DESC, id DESC LIMIT :limit";

/// `check_id`'s results that started at or after `page.since`, newest
/// first, ties by id. Rides `check_result_by_check`.
pub(crate) fn results(
    conn: &Connection,
    check_id: &str,
    page: HistoryPage,
) -> Result<Vec<CheckResult>> {
    let mut statement = conn.prepare_cached(RESULTS)?;
    let rows = statement.query_map(
        named_params! {
            ":check": check_id,
            ":since": page.since.as_millis(),
            ":before": page.before,
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

/// Outages open at any moment at or after `page.since`: started since,
/// or started before and ended since or not at all. Newest start first,
/// ties by id.
pub(crate) fn outages(conn: &Connection, page: HistoryPage) -> Result<Vec<OutageRecord>> {
    let mut statement = conn.prepare_cached(
        "SELECT id, node_id, check_id, component_id, health, started_at, ended_at, subject_kind
         FROM outage
         WHERE (ended_at IS NULL OR ended_at >= :since)
           AND (:before IS NULL OR (started_at, id) <
                (SELECT started_at, id FROM outage WHERE id = :before))
         ORDER BY started_at DESC, id DESC LIMIT :limit",
    )?;
    let rows = statement.query_map(
        named_params! {
            ":since": page.since.as_millis(),
            ":before": page.before,
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
                kind: match parse_column(7, &row.get::<_, String>(7)?)? {
                    SubjectKind::Check => CheckKind::Check,
                    SubjectKind::Heartbeat => CheckKind::Heartbeat,
                },
            })
        },
    )?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[cfg(test)]
mod tests {
    use rusqlite::{Connection, named_params};

    use super::RESULTS;

    #[tokio::test]
    async fn a_results_page_reads_the_index_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(crate::DB_FILE);
        crate::Store::open(&path).unwrap().close().await.unwrap();
        let conn = Connection::open(&path).unwrap();
        let mut statement = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {RESULTS}"))
            .unwrap();
        let plan: Vec<String> = statement
            .query_map(
                named_params! {
                    ":check": "api", ":since": 0, ":before": 7, ":limit": 101,
                },
                |row| row.get(3),
            )
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let plan = plan.join(
            "
",
        );
        assert!(plan.contains("check_result_by_check"), "{plan}");
        assert!(
            !plan.contains("TEMP B-TREE"),
            "a page sorts no rows: {plan}"
        );
    }
}
