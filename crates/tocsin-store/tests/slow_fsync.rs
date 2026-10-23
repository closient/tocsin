// SPDX-License-Identifier: AGPL-3.0-only

//! A disk whose log syncs are slow, on demand (TOC-66).
//!
//! The first commit after a checkpoint has copied the whole log rewinds it,
//! and SQLite syncs the log's new header inside that commit, holding the
//! write lock. On a busy disk that sync took up to 442 ms on CI. A probe
//! result that confirms no change is acknowledged once queued, so no such
//! sync reaches its caller; every other write still waits for its commit
//! (DESIGN.md, "Durability").
//!
//! The test builds `scripts/bench/fsync_shim.c`, then runs itself again
//! under it with every sync of the log delayed by [`DELAY`].

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use tocsin_core::Health;
use tocsin_store::{NewCheckResult, NewOutage, OutageChange, Store, SubjectKind};

/// Each sync of the log takes at least this long under the shim.
const DELAY: Duration = Duration::from_millis(300);

/// The write-latency bench's p99 budget.
const BUDGET: Duration = Duration::from_millis(50);

/// Set in the run under the shim.
const UNDER_SHIM: &str = "TOCSIN_TEST_UNDER_FSYNC_SHIM";

/// Compiles the shim into the target directory.
fn build_shim() -> PathBuf {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/bench/fsync_shim.c");
    let library = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("fsync_shim.so");
    let status = Command::new("cc")
        .args(["-shared", "-fPIC", "-O2", "-o"])
        .arg(&library)
        .arg(&source)
        .args(["-ldl", "-lpthread"])
        .status()
        .expect("a C compiler, cc, to build the fsync shim");
    assert!(status.success(), "cc could not build {}", source.display());
    library
}

fn result(check: &str) -> NewCheckResult {
    NewCheckResult {
        node_id: "node-a".into(),
        check_id: check.into(),
        started_at_ms: tocsin_store::now_ms(),
        duration_ms: 42,
        health: Health::Up,
        detail: None,
        error: None,
        status_code: Some(200),
    }
}

/// Copies the whole log into the database from a second connection, so the
/// store's next commit rewinds the log and syncs its header.
fn copy_whole_log(path: &std::path::Path) {
    let conn = Connection::open(path).unwrap();
    let (busy, log, copied): (i64, i64, i64) = conn
        .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap();
    assert_eq!(
        (busy, log),
        (0, copied),
        "the checkpoint copied the whole log"
    );
    assert!(log > 0, "the log had frames to copy");
}

#[test]
fn a_slow_header_sync_stays_off_probe_results() {
    if std::env::var_os(UNDER_SHIM).is_none() {
        let shim = build_shim();
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "a_slow_header_sync_stays_off_probe_results",
                "--nocapture",
            ])
            .env("LD_PRELOAD", &shim)
            .env("TOCSIN_FSYNC_DELAY_MS", DELAY.as_millis().to_string())
            .env(UNDER_SHIM, "1")
            .status()
            .unwrap();
        assert!(status.success(), "the run under the shim failed");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    // The shim is loaded: a sync of a file named like a log is slow.
    let probe = dir.path().join("probe-wal");
    let file = std::fs::File::create(&probe).unwrap();
    let started = Instant::now();
    file.sync_all().unwrap();
    assert!(
        started.elapsed() >= DELAY,
        "the shim did not delay the sync"
    );

    let path = dir.path().join(tocsin_store::DB_FILE);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let store = Store::open(&path).unwrap();
        store.append(result("web")).await.unwrap();

        // The control: a result that opens an outage waits for its commit,
        // which rewinds the log and syncs the header.
        copy_whole_log(&path);
        let rewinds = store.checkpoint_stats().rewinds;
        let started = Instant::now();
        let mut down = result("web");
        down.health = Health::Down;
        let opened = store
            .record(
                down,
                SubjectKind::Check,
                Some(OutageChange {
                    end: None,
                    open: Some(NewOutage {
                        node_id: "node-a".into(),
                        subject_kind: SubjectKind::Check,
                        check_id: "web".into(),
                        component_id: None,
                        health: Health::Down,
                        started_at_ms: tocsin_store::now_ms(),
                        alerted: true,
                    }),
                }),
            )
            .await
            .unwrap();
        let control = started.elapsed();
        assert!(opened.is_some());
        assert_eq!(store.checkpoint_stats().rewinds, rewinds + 1, "it rewound");
        assert!(
            control >= DELAY,
            "a commit that rewound took {control:?}, under the {DELAY:?} sync"
        );

        // Probe results that change nothing, one every 10 ms, while the
        // first of their commits rewinds the log again.
        copy_whole_log(&path);
        let rewinds = store.checkpoint_stats().rewinds;
        let mut slowest = Duration::ZERO;
        for _ in 0..30 {
            let started = Instant::now();
            store
                .record(result("api"), SubjectKind::Check, None)
                .await
                .unwrap();
            slowest = slowest.max(started.elapsed());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Behind every one of them in the queue, so after their commits.
        store.append(result("web")).await.unwrap();
        assert_eq!(store.checkpoint_stats().rewinds, rewinds + 1, "it rewound");
        let stored: i64 = store
            .read(|conn| {
                Ok(conn.query_row(
                    "SELECT count(*) FROM check_result WHERE check_id = 'api'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(stored, 30, "every result was committed");
        assert!(
            slowest < BUDGET,
            "a probe result waited {slowest:?} while the log's header synced"
        );
        store.close().await.unwrap();
    });
}
