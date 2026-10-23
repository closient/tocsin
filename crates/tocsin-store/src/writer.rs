// SPDX-License-Identifier: AGPL-3.0-only

//! The single writer: one thread owns the only write connection and takes
//! work from a channel, so no two writes ever contend for SQLite's lock.
//!
//! A panic in a write is caught, the open transaction (if any) is rolled
//! back, the waiting callers get [`Error::WriterPanicked`] and the thread
//! carries on: a bug in one write must not silently stop every later one.

use std::panic::{self, AssertUnwindSafe};
use std::thread::JoinHandle;

use rusqlite::{Connection, TransactionBehavior, params};
use tokio::sync::{mpsc, oneshot};

use crate::error::{Error, Result};
use crate::{NewCheckResult, NewOutage};

/// Queued writes beyond this make callers wait (backpressure).
const QUEUE_DEPTH: usize = 1024;

/// Most inserts committed in one transaction.
const MAX_BATCH: usize = 256;

/// A small row write. Consecutive queued inserts share one transaction.
pub(crate) enum Insert {
    CheckResult(NewCheckResult),
    OpenOutage(NewOutage),
    EndOutage { id: i64, ended_at_ms: i64 },
}

/// Any other write; it runs alone and replies through its own channel.
type Job = Box<dyn FnOnce(&mut Connection) + Send>;

enum Command {
    Insert(Insert, oneshot::Sender<Result<i64>>),
    Run(Job),
}

pub(crate) struct Writer {
    sender: mpsc::Sender<Command>,
    thread: JoinHandle<()>,
}

impl Writer {
    pub(crate) fn spawn(conn: Connection) -> Result<Writer> {
        let (sender, receiver) = mpsc::channel(QUEUE_DEPTH);
        let thread = std::thread::Builder::new()
            .name("tocsin-store-writer".into())
            .spawn(move || run(conn, receiver))?;
        Ok(Writer { sender, thread })
    }

    /// Queues an insert; resolves with the new (or, for `EndOutage`, the
    /// updated) row id once its transaction has committed.
    pub(crate) async fn insert(&self, insert: Insert) -> Result<i64> {
        let (reply, receive) = oneshot::channel();
        self.sender
            .send(Command::Insert(insert, reply))
            .await
            .map_err(|_| Error::Closed)?;
        receive.await.map_err(|_| Error::Closed)?
    }

    /// Runs `job` on the write connection, after every write queued before it.
    pub(crate) async fn run<T, F>(&self, job: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let (reply, receive) = oneshot::channel();
        let job: Job = Box::new(move |conn| {
            let outcome = guarded(conn, |conn| job(conn));
            // The caller may have stopped waiting; the write still happened.
            let _ = reply.send(outcome);
        });
        self.sender
            .send(Command::Run(job))
            .await
            .map_err(|_| Error::Closed)?;
        receive.await.map_err(|_| Error::Closed)?
    }

    /// Stops taking work, lets the queue drain and returns the thread, which
    /// finishes once the connection is closed (checkpointing the WAL).
    pub(crate) fn close(self) -> JoinHandle<()> {
        drop(self.sender);
        self.thread
    }
}

