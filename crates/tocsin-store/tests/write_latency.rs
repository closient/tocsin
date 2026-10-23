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
//!
//! Beside the latencies it prints what the writer and the checkpointer did
//! ([`Store::checkpoint_stats`]), and for each stall, a run of appends over
//! the budget, the slow paths that overlapped it, so a failure names its
//! path (TOC-62).

use std::time::{Duration, Instant};

use rusqlite::Connection;
use tocsin_core::Health;
use tocsin_store::{CheckpointEvent, CheckpointStats, NewCheckResult, Retention, Store};

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

/// One check appending a result every `interval`; returns when each append
/// was submitted and its latency to commit.
async fn run_check(
    store: std::sync::Arc<Store>,
    check: u32,
    interval: Duration,
    run_for: Duration,
) -> Vec<(Instant, Duration)> {
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
        latencies.push((started, started.elapsed()));
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

/// Appends over the budget whose spans overlap, as one stall: when it
/// started and ended, how many appends, and the worst.
struct Stall {
    from: Instant,
    to: Instant,
    appends: usize,
    worst: Duration,
}

fn stalls(samples: &[(Instant, Duration)]) -> Vec<Stall> {
    let mut slow: Vec<_> = samples
        .iter()
        .filter(|(_, took)| *took > P99_BUDGET)
        .map(|&(at, took)| (at, at + took, took))
        .collect();
    slow.sort_unstable_by_key(|&(at, _, _)| at);
    let mut stalls: Vec<Stall> = Vec::new();
    for (from, to, took) in slow {
        match stalls.last_mut() {
            Some(stall) if from <= stall.to => {
                stall.to = stall.to.max(to);
                stall.appends += 1;
                stall.worst = stall.worst.max(took);
            }
            _ => stalls.push(Stall {
                from,
                to,
                appends: 1,
                worst: took,
            }),
        }
    }
    stalls
}

/// When `event` started and ended.
fn span(event: &CheckpointEvent) -> (Instant, Instant) {
    match *event {
        CheckpointEvent::Limit {
            at, waited, copy, ..
        } => (at, at + waited + copy),
        CheckpointEvent::Pass { at, took, .. } | CheckpointEvent::Write { at, took, .. } => {
            (at, at + took)
        }
        CheckpointEvent::Paced { at, waited } => (at, at + waited),
        _ => unreachable!("an event this bench does not know"),
    }
}

fn describe(event: &CheckpointEvent) -> String {
    match *event {
        CheckpointEvent::Limit {
            frames,
            waited,
            copy,
            copied,
            ..
        } => format!(
            "writer at the hard limit, {frames} frames: waited {waited:.2?}, checkpoint {copy:.2?}, {}",
            copied.map_or_else(
                || "busy or failed".to_owned(),
                |copied| format!("{copied} frames in the database after")
            )
        ),
        CheckpointEvent::Pass {
            took,
            chase,
            complete,
            ..
        } => format!(
            "checkpointer pass {took:.2?}{}{}",
            if chase { ", chasing" } else { "" },
            if complete {
                ""
            } else {
                ", a read held part back"
            }
        ),
        CheckpointEvent::Write { took, frames, .. } => {
            format!("write {took:.2?}, log at {frames} frames after")
        }
        CheckpointEvent::Paced { waited, .. } => {
            format!("retention waited {waited:.2?} for the checkpointer")
        }
        _ => unreachable!("an event this bench does not know"),
    }
}

/// The counters, then each stall with the slow paths that overlapped it.
fn paths(stats: &CheckpointStats, stalls: &[Stall], opened: Instant, purged: Instant) -> String {
    use std::fmt::Write;
    let since = |at: Instant| at.saturating_duration_since(opened).as_secs_f64();
    let mut out = format!(
        "| path | count | longest | other |\n|---|---|---|---|\n\
         | writes at the hard limit | {} | | {} waited, longest {:.2?} |\n\
         | the writer's checkpoints there | {} | {:.2?} | the log at up to {} frames |\n\
         | lockouts | {} | | |\n\
         | stand-asides | {} | | |\n\
         | retention writes paced | {} | {:.2?} | |\n\
         | checkpointer passes | {} | {:.2?} | p99 {:.2?} |\n\
         | chase passes | {} | | {} in a row at most |\n\
         | rewinds | {} | | the log at up to {} frames |\n\
         | writes | | {:.2?} | |\n\n\
         The purge finished {:.3} s after the store opened.\n",
        stats.limit_writes,
        stats.limit_waits,
        stats.limit_wait_max,
        stats.inline_copies,
        stats.inline_copy_max,
        stats.inline_frames_max,
        stats.lockouts,
        stats.stand_asides,
        stats.paced_writes,
        stats.paced_wait_max,
        stats.passes,
        stats.pass_max,
        stats.pass_p99,
        stats.chase_passes,
        stats.chase_streak_max,
        stats.rewinds,
        stats.frames_high,
        stats.write_max,
        since(purged),
    );
    if stats.events_dropped > 0 {
        let _ = writeln!(out, "{} later events were not kept.", stats.events_dropped);
    }
    // Each is a stall waiting to happen, so all of them, over budget or not.
    for event in &stats.events {
        if let CheckpointEvent::Limit { .. } = event {
            let (from, to) = span(event);
            let _ = writeln!(
                out,
                "- {:.3}–{:.3} s {}",
                since(from),
                since(to),
                describe(event)
            );
        }
    }
    for stall in stalls {
        let _ = writeln!(
            out,
            "\nStall at {:.3}–{:.3} s: {} appends over {P99_BUDGET:?}, the worst {:.2?}.",
            since(stall.from),
            since(stall.to),
            stall.appends,
            stall.worst,
        );
        let overlapping: Vec<_> = stats
            .events
            .iter()
            .filter(|event| {
                let (from, to) = span(event);
                from <= stall.to && to >= stall.from
            })
            .collect();
        if overlapping.is_empty() {
            let _ = writeln!(out, "- no slow path overlapped it");
        }
        for event in overlapping {
            let (from, to) = span(event);
            let _ = writeln!(
                out,
                "- {:.3}–{:.3} s {}",
                since(from),
                since(to),
                describe(event)
            );
        }
    }
    out
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
    let opened = Instant::now();
    let (samples, retention, (reads, seeded_log, largest_log), stats, purged) =
        runtime.block_on(async {
            let store = std::sync::Arc::new(Store::open(&path).unwrap());
            seed_backlog(&path, tocsin_store::now_ms());

            let purge = {
                let store = std::sync::Arc::clone(&store);
                tokio::spawn(async move {
                    let report = store
                        .apply_retention(Retention::days(7), tocsin_store::now_ms())
                        .await
                        .unwrap();
                    (report, Instant::now())
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
            let mut samples = Vec::new();
            for check in checks {
                samples.extend(check.await.unwrap());
            }
            let (retention, purged) = purge.await.unwrap();
            let reads = reader.await.unwrap();
            let stats = store.checkpoint_stats();
            std::sync::Arc::into_inner(store)
                .unwrap()
                .close()
                .await
                .unwrap();
            (samples, retention, reads, stats, purged)
        });

    let mut sorted: Vec<_> = samples.iter().map(|&(_, took)| took).collect();
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
         | {samples} | {p50:.2?} | {p95:.2?} | {p99:.2?} | {max:.2?} | {P99_BUDGET:?} |\n\n\
         {paths}",
        deleted = retention.deleted,
        chunks = retention.chunks,
        samples = sorted.len(),
        seeded = mib(seeded_log),
        largest = mib(largest_log),
        paths = paths(&stats, &stalls(&samples), opened, purged),
    );
    publish(&summary);
    assert_eq!(
        retention.deleted,
        usize::try_from(BACKLOG_ROWS).unwrap(),
        "the backlog was purged during the run"
    );
    assert!(p99 < P99_BUDGET, "p99 {p99:?} is over {P99_BUDGET:?}");
}
