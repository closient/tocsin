// SPDX-License-Identifier: AGPL-3.0-only

//! Write-latency bench: 100 checks recording results while retention purges
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
//! writes a second) for `TOCSIN_BENCH_SECONDS` (default 30).
//!
//! Most checks record results that change nothing, which the store answers
//! once queued (DESIGN.md, "Durability"); the budget is on what their
//! callers wait. The last [`CHANGING`] checks open or end an outage with
//! every result, each answered once committed. The bench reports those
//! callers' latency, and how long every insert waited for its commit, beside
//! it: the disk's stalls stay visible there though no probe waits them out.
//!
//! It also prints what the writer and the checkpointer did
//! ([`Store::checkpoint_stats`]), and for each stall, a run of waits over the
//! budget, the slow paths that overlapped it, so a stall names its path
//! (TOC-62). Under `scripts/bench/fsync_shim.c` the TRACE lines it prints
//! feed `scripts/bench/sweep.py`.

use std::time::{Duration, Instant};

use rusqlite::Connection;
use tocsin_core::Health;
use tocsin_store::{
    CheckpointEvent, CheckpointStats, NewCheckResult, NewOutage, OutageChange, OutageEnd,
    Retention, RetentionReport, Store, SubjectKind,
};

const CHECKS: u32 = 100;
/// Of [`CHECKS`], the last this many open or end an outage with every result.
const CHANGING: u32 = 10;
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

/// One check recording a result every `interval`; returns when each record
/// was submitted and how long its caller waited. A changing check alternates
/// opening an outage and ending it.
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
    let changing = check >= CHECKS - CHANGING;
    let mut open = None;
    let mut latencies = Vec::new();
    while Instant::now() < deadline {
        ticks.tick().await;
        let mut result = NewCheckResult {
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
        let change = changing.then(|| flip(open.take(), &mut result));
        let started = Instant::now();
        open = store
            .record(result, SubjectKind::Check, change)
            .await
            .unwrap();
        latencies.push((started, started.elapsed()));
    }
    latencies
}

