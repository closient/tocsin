// SPDX-License-Identifier: AGPL-3.0-only

//! Write-latency bench: 100 checks appending results while retention purges
//! a backlog and a reader polls the status query.
//!
//! Ignored by default; CI runs it pinned to one core and fails the build if
//! the p99 is over [`P99_BUDGET`]:
//!
//! ```text
//! taskset -c 0 cargo test --release -p tocsin-store --test write_latency -- --ignored --nocapture
//! ```
//!
//! The ticket's load is 100 checks every 10 s, which is 10 writes a second.
//! The bench compresses time by `TOCSIN_BENCH_SPEEDUP` (default 10, so 100
//! writes a second) for `TOCSIN_BENCH_SECONDS` (default 30), measuring each
//! append from submission to commit.

use std::time::{Duration, Instant};

use rusqlite::Connection;
use tocsin_core::Health;
use tocsin_store::{NewCheckResult, Retention, Store};

const CHECKS: u32 = 100;
const INTERVAL: Duration = Duration::from_secs(10);
/// Rows older than the retention window, purged while the bench runs.
const BACKLOG_ROWS: i64 = 300_000;
const P99_BUDGET: Duration = Duration::from_millis(50);

fn env_u32(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// Fills `check_result` with a backlog 30 to 31 days old.
fn seed_backlog(path: &std::path::Path, now_ms: i64) {
    let conn = Connection::open(path).unwrap();
    let from = now_ms - 31 * 86_400_000;
    conn.execute(
        "WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i < ?1 - 1)
         INSERT INTO check_result (node_id, check_id, started_at, duration_ms, health)
         SELECT 'node-a', 'check-' || (i % 100), ?2 + i * 250, 5,
                CASE i % 4 WHEN 2 THEN 'degraded' WHEN 3 THEN 'down' ELSE 'up' END
         FROM n",
        rusqlite::params![BACKLOG_ROWS, from],
    )
    .unwrap();
}

fn percentile(sorted: &[Duration], fraction: f64) -> Duration {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let index = ((sorted.len() as f64 * fraction).ceil() as usize).clamp(1, sorted.len()) - 1;
    sorted[index]
}

/// One check appending a result every `interval`; returns each append's
/// latency from submission to commit.
async fn run_check(
    store: std::sync::Arc<Store>,
    check: u32,
    interval: Duration,
    run_for: Duration,
) -> Vec<Duration> {
    // Spread the checks across the interval, as a scheduler would.
    tokio::time::sleep(interval * check / CHECKS).await;
    let mut ticks = tokio::time::interval(interval);
    let deadline = Instant::now() + run_for;
    let mut latencies = Vec::new();
    while Instant::now() < deadline {
        ticks.tick().await;
        let result = NewCheckResult {
            node_id: "node-a".into(),
            check_id: format!("check-{check}"),
            started_at_ms: tocsin_store::now_ms(),
            duration_ms: 42,
            health: Health::Up,
            detail: Some(serde_json::json!({
                "status": { "expected": 200, "got": 200 },
                "body_contains": { "expected": "ok", "passed": true },
            })),
            error: None,
            status_code: Some(200),
        };
        let started = Instant::now();
        store.append(result).await.unwrap();
        latencies.push(started.elapsed());
    }
    latencies
}

/// Runs the status page's query (each check's latest result) every 100 ms;
/// returns how many ran, the log file's size before the first (the seed's
/// one large transaction) and the largest it was after one.
async fn poll_status(
    store: std::sync::Arc<Store>,
    log: std::path::PathBuf,
    run_for: Duration,
) -> (u32, u64, u64) {
    let deadline = Instant::now() + run_for;
    let mut reads = 0;
    let seeded = log_bytes(&log);
    let mut largest = 0;
    while Instant::now() < deadline {
        store
            .read(|conn| {
                Ok(conn
                    .prepare_cached(
                        "SELECT check_id, health, max(started_at) FROM check_result
                         WHERE started_at > ?1 GROUP BY check_id",
                    )?
                    .query_map([tocsin_store::now_ms() - 60_000], |_| Ok(()))?
                    .count())
            })
            .await
            .unwrap();
        reads += 1;
        largest = largest.max(log_bytes(&log));
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    (reads, seeded, largest)
}

fn log_bytes(log: &std::path::Path) -> u64 {
    std::fs::metadata(log).map_or(0, |metadata| metadata.len())
}

/// Prints the summary and appends it to the GitHub job summary when set.
fn publish(summary: &str) {
    use std::io::Write;
    println!("{summary}");
    if let Ok(file) = std::env::var("GITHUB_STEP_SUMMARY") {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(file)
            .unwrap();
        writeln!(file, "{summary}").unwrap();
    }
}

#[test]
#[ignore = "bench; run with --ignored --release"]
fn p99_append_latency_under_load() {
    let speedup = env_u32("TOCSIN_BENCH_SPEEDUP", 10).max(1);
    let seconds = env_u32("TOCSIN_BENCH_SECONDS", 30).max(1);
    let interval = INTERVAL / speedup;
    let run_for = Duration::from_secs(u64::from(seconds));

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(tocsin_store::DB_FILE);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (latencies, retention, (reads, seeded_log, largest_log)) = runtime.block_on(async {
        let store = std::sync::Arc::new(Store::open(&path).unwrap());
        seed_backlog(&path, tocsin_store::now_ms());

        let purge = {
            let store = std::sync::Arc::clone(&store);
            tokio::spawn(async move {
                store
                    .apply_retention(Retention::days(7), tocsin_store::now_ms())
                    .await
                    .unwrap()
            })
        };
        let mut log = path.as_os_str().to_owned();
        log.push("-wal");
        let reader = tokio::spawn(poll_status(
            std::sync::Arc::clone(&store),
            log.into(),
            run_for,
        ));

        let mut checks = Vec::new();
        for check in 0..CHECKS {
            checks.push(tokio::spawn(run_check(
                std::sync::Arc::clone(&store),
                check,
                interval,
                run_for,
            )));
        }
        let mut latencies = Vec::new();
        for check in checks {
            latencies.extend(check.await.unwrap());
        }
        let retention = purge.await.unwrap();
        let reads = reader.await.unwrap();
        std::sync::Arc::into_inner(store)
            .unwrap()
            .close()
            .await
            .unwrap();
        (latencies, retention, reads)
    });

    let mut sorted = latencies;
    sorted.sort_unstable();
    let (p50, p95, p99) = (
        percentile(&sorted, 0.50),
        percentile(&sorted, 0.95),
        percentile(&sorted, 0.99),
    );
    let max = *sorted.last().unwrap();
    let rate = f64::from(CHECKS) / interval.as_secs_f64();
    #[allow(clippy::cast_precision_loss)] // tens of MiB
    let mib = |bytes: u64| bytes as f64 / f64::from(1 << 20);
    let summary = format!(
        "### tocsin-store write latency\n\n\
         {CHECKS} checks every {interval:?} ({rate:.0} writes/s, {speedup}x the 10 s target) for {seconds} s, \
         while retention purged {deleted} backlog rows in {chunks} chunks and a reader ran {reads} status queries. \
         The log file held {seeded:.1} MiB after the seed and at most {largest:.1} MiB during the run.\n\n\
         | samples | p50 | p95 | p99 | max | p99 budget |\n\
         |---|---|---|---|---|---|\n\
         | {samples} | {p50:.2?} | {p95:.2?} | {p99:.2?} | {max:.2?} | {P99_BUDGET:?} |\n",
        deleted = retention.deleted,
        chunks = retention.chunks,
        samples = sorted.len(),
        seeded = mib(seeded_log),
        largest = mib(largest_log),
    );
    publish(&summary);
    assert_eq!(
        retention.deleted,
        usize::try_from(BACKLOG_ROWS).unwrap(),
        "the backlog was purged during the run"
    );
    assert!(p99 < P99_BUDGET, "p99 {p99:?} is over {P99_BUDGET:?}");
}