fn run(mut conn: Connection, mut receiver: mpsc::Receiver<Command>) {
    let mut next = None;
    loop {
        let Some(command) = next.take().or_else(|| receiver.blocking_recv()) else {
            return;
        };
        match command {
            Command::Run(job) => job(&mut conn),
            Command::Insert(insert, reply) => {
                let mut batch = vec![(insert, reply)];
                while batch.len() < MAX_BATCH {
                    match receiver.try_recv() {
                        Ok(Command::Insert(insert, reply)) => batch.push((insert, reply)),
                        Ok(other) => {
                            next = Some(other);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                let (inserts, replies): (Vec<_>, Vec<_>) = batch.into_iter().unzip();
                let outcomes = commit_batch(&mut conn, &inserts);
                for (reply, outcome) in replies.into_iter().zip(outcomes) {
                    let _ = reply.send(outcome);
                }
            }
        }
    }
}

/// Runs `write`, turning a panic into [`Error::WriterPanicked`] after
/// rolling back whatever transaction it left open.
fn guarded<T>(
    conn: &mut Connection,
    write: impl FnOnce(&mut Connection) -> Result<T>,
) -> Result<T> {
    let Ok(outcome) = panic::catch_unwind(AssertUnwindSafe(|| write(conn))) else {
        let rolled_back = conn.is_autocommit() || conn.execute_batch("ROLLBACK").is_ok();
        tracing::error!(
            rolled_back,
            "a database write panicked; its callers get an error and the writer carries on"
        );
        return Err(Error::WriterPanicked);
    };
    outcome
}

/// Applies each insert under its own savepoint, so one rejected row does not
/// sink the others, then commits once. Returns one outcome per insert, each
/// true of what is in the database afterwards.
fn commit_batch(conn: &mut Connection, inserts: &[Insert]) -> Vec<Result<i64>> {
    let committed = guarded(conn, |conn| Ok(commit_inserts(conn, inserts)?));
    match committed {
        Ok(outcomes) => outcomes
            .into_iter()
            .map(|outcome| outcome.map_err(Error::from))
            .collect(),
        Err(Error::WriterPanicked) => inserts.iter().map(|_| Err(Error::WriterPanicked)).collect(),
        Err(error) => {
            let message = error.to_string();
            inserts
                .iter()
                .map(|_| Err(Error::Batch(message.clone())))
                .collect()
        }
    }
}

fn commit_inserts(
    conn: &mut Connection,
    inserts: &[Insert],
) -> rusqlite::Result<Vec<rusqlite::Result<i64>>> {
    let mut transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut outcomes = Vec::with_capacity(inserts.len());
    for insert in inserts {
        let savepoint = transaction.savepoint()?;
        let outcome = apply(&savepoint, insert);
        match &outcome {
            Ok(_) => savepoint.commit()?,
            // Rolls back to the savepoint, undoing only this insert.
            Err(_) => drop(savepoint),
        }
        // Some failures (SQLITE_FULL, IOERR, NOMEM, a RAISE(ROLLBACK))
        // roll back the whole transaction, not just the savepoint. Carrying
        // on would open a fresh implicit transaction and report outcomes
        // that do not match what committed, so the whole batch fails.
        if transaction.is_autocommit() {
            return Err(outcome.err().unwrap_or_else(|| {
                rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ABORT),
                    Some("the batch's transaction was rolled back".into()),
                )
            }));
        }
        outcomes.push(outcome);
    }
    transaction.commit()?;
    Ok(outcomes)
}

fn apply(conn: &Connection, insert: &Insert) -> rusqlite::Result<i64> {
    match insert {
        Insert::CheckResult(result) => {
            let detail = result.detail.as_ref().map(ToString::to_string);
            conn.prepare_cached(
                "INSERT INTO check_result
                     (node_id, check_id, started_at, duration_ms, health, detail, error, status_code)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?
            .execute(params![
                result.node_id,
                result.check_id,
                result.started_at_ms,
                result.duration_ms,
                result.health.as_str(),
                detail,
                result.error,
                result.status_code,
            ])?;
            Ok(conn.last_insert_rowid())
        }
        Insert::OpenOutage(outage) => {
            conn.prepare_cached(
                "INSERT INTO outage (node_id, check_id, component_id, health, started_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?
            .execute(params![
                outage.node_id,
                outage.check_id,
                outage.component_id,
                outage.health.as_str(),
                outage.started_at_ms,
            ])?;
            Ok(conn.last_insert_rowid())
        }
        Insert::EndOutage { id, ended_at_ms } => {
            let changed = conn
                .prepare_cached("UPDATE outage SET ended_at = ?2 WHERE id = ?1")?
                .execute(params![id, ended_at_ms])?;
            if changed == 0 {
                return Err(rusqlite::Error::QueryReturnedNoRows);
            }
            Ok(*id)
        }
    }
}