/// Ends the outage `open`, or with none opens one, `result` being down.
fn flip(open: Option<i64>, result: &mut NewCheckResult) -> OutageChange {
    if let Some(id) = open {
        return OutageChange {
            end: Some(OutageEnd {
                id,
                ended_at_ms: result.started_at_ms,
            }),
            open: None,
        };
    }
    result.health = Health::Down;
    OutageChange {
        end: None,
        open: Some(NewOutage {
            node_id: result.node_id.clone(),
            subject_kind: SubjectKind::Check,
            check_id: result.check_id.clone(),
            component_id: None,
            health: Health::Down,
            started_at_ms: result.started_at_ms,
            alerted: false,
        }),
    }
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

/// Waits over the budget whose spans overlap, as one stall: when it started
/// and ended, how many waits, and the worst.
struct Stall {
    from: Instant,
    to: Instant,
    waits: u64,
    worst: Duration,
}

/// Merges `(from, took, waits)` spans over the budget into stalls.
fn stalls(spans: impl IntoIterator<Item = (Instant, Duration, u64)>) -> Vec<Stall> {
    let mut slow: Vec<_> = spans
        .into_iter()
        .filter(|&(_, took, _)| took > P99_BUDGET)
        .collect();
    slow.sort_unstable_by_key(|&(at, _, _)| at);
    let mut stalls: Vec<Stall> = Vec::new();
    for (from, took, waits) in slow {
        let to = from + took;
        match stalls.last_mut() {
            Some(stall) if from <= stall.to => {
                stall.to = stall.to.max(to);
                stall.waits += waits;
                stall.worst = stall.worst.max(took);
            }
            _ => stalls.push(Stall {
                from,
                to,
                waits,
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
        CheckpointEvent::Pass { at, took, .. }
        | CheckpointEvent::Write { at, took, .. }
        | CheckpointEvent::Lag { at, took, .. } => (at, at + took),
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
        CheckpointEvent::Write {
            took,
            frames,
            rewound,
            ..
        } => format!(
            "write {took:.2?}{}, log at {frames} frames after",
            if rewound { ", rewound the log" } else { "" }
        ),
        CheckpointEvent::Paced { waited, .. } => {
            format!("retention waited {waited:.2?} for the checkpointer")
        }
        CheckpointEvent::Lag { took, inserts, .. } => {
            format!("{inserts} inserts committed, the oldest {took:.2?} after it was queued")
        }
        _ => unreachable!("an event this bench does not know"),
    }
}

/// The counters.
fn counters(stats: &CheckpointStats, opened: Instant, purged: Instant) -> String {
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
         | writes | | {:.2?} | |\n\
         | inserts, queueing to commit | {} | {:.2?} | p99 {:.2?}, {} over {P99_BUDGET:?} |\n\
         | probe results refused after their answer | {} | | |\n\n\
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
        stats.inserts,
        stats.lag_max,
        stats.lag_p99,
        stats.lagging,
        stats.rejected,
        since(purged),
    );
    if stats.events_dropped > 0 {
        let _ = writeln!(
            out,
            "\n**{} later events were not kept**, so the stalls below may miss their paths.",
            stats.events_dropped
        );
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
    out
}

/// Each stall of `waits`, with the slow paths that overlapped it; a commit's
/// own lag is not a path.
fn stall_paths(waits: &str, stats: &CheckpointStats, stalls: &[Stall], opened: Instant) -> String {
    use std::fmt::Write;
    let since = |at: Instant| at.saturating_duration_since(opened).as_secs_f64();
    let mut out = String::new();
    for stall in stalls {
        let _ = writeln!(
            out,
            "\n{waits} stall at {:.3}–{:.3} s: {} over {P99_BUDGET:?}, the worst {:.2?}.",
            since(stall.from),
            since(stall.to),
            stall.waits,
            stall.worst,
        );
        let overlapping: Vec<_> = stats
            .events
            .iter()
            .filter(|event| !matches!(event, CheckpointEvent::Lag { .. }))
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

/// The p50, p95, p99 and max of `samples`' latencies, sorted.
fn quantiles(samples: &[(Instant, Duration)]) -> [Duration; 4] {
    let mut sorted: Vec<_> = samples.iter().map(|&(_, took)| took).collect();
    sorted.sort_unstable();
    [
        percentile(&sorted, 0.50),
        percentile(&sorted, 0.95),
        percentile(&sorted, 0.99),
        *sorted.last().unwrap(),
    ]
}

/// One line per wait over 10 ms and per slow path, in milliseconds since
/// the store opened, for `scripts/bench/sweep.py` to set beside the fsync
/// shim's trace of the same run: `append` is a probe result's caller,
/// `change` a changing result's, `event lag` a commit's oldest insert. The
/// `run` line leads: probe samples, then p99 and max of the probes, the
/// changes and the commit lag, then the inserts that lagged over budget.
fn trace(
    probes: &[(Instant, Duration)],
    changes: &[(Instant, Duration)],
    stats: &CheckpointStats,
    opened: Instant,
) {
    let ms = |took: Duration| took.as_secs_f64() * 1e3;
    let since = |at: Instant| ms(at.saturating_duration_since(opened));
    let [_, _, p99, max] = quantiles(probes);
    let [_, _, change_p99, change_max] = quantiles(changes);
    println!(
        "TRACE run {} {:.3} {:.3} {:.3} {:.3} {:.3} {:.3} {}",
        probes.len(),
        ms(p99),
        ms(max),
        ms(change_p99),
        ms(change_max),
        ms(stats.lag_p99),
        ms(stats.lag_max),
        stats.lagging,
    );
    for (series, samples) in [("append", probes), ("change", changes)] {
        for &(at, took) in samples {
            if took > Duration::from_millis(10) {
                println!("TRACE {series} {:.3} {:.3}", since(at), ms(took));
            }
        }
    }
    for event in &stats.events {
        let (from, to) = span(event);
        let kind = match *event {
            CheckpointEvent::Limit { .. } => "limit",
            CheckpointEvent::Pass { .. } => "pass",
            CheckpointEvent::Paced { .. } => "paced",
            CheckpointEvent::Lag { .. } => "lag",
            CheckpointEvent::Write { rewound: true, .. } => "rewind",
            CheckpointEvent::Write { .. } => "write",
            _ => unreachable!("an event this bench does not know"),
        };
        println!("TRACE event {kind} {:.3} {:.3}", since(from), since(to));
    }
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

/// What a run measured.
struct Load {
    /// When each probe result was submitted, and its caller's wait.
    probes: Vec<(Instant, Duration)>,
    /// The same for each changing result.
    changes: Vec<(Instant, Duration)>,
    retention: RetentionReport,
    /// Status queries run, and the log's size after the seed and at most.
    reads: (u32, u64, u64),
    stats: CheckpointStats,
    /// When the purge finished.
    purged: Instant,
}

/// Seeds the backlog, then runs the checks, the purge and the reader.
async fn load(path: &std::path::Path, interval: Duration, run_for: Duration) -> Load {
    let store = std::sync::Arc::new(Store::open(path).unwrap());
    seed_backlog(path, tocsin_store::now_ms());

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
    let (mut probes, mut changes) = (Vec::new(), Vec::new());
    for (check, samples) in (0..CHECKS).zip(checks) {
        let samples = samples.await.unwrap();
        if check >= CHECKS - CHANGING {
            changes.extend(samples);
        } else {
            probes.extend(samples);
        }
    }
    let (retention, purged) = purge.await.unwrap();
    let reads = reader.await.unwrap();
    // Queued behind every probe result, so answered once they have
    // all committed and their lags are counted.
    store
        .append(NewCheckResult {
            node_id: "node-a".into(),
            check_id: "check-0".into(),
            started_at_ms: tocsin_store::now_ms(),
            duration_ms: 42,
            health: Health::Up,
            detail: None,
            error: None,
            status_code: Some(200),
        })
        .await
        .unwrap();
    let stats = store.checkpoint_stats();
    std::sync::Arc::into_inner(store)
        .unwrap()
        .close()
        .await
        .unwrap();
    Load {
        probes,
        changes,
        retention,
        reads,
        stats,
        purged,
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
    // The fsync shim, when loaded, traces this sync's start: the origin its
    // times are read against. /dev/null refuses it, which does not matter.
    let _ = std::fs::File::open("/dev/null").and_then(|null| null.sync_all());
    let Load {
        probes,
        changes,
        retention,
        reads: (reads, seeded_log, largest_log),
        stats,
        purged,
    } = runtime.block_on(load(&path, interval, run_for));

    let [p50, p95, p99, max] = quantiles(&probes);
    let [change_p50, change_p95, change_p99, change_max] = quantiles(&changes);
    let rate = f64::from(CHECKS) / interval.as_secs_f64();
    #[allow(clippy::cast_precision_loss)] // tens of MiB
    let mib = |bytes: u64| bytes as f64 / f64::from(1 << 20);
    let lags = stats.events.iter().filter_map(|event| match *event {
        CheckpointEvent::Lag { at, took, inserts } => Some((at, took, inserts)),
        _ => None,
    });
    let probe_spans = probes.iter().map(|&(at, took)| (at, took, 1));
    let change_spans = changes.iter().map(|&(at, took)| (at, took, 1));
    let summary = format!(
        "### tocsin-store write latency\n\n\
         {CHECKS} checks every {interval:?} ({rate:.0} writes/s, {speedup}x the 10 s target) for {seconds} s, \
         while retention purged {deleted} backlog rows in {chunks} chunks and a reader ran {reads} status queries. \
         {CHANGING} of the checks open or end an outage with every result. \
         The log file held {seeded:.1} MiB after the seed and at most {largest:.1} MiB during the run.\n\n\
         | what the caller waits for | samples | p50 | p95 | p99 | max | p99 budget |\n\
         |---|---|---|---|---|---|---|\n\
         | a probe result, queued | {samples} | {p50:.2?} | {p95:.2?} | {p99:.2?} | {max:.2?} | {P99_BUDGET:?} |\n\
         | a change, committed | {change_samples} | {change_p50:.2?} | {change_p95:.2?} | {change_p99:.2?} | {change_max:.2?} | report only |\n\
         | every insert, queueing to commit | {inserts} | | | {lag_p99:.2?} | {lag_max:.2?} | report only, {lagging} over {P99_BUDGET:?} |\n\n\
         {counters}{probe_stalls}{change_stalls}{lag_stalls}",
        deleted = retention.deleted,
        chunks = retention.chunks,
        samples = probes.len(),
        change_samples = changes.len(),
        inserts = stats.inserts,
        lag_p99 = stats.lag_p99,
        lag_max = stats.lag_max,
        lagging = stats.lagging,
        seeded = mib(seeded_log),
        largest = mib(largest_log),
        counters = counters(&stats, opened, purged),
        probe_stalls = stall_paths("Probe result", &stats, &stalls(probe_spans), opened),
        change_stalls = stall_paths("Change", &stats, &stalls(change_spans), opened),
        lag_stalls = stall_paths("Commit", &stats, &stalls(lags), opened),
    );
    publish(&summary);
    trace(&probes, &changes, &stats, opened);
    assert_eq!(
        retention.deleted,
        usize::try_from(BACKLOG_ROWS).unwrap(),
        "the backlog was purged during the run"
    );
    assert_eq!(stats.rejected, 0, "a probe result was refused");
    assert!(p99 < P99_BUDGET, "p99 {p99:?} is over {P99_BUDGET:?}");
}
